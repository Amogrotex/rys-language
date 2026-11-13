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
