use std::fs;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

fn run_source(source: &str) -> Output {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rys-test-{}-{}.rys", std::process::id(), id));
    fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rys"))
        .env("RYS_NO_CACHE", "1")
        .arg(&path)
        .output().unwrap();
    fs::remove_file(path).unwrap();
    output
}

#[test]
fn string_helpers_and_trailing_commas() {
    let output = run_source(r#"def greet(name,):
    return upper(trim(name,))
print(greet("  Rys  ",),)
print(lower("HELLO"))
print(contains("RYS language", "language"))
print(join(split("a,b,c", ","), "-"))
print(reverse("abc"))
print(reverse([1, 2, 3,]))
"#);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "RYS\nhello\ntrue\na-b-c\ncba\n[3, 2, 1]\n");
}

#[test]
fn reverse_does_not_mutate_original_array() {
    let output = run_source("a = [1, 2]\nb = reverse(a)\nprint(a)\nprint(b)\n");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "[1, 2]\n[2, 1]\n");
}

#[test]
fn invalid_builtin_arguments_report_errors() {
    for source in ["split(\"abc\", \"\")", "join([1], \",\")", "upper(2)", "reverse()"] {
        let output = run_source(source);
        assert!(!output.status.success(), "expected error for {source}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("line 1"));
    }
}

#[test]
fn help_is_available() {
    let output = Command::new(env!("CARGO_BIN_EXE_rys")).arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: rys"));
}

#[test]
fn string_indexing_handles_unicode_negative_and_bounds() {
    let output = run_source("s = \"aé猫🙂\"\nprint(s[0])\nprint(s[1])\nprint(s[-1])\nprint(s[-4])\nprint(\"abc\"[-2])\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "a\né\n🙂\na\nb\n");
    for source in ["print(\"é猫\"[2])", "print(\"é猫\"[-3])", "print(\"abc\"[3])", "print(\"\"[0])", "print(\"é\"[9223372036854775807])"] {
        let output = run_source(source);
        assert!(!output.status.success(), "expected error for {source}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("out of range"));
    }
}

#[test]
fn builtin_argument_buffer_does_not_leak_between_calls() {
    let output = run_source("a = [1]\npush(a, 2)\nprint(len(a))\nprint(pop(a))\nprint(len(a))\nprint(join([\"x\", \"y\"], \"-\"))\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2\n2\n1\nx-y\n");
}

#[test]
fn bytecode_cache_matches_uncached_execution() {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rys-cache-test-{}-{}", std::process::id(), id));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("program.rys");
    fs::write(&path, "print(upper(\"hello\"))\nprint(\"é猫\"[-1])\n").unwrap();
    let run = |disabled: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rys"));
        command.env("RYS_CACHE_DIR", &dir).arg(&path);
        if disabled { command.env("RYS_NO_CACHE", "1"); }
        else { command.env_remove("RYS_NO_CACHE"); }
        command.output().unwrap()
    };
    let cold = run(false);
    let warm = run(false);
    let uncached = run(true);
    assert!(cold.status.success() && warm.status.success() && uncached.status.success());
    assert_eq!(cold.stdout, warm.stdout);
    assert_eq!(warm.stdout, uncached.stdout);
    assert!(fs::read_dir(&dir).unwrap().any(|entry| entry.unwrap().path().extension().is_some_and(|ext| ext == "rysc")));
    fs::remove_dir_all(dir).unwrap();
}
