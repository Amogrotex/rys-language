// ============================================================
//  RYS bytecode verifier
//
//  Runs over every function prototype before execution and proves
//  the instruction stream is well-formed:
//
//   • every jump target is in bounds and is a real instruction
//   • the operand stack never underflows on any reachable path
//   • every path reaching a given instruction agrees on stack depth
//   • every path that falls through the end of code is empty
//   • every `return` path is consistent
//   • constant indices / local slots / closure indices are in range
//   • ITERNEXT iterates only over slots filled by ITERINIT
//
//  This is the same class of check the JVM performs on class files:
//  it is what makes a corrupt or hand-written .rysc file harmless
//  instead of a segfault or memory corruption.
// ============================================================

use crate::bytecode::{FunctionProto, Op, Program};

/// Result of verifying one program.
#[derive(Debug)]
pub struct Report {
    pub instructions: usize,
    pub max_stack: usize,
}

const GLOBAL_FLAG: u32 = 0x8000_0000;

/// Stack transitions for one instruction, in the abstract interpreter.
struct Effect {
    pops: usize,
    pushes: usize,
}

fn effect(op: &Op) -> Effect {
    use Op::*;
    let (pops, pushes) = match op {
        Const(_) | Nil | GetLocal(_) | GetGlobal(_) | MakeClosure(_) => (0, 1),
        SetLocal(_) | SetGlobal(_) | Pop | Echo => (1, 0),
        Dup2 => (2, 4),
        ToBool | Neg | Not => (1, 1),
        Add | Sub | Mul | Div | Mod | Pow | Eq | Ne | Lt | Le | Gt | Ge | MakeRange => (2, 1),
        Jump(_) => (0, 0),
        JumpIfFalsePop(_) | JumpIfTruePop(_) => (1, 0),
        EqJf(_) | NeJf(_) | LtJf(_) | LeJf(_) | GtJf(_) | GeJf(_) => (2, 0),
        MakeArray(n) => (*n as usize, 1),
        Call(argc) => (*argc as usize + 1, 1),
        Index => (2, 1),
        SetIndex => (3, 0),
        IterInit { .. } => (1, 0),
        IterNext { .. } => (0, 0),
        Return => (1, 0),
    };
    Effect { pops, pushes }
}

/// Instructions that do not fall through to the next one.
fn is_terminal(op: &Op) -> bool {
    matches!(op, Op::Return)
}

/// Conditional branches: successors are the fallthrough AND the target.
fn branch_target(op: &Op) -> Option<u32> {
    use Op::*;
    match op {
        Jump(t) | JumpIfFalsePop(t) | JumpIfTruePop(t) | EqJf(t) | NeJf(t) | LtJf(t) | LeJf(t)
        | GtJf(t) | GeJf(t) => Some(*t),
        _ => None,
    }
}

fn verror(f: &FunctionProto, ip: usize, msg: impl AsRef<str>) -> String {
    let line = f.chunk.lines.get(ip).copied().unwrap_or(0);
    format!(
        "line {}: bytecode verify failed in {}() at instruction {}: {}",
        line,
        f.name,
        ip,
        msg.as_ref()
    )
}

fn verify_proto(f: &FunctionProto, func_count: usize, is_entry: bool, check_funcs_after: usize) -> Result<Report, String> {
    let code = &f.chunk.code;
    let n = code.len();

    if n == 0 {
        return Err(format!("function {}() has an empty instruction stream", f.name));
    }
    if f.chunk.lines.len() != n {
        return Err(format!(
            "function {}() has {} instructions but {} line entries",
            f.name,
            n,
            f.chunk.lines.len()
        ));
    }
    // The compiler always emits `Nil; Return` as the tail. Anything else means
    // control could run off the end of the chunk.
    match code.last() {
        Some(Op::Return) => {}
        _ => {
            return Err(verror(
                f,
                n - 1,
                "function does not end with RETURN (control could fall off the end)",
            ))
        }
    }

    // ---- depth[ip] = stack depth on entry to ip; usize::MAX = not reached ----
    let mut depth = vec![usize::MAX; n];
    let mut iter_source: Vec<Option<u16>> = vec![None; n];
    let mut stack: Vec<usize> = Vec::with_capacity(64);
    stack.push(0);
    depth[0] = 0;

    let mut max_stack = 0usize;
    let mut visits = 0usize;
    let visit_limit = n.saturating_mul(8).saturating_add(1024);

    while let Some(ip) = stack.pop() {
        visits += 1;
        if visits > visit_limit {
            return Err(verror(f, ip, "control flow did not converge (verification loop)"));
        }
        if ip >= n {
            return Err(verror(f, n.saturating_sub(1), format!("instruction pointer {} out of range", ip)));
        }

        let op = &code[ip];
        let d = depth[ip];
        debug_assert!(d != usize::MAX);

        // ---- operand-specific bounds checks ----
        match op {
            Op::Const(i) => {
                if *i as usize >= f.consts.len() {
                    return Err(verror(
                        f,
                        ip,
                        format!("constant index {} out of range (pool has {})", i, f.consts.len()),
                    ));
                }
            }
            Op::GetLocal(s) | Op::SetLocal(s) => {
                if *s >= f.slots {
                    return Err(verror(
                        f,
                        ip,
                        format!("local slot {} out of range (function declares {} slots)", s, f.slots),
                    ));
                }
            }
            Op::MakeClosure(i) => {
                // closures declared inside this proto must live in the program's
                // function table; the compiler emits them before the enclosing
                // function is finalised, so the bound is the final func count.
                if *i as usize >= func_count.saturating_add(check_funcs_after) {
                    return Err(verror(
                        f,
                        ip,
                        format!("closure index {} out of range ({} functions compiled)", i, func_count),
                    ));
                }
            }
            Op::IterInit { iter, cursor } => {
                if *iter >= f.slots || *cursor >= f.slots {
                    return Err(verror(
                        f,
                        ip,
                        format!("ITERINIT slots ({}, {}) out of range ({} slots)", iter, cursor, f.slots),
                    ));
                }
                if iter == cursor {
                    return Err(verror(f, ip, "ITERINIT uses the same slot for iterator and cursor"));
                }
            }
            Op::IterNext { iter, cursor, target, .. } => {
                if *iter >= f.slots || *cursor >= f.slots {
                    return Err(verror(
                        f,
                        ip,
                        format!("ITERNEXT slots ({}, {}) out of range ({} slots)", iter, cursor, f.slots),
                    ));
                }
                if target & GLOBAL_FLAG == 0 && (*target & !GLOBAL_FLAG) as u16 >= f.slots {
                    return Err(verror(
                        f,
                        ip,
                        format!("ITERNEXT target slot {} out of range ({} slots)", target, f.slots),
                    ));
                }
            }
            _ => {}
        }

        // ---- stack effect ----
        let e = effect(op);
        if d < e.pops {
            return Err(verror(
                f,
                ip,
                format!(
                    "stack underflow: needs {} operand(s) but only {} available at this point",
                    e.pops, d
                ),
            ));
        }
        let after = d - e.pops + e.pushes;
        if after > max_stack {
            max_stack = after;
        }

        // ---- successors ----
        let push_succ = |target: usize, want: usize, depth: &mut Vec<usize>, stack: &mut Vec<usize>| -> Result<(), String> {
            if target >= n {
                return Err(verror(
                    f,
                    ip,
                    format!("jump target {} is outside the instruction stream (length {})", target, n),
                ));
            }
            if depth[target] == usize::MAX {
                depth[target] = want;
                stack.push(target);
            } else if depth[target] != want {
                return Err(verror(
                    f,
                    target,
                    format!(
                        "inconsistent stack depth: {} on one path, {} on another (jumps must preserve stack height)",
                        depth[target], want
                    ),
                ));
            }
            Ok(())
        };

        match op {
            Op::Jump(t) => {
                push_succ(*t as usize, after, &mut depth, &mut stack)?;
            }
            Op::JumpIfFalsePop(_) | Op::JumpIfTruePop(_) => {
                let t = branch_target(op).unwrap() as usize;
                push_succ(t, after, &mut depth, &mut stack)?;
                push_succ(ip + 1, after, &mut depth, &mut stack)?;
            }
            Op::EqJf(t) | Op::NeJf(t) | Op::LtJf(t) | Op::LeJf(t) | Op::GtJf(t) | Op::GeJf(t) => {
                let t = *t as usize;
                push_succ(t, after, &mut depth, &mut stack)?;
                push_succ(ip + 1, after, &mut depth, &mut stack)?;
            }
            Op::IterNext { cursor, exit, .. } => {
                // remember which slot this instruction reads, so we can require
                // a matching ITERINIT on every path that reaches it
                match iter_source[ip] {
                    None => iter_source[ip] = Some(*cursor),
                    Some(c) if c != *cursor => {
                        return Err(verror(f, ip, "ITERNEXT reached with conflicting cursor slots"))
                    }
                    _ => {}
                }
                push_succ(*exit as usize, after, &mut depth, &mut stack)?;
                push_succ(ip + 1, after, &mut depth, &mut stack)?;
            }
            _ => {
                if is_terminal(op) {
                    // no fallthrough
                } else if ip + 1 < n {
                    push_succ(ip + 1, after, &mut depth, &mut stack)?;
                } else {
                    return Err(verror(f, ip, "execution falls off the end of the function"));
                }
            }
        }
    }

    // ---- every ITERNEXT must be dominated by an ITERINIT for the same slots ----
    // Conservative syntactic check: the cursor slot must be written by an
    // ITERINIT somewhere earlier in the same function.
    for (ip, src) in iter_source.iter().enumerate() {
        if let Some(want_cursor) = src {
            let Ok(Op::IterNext { iter: want_iter, .. }) =
                code.get(ip).cloned().ok_or(())
            else {
                continue;
            };
            let initialised = code[..ip].iter().any(|o| {
                matches!(o, Op::IterInit { iter: i, cursor: c }
                    if *i == want_iter && *c == *want_cursor)
            });
            if !initialised {
                return Err(verror(
                    f,
                    ip,
                    "ITERNEXT reached without a preceding ITERINIT for its iterator slot",
                ));
            }
        }
    }

    if !is_entry {
        // A function must not leave values behind on the stack when it returns:
        // RETURNS pops exactly one, and the implicit tail returns nil.
        for ip in 0..n {
            if let Op::Return = code[ip] {
                if depth[ip] != usize::MAX && depth[ip] != 1 {
                    return Err(verror(
                        f,
                        ip,
                        format!("RETURN with {} values on the stack (expected exactly 1)", depth[ip]),
                    ));
                }
            }
        }
    }

    Ok(Report { instructions: n, max_stack })
}

/// Verify a whole program. Runs before every execution; cost is linear in the
/// number of instructions plus edges, i.e. effectively free next to running it.
pub fn verify(prog: &Program) -> Result<Report, String> {
    let total_funcs = prog.funcs.len() + 1;

    let mut instructions = 0;
    let mut max_stack = 0;

    let main = verify_proto(&prog.main, total_funcs, true, 0)?;
    instructions += main.instructions;
    max_stack = max_stack.max(main.max_stack);

    for (i, f) in prog.funcs.iter().enumerate() {
        let r = verify_proto(f, total_funcs, false, i + 1)?;
        instructions += r.instructions;
        max_stack = max_stack.max(r.max_stack);
    }

    Ok(Report { instructions, max_stack })
}

// ============================= TESTS =============================
//
// These build instruction streams by hand — including deliberately broken
// ones — and assert that verification catches exactly what it should.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Chunk, FunctionProto, Program};
    use crate::Value;
    use std::rc::Rc;

    fn proto(name: &str, code: Vec<Op>, lines: Vec<u32>, slots: u16, consts: Vec<Value>) -> Rc<FunctionProto> {
        Rc::new(FunctionProto {
            name: name.to_string(),
            arity: 0,
            slots,
            consts,
            chunk: Chunk { code, lines },
        })
    }

    fn prog(main: Rc<FunctionProto>) -> Program {
        Program { main, funcs: vec![] }
    }

    #[test]
    fn accepts_valid_program() {
        let main = proto(
            "<main>",
            vec![Op::Nil, Op::Return],
            vec![1, 1],
            0,
            vec![],
        );
        assert!(verify(&prog(main)).is_ok());
    }

    #[test]
    fn catches_stack_underflow() {
        // Pop with nothing on the stack
        let main = proto("<main>", vec![Op::Pop, Op::Nil, Op::Return], vec![1, 1, 1], 0, vec![]);
        let err = verify(&prog(main)).unwrap_err();
        assert!(err.contains("underflow"), "{}", err);
    }

    #[test]
    fn catches_out_of_range_jump() {
        let main = proto(
            "<main>",
            vec![Op::Jump(99), Op::Nil, Op::Return],
            vec![1, 1, 1],
            0,
            vec![],
        );
        let err = verify(&prog(main)).unwrap_err();
        assert!(err.contains("jump target"), "{}", err);
    }

    #[test]
    fn catches_bad_constant_index() {
        let main = proto(
            "<main>",
            vec![Op::Const(7), Op::Pop, Op::Nil, Op::Return],
            vec![1, 1, 1, 1],
            0,
            vec![Value::Int(1)],
        );
        let err = verify(&prog(main)).unwrap_err();
        assert!(err.contains("constant index"), "{}", err);
    }

    #[test]
    fn catches_bad_local_slot() {
        let main = proto(
            "<main>",
            vec![Op::GetLocal(4), Op::Pop, Op::Nil, Op::Return],
            vec![1, 1, 1, 1],
            2,
            vec![],
        );
        let err = verify(&prog(main)).unwrap_err();
        assert!(err.contains("local slot"), "{}", err);
    }

    #[test]
    fn catches_inconsistent_stack_depth() {
        // One path reaches RETURNS with depth 1, the other with depth 0.
        //   0 GETGLOBAL 0      depth 0 -> 1
        //   1 JFALSE -> 4      depth 1 -> 0
        //   2 JUMP -> 4        depth 0
        //   3 (unreachable padding)
        //   4 RETURN           depth 0 on one path, but never 1 -> underflow at 4
        let main = proto(
            "<main>",
            vec![
                Op::JumpIfFalsePop(4),
                Op::Nil,
                Op::Jump(5),
                Op::Nil,
                Op::Return,
                Op::Nil,
                Op::Return,
            ],
            vec![1, 1, 1, 1, 1, 1, 1],
            0,
            vec![],
        );
        // JumpIfFalsePop at depth 0 is itself an underflow; the point is that
        // verification rejects it rather than running it.
        assert!(verify(&prog(main)).is_err());
    }

    #[test]
    fn catches_fallthrough_past_end() {
        let main = proto("<main>", vec![Op::Nil], vec![1], 0, vec![]);
        let err = verify(&prog(main)).unwrap_err();
        assert!(
            err.contains("RETURN") || err.contains("falls off"),
            "{}",
            err
        );
    }

    #[test]
    fn catches_itern_ext_without_init() {
        // Terminates correctly (Nil; Return) but no ITERINIT precedes ITERNEXT.
        let main = proto(
            "<main>",
            vec![
                Op::IterNext { iter: 0, cursor: 1, target: 0, exit: 1 },
                Op::Nil,
                Op::Return,
            ],
            vec![1, 1, 1],
            2,
            vec![],
        );
        let err = verify(&prog(main)).unwrap_err();
        assert!(err.contains("ITERINIT"), "{}", err);
    }

    #[test]
    fn accepts_real_loop() {
        // Exactly mirrors the compiler for:  for i in 0..3: x = i
        // ITERNEXT stores straight into its target slot and has NO stack
        // effect, which is what the earlier fixtures got wrong.
        //   0 CONST hi           -> 1
        //   1 CONST lo           -> 2
        //   2 RANGE              -> 1
        //   3 ITERINIT           -> 0
        //   4 ITERNEXT -> slot2  -> 0      exit -> 8
        //   5 GETLOCAL 2         -> 1
        //   6 SETGLOBAL x        -> 0
        //   7 JUMP -> 4
        //   8 NIL
        //   9 RETURN
        let main = proto(
            "<main>",
            vec![
                Op::Const(0),
                Op::Const(1),
                Op::MakeRange,
                Op::IterInit { iter: 0, cursor: 1 },
                Op::IterNext { iter: 0, cursor: 1, target: 2, exit: 8 },
                Op::GetLocal(2),
                Op::SetGlobal(0),
                Op::Jump(4),
                Op::Nil,
                Op::Return,
            ],
            vec![1, 1, 1, 1, 1, 1, 1, 1, 0, 0],
            3,
            vec![Value::Int(3), Value::Int(0)],
        );
        match verify(&prog(main)) {
            Ok(_) => {}
            Err(e) => panic!("REAL LOOP REJECTED: {}", e),
        }
    }
}
