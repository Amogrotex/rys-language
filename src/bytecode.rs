//! Bytecode compiler, virtual machine, and disassembler for RYS.
//!
//! The compiler turns parsed programs into instructions and interns global
//! names in the VM, allowing values to persist between REPL inputs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{
    binary, call_builtin, norm_index, repr, truthy, type_name, BinOp, Expr, Stmt, UnOp, Value,
    BUILTINS,
};

const GLOBAL_FLAG: u32 = 0x8000_0000;

// Instructions

#[derive(Clone, Copy)]
pub enum Op {
    Const(u32),

    Nil,

    GetLocal(u16),
    SetLocal(u16), // pops
    GetGlobal(u32),
    SetGlobal(u32), // pops

    Pop,
    Dup2, // duplicates top two values  [a b] -> [a b a b]
    ToBool,
    Echo, // REPL: pop, print repr unless nil

    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Neg,
    Not,

    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    Jump(u32),
    JumpIfFalsePop(u32),
    JumpIfTruePop(u32),

    // fused compare-and-branch: pop b, a; if !(a op b) then ip = target
    EqJf(u32),
    NeJf(u32),
    LtJf(u32),
    LeJf(u32),
    GtJf(u32),
    GeJf(u32),

    MakeArray(u32),
    MakeRange,
    MakeClosure(u32),
    Call(u8),

    Index,
    SetIndex,

    IterInit { iter: u16, cursor: u16 },
    IterNext { iter: u16, cursor: u16, target: u32, exit: u32 },

    Return,
}

pub struct Chunk {
    pub code: Vec<Op>,
    pub lines: Vec<u32>,
}

pub struct FunctionProto {
    pub name: String,
    pub arity: usize,
    pub slots: u16,
    pub consts: Vec<Value>,
    pub chunk: Chunk,
}

pub struct Program {
    pub main: Rc<FunctionProto>,
    pub funcs: Vec<Rc<FunctionProto>>,
}

// Compiler

struct LoopCtx {
    continues: Vec<usize>,
    breaks: Vec<usize>,
}

struct Compiler<'a> {
    funcs: Vec<Rc<FunctionProto>>,
    chunk: Chunk,
    consts: Vec<Value>,
    const_ids: HashMap<ConstKey, u32>,

    // VM global table, interned across REPL lines
    global_names: &'a mut Vec<String>,
    global_ids: &'a mut HashMap<String, u32>,

    locals: Vec<String>,
    slots: u16,
    in_func: bool,
    loops: Vec<LoopCtx>,
    repl: bool,
    cur_line: usize,
}

#[derive(Hash, PartialEq, Eq)]
enum ConstKey {
    Int(i64),
    Float(u64),
    Str(String),
    Bool(bool),
    Nil,
}

fn const_key(v: &Value) -> Option<ConstKey> {
    Some(match v {
        Value::Int(i) => ConstKey::Int(*i),
        Value::Float(f) => ConstKey::Float(f.to_bits()),
        Value::Str(s) => ConstKey::Str(s.clone()),
        Value::Bool(b) => ConstKey::Bool(*b),
        Value::Nil => ConstKey::Nil,
        _ => return None,
    })
}

fn pack_slot(local: Option<u16>, global: Option<u32>) -> u32 {
    match (local, global) {
        (Some(i), _) => i as u32,
        (None, Some(g)) => g | GLOBAL_FLAG,
        _ => unreachable!("pack_slot needs a target"),
    }
}

impl<'a> Compiler<'a> {
    fn new(
        repl: bool,
        global_names: &'a mut Vec<String>,
        global_ids: &'a mut HashMap<String, u32>,
    ) -> Self {
        Compiler {
            funcs: Vec::new(),
            chunk: Chunk { code: Vec::new(), lines: Vec::new() },
            consts: Vec::new(),
            const_ids: HashMap::new(),
            global_names,
            global_ids,
            locals: Vec::new(),
            slots: 0,
            in_func: false,
            loops: Vec::new(),
            repl,
            cur_line: 0,
        }
    }

    fn emit(&mut self, op: Op) -> usize {
        let line = self.cur_line;
        self.emit_at(op, line)
    }

    fn emit_at(&mut self, op: Op, line: usize) -> usize {
        self.chunk.code.push(op);
        self.chunk.lines.push(line as u32);
        self.chunk.code.len() - 1
    }

    fn here(&self) -> usize {
        self.chunk.code.len()
    }

    fn patch_jump(&mut self, at: usize, target: usize) {
        debug_assert!(target <= u32::MAX as usize, "jump target overflow");
        match &mut self.chunk.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalsePop(t)
            | Op::JumpIfTruePop(t)
            | Op::EqJf(t)
            | Op::NeJf(t)
            | Op::LtJf(t)
            | Op::LeJf(t)
            | Op::GtJf(t)
            | Op::GeJf(t) => *t = target as u32,
            Op::IterNext { exit, .. } => *exit = target as u32,
            _ => unreachable!("patch_jump on a non-jump instruction"),
        }
    }

    /// Intern a global name into the VM's persistent table. Returns a dense id;
    /// no string hashing happens at runtime.
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.global_ids.get(name) {
            return i;
        }
        let i = self.global_names.len() as u32;
        self.global_names.push(name.to_string());
        self.global_ids.insert(name.to_string(), i);
        i
    }

    /// Intern a constant into the current function's pool.
    fn konst(&mut self, v: Value) -> u32 {
        let key = match const_key(&v) {
            Some(k) => k,
            None => {
                self.consts.push(v);
                return (self.consts.len() - 1) as u32;
            }
        };
        if let Some(&i) = self.const_ids.get(&key) {
            return i;
        }
        let i = self.consts.len() as u32;
        self.consts.push(v);
        self.const_ids.insert(key, i);
        i
    }

    fn local_slot(&self, name: &str) -> Option<u16> {
        // `locals` is indexed by slot number, so a temp slot leaves a
        // sentinel behind and positions never drift from slots.
        self.locals
            .iter()
            .position(|n| n == name)
            .map(|i| i as u16)
    }

    fn declare_local(&mut self, name: &str) -> u16 {
        let slot = self.slots;
        self.slots += 1;
        self.locals.push(name.to_string());
        slot
    }

    fn alloc_temp(&mut self) -> u16 {
        let slot = self.slots;
        self.slots += 1;
        // unnamed slot: sentinel keeps locals[i] aligned with slot i
        self.locals.push(String::from("\u{1}TEMP"));
        slot
    }

    /// Inside functions, locals shadow globals. Top-level names are global.
    fn get_op(&mut self, name: &str) -> Op {
        if self.in_func {
            if let Some(i) = self.local_slot(name) {
                return Op::GetLocal(i);
            }
        }
        Op::GetGlobal(self.intern(name))
    }

    fn set_op(&mut self, name: &str) -> Op {
        if self.in_func {
            if let Some(i) = self.local_slot(name) {
                return Op::SetLocal(i);
            }
            let i = self.declare_local(name);
            return Op::SetLocal(i);
        }
        Op::SetGlobal(self.intern(name))
    }

    fn compile_body(&mut self, stmts: &[Stmt]) -> Result<(), String> {
        for s in stmts {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn compile_function(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
    ) -> Result<usize, String> {
        // save the enclosing compiler state
        let saved_chunk = std::mem::replace(&mut self.chunk, Chunk { code: Vec::new(), lines: Vec::new() });
        let saved_consts = std::mem::take(&mut self.consts);
        let saved_const_ids = std::mem::take(&mut self.const_ids);
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_slots = self.slots;
        let saved_in_func = self.in_func;
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_line = self.cur_line;

        // fresh function scope: params occupy the first slots
        self.in_func = true;
        self.slots = 0;
        self.locals.clear();
        for p in params {
            self.declare_local(p);
        }

        let res = self.compile_body(body);
        if res.is_ok() {
            // implicit `return nil`
            self.emit_at(Op::Nil, saved_line);
            self.emit_at(Op::Return, saved_line);
        }

        let proto = Rc::new(FunctionProto {
            name: name.to_string(),
            arity: params.len(),
            slots: self.slots,
            consts: std::mem::take(&mut self.consts),
            chunk: std::mem::replace(&mut self.chunk, saved_chunk),
        });

        // restore the enclosing function
        self.consts = saved_consts;
        self.const_ids = saved_const_ids;
        self.locals = saved_locals;
        self.slots = saved_slots;
        self.in_func = saved_in_func;
        self.loops = saved_loops;
        self.cur_line = saved_line;

        res?;
        self.funcs.push(proto);
        Ok(self.funcs.len() - 1)
    }

    /// Emit a condition: control falls through when the condition is TRUE,
    /// and jumps to a not-yet-known target when FALSE. Returns the index of
    /// the instruction to patch.
    fn cond(&mut self, e: &Expr) -> Result<usize, String> {
        if let Expr::Binary(l, op, r, line) = e {
            let mk: Option<fn(u32) -> Op> = match op {
                BinOp::Eq => Some(Op::EqJf),
                BinOp::Ne => Some(Op::NeJf),
                BinOp::Lt => Some(Op::LtJf),
                BinOp::Le => Some(Op::LeJf),
                BinOp::Gt => Some(Op::GtJf),
                BinOp::Ge => Some(Op::GeJf),
                _ => None,
            };
            if let Some(mk) = mk {
                self.cur_line = *line;
                self.expr(l)?;
                self.expr(r)?;
                return Ok(self.emit_at(mk(0), *line));
            }
        }
        self.expr(e)?;
        let line = self.cur_line;
        Ok(self.emit_at(Op::JumpIfFalsePop(0), line))
    }

    fn compile_if(
        &mut self,
        branches: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<(), String> {
        let mut end_jumps = Vec::new();
        for (cond, body) in branches {
            let line = self.cur_line;
            let jf = self.cond(cond)?;
            self.compile_body(body)?;
            end_jumps.push(self.emit_at(Op::Jump(0), line));
            let next = self.here();
            self.patch_jump(jf, next);
        }
        if let Some(body) = else_body {
            self.compile_body(body)?;
        }
        let end = self.here();
        for j in end_jumps {
            self.patch_jump(j, end);
        }
        Ok(())
    }

    fn compile_while(&mut self, cond: &Expr, body: &[Stmt], line: usize) -> Result<(), String> {
        let head = self.here();
        let jf = self.cond(cond)?;
        self.loops.push(LoopCtx { continues: Vec::new(), breaks: Vec::new() });
        self.compile_body(body)?;
        self.emit_at(Op::Jump(head as u32), line);
        let end = self.here();
        self.patch_jump(jf, end);
        let ctx = self.loops.pop().unwrap();
        for j in ctx.breaks {
            self.patch_jump(j, end);
        }
        for j in ctx.continues {
            self.patch_jump(j, head);
        }
        Ok(())
    }

    fn compile_for(
        &mut self,
        var: &str,
        iter: &Expr,
        body: &[Stmt],
        line: usize,
    ) -> Result<(), String> {
        let iter_slot = self.alloc_temp();
        let cursor_slot = self.alloc_temp();
        self.expr(iter)?;
        self.emit_at(Op::IterInit { iter: iter_slot, cursor: cursor_slot }, line);

        let head = self.here();
        let target = if self.in_func {
            let s = match self.local_slot(var) {
                Some(i) => i,
                None => self.declare_local(var),
            };
            pack_slot(Some(s), None)
        } else {
            pack_slot(None, Some(self.intern(var)))
        };
        let next = self.emit_at(
            Op::IterNext { iter: iter_slot, cursor: cursor_slot, target, exit: 0 },
            line,
        );

        self.loops.push(LoopCtx { continues: Vec::new(), breaks: Vec::new() });
        self.compile_body(body)?;
        self.emit_at(Op::Jump(head as u32), line);

        let end = self.here();
        self.patch_jump(next, end);
        let ctx = self.loops.pop().unwrap();
        for j in ctx.breaks {
            self.patch_jump(j, end);
        }
        for j in ctx.continues {
            self.patch_jump(j, head);
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Expr(e, line) => {
                self.cur_line = *line;
                self.expr(e)?;
                if self.repl && !self.in_func {
                    self.emit_at(Op::Echo, *line);
                } else {
                    self.emit_at(Op::Pop, *line);
                }
            }
            Stmt::Assign(target, value, line) => {
                self.cur_line = *line;
                match target {
                    Expr::Var(name, _) => {
                        self.expr(value)?;
                        let op = self.set_op(name);
                        self.emit_at(op, *line);
                    }
                    Expr::Index(obj, idx, _) => {
                        self.expr(obj)?;
                        self.expr(idx)?;
                        self.expr(value)?;
                        self.emit_at(Op::SetIndex, *line);
                    }
                    _ => return Err(format!("line {}: invalid assignment target", line)),
                }
            }
            Stmt::AugAssign(target, op, value, line) => {
                self.cur_line = *line;
                match target {
                    Expr::Var(name, _) => {
                        let g = self.get_op(name);
                        self.emit_at(g, *line);
                        self.expr(value)?;
                        let b = Self::binop_op(*op)
                            .ok_or_else(|| format!("line {}: invalid compound operator", line))?;
                        self.emit_at(b, *line);
                        let s = self.set_op(name);
                        self.emit_at(s, *line);
                    }
                    Expr::Index(obj, idx, _) => {
                        self.expr(obj)?;
                        self.expr(idx)?;
                        self.emit_at(Op::Dup2, *line);
                        self.emit_at(Op::Index, *line);
                        self.expr(value)?;
                        let b = Self::binop_op(*op)
                            .ok_or_else(|| format!("line {}: invalid compound operator", line))?;
                        self.emit_at(b, *line);
                        self.emit_at(Op::SetIndex, *line);
                    }
                    _ => return Err(format!("line {}: invalid assignment target", line)),
                }
            }
            Stmt::Def(name, params, body, line) => {
                self.cur_line = *line;
                let idx = self.compile_function(name, params, body)?;
                self.emit_at(Op::MakeClosure(idx as u32), *line);
                let op = self.set_op(name);
                self.emit_at(op, *line);
            }
            Stmt::If(branches, else_body, line) => {
                self.cur_line = *line;
                self.compile_if(branches, else_body)?;
            }
            Stmt::While(cond, body, line) => {
                self.cur_line = *line;
                self.compile_while(cond, body, *line)?;
            }
            Stmt::For(var, iter, body, line) => {
                self.cur_line = *line;
                self.compile_for(var, iter, body, *line)?;
            }
            Stmt::Return(e, line) => {
                self.cur_line = *line;
                match e {
                    Some(e) => self.expr(e)?,
                    None => {
                        self.emit_at(Op::Nil, *line);
                    }
                }
                self.emit_at(Op::Return, *line);
            }
            Stmt::Break(line) => {
                if !self.loops.is_empty() {
                    let j = self.emit_at(Op::Jump(0), *line);
                    self.loops.last_mut().unwrap().breaks.push(j);
                }
            }
            Stmt::Continue(line) => {
                if !self.loops.is_empty() {
                    let j = self.emit_at(Op::Jump(0), *line);
                    self.loops.last_mut().unwrap().continues.push(j);
                }
            }
        }
        Ok(())
    }

    fn binop_op(op: BinOp) -> Option<Op> {
        Some(match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Div => Op::Div,
            BinOp::Mod => Op::Mod,
            BinOp::Pow => Op::Pow,
            BinOp::Eq => Op::Eq,
            BinOp::Ne => Op::Ne,
            BinOp::Lt => Op::Lt,
            BinOp::Le => Op::Le,
            BinOp::Gt => Op::Gt,
            BinOp::Ge => Op::Ge,
            BinOp::And | BinOp::Or => return None,
        })
    }

    /// Compile-time constant folding. Only fires when the fold genuinely
    /// succeeds; anything that would error at runtime is left to the VM so
    /// error messages keep their runtime line numbers.
    fn fold(e: &Expr) -> Option<Value> {
        match e {
            Expr::Int(v) => Some(Value::Int(*v)),
            Expr::Float(v) => Some(Value::Float(*v)),
            Expr::Str(s) => Some(Value::Str(s.clone())),
            Expr::Bool(b) => Some(Value::Bool(*b)),
            Expr::Nil => Some(Value::Nil),
            Expr::Unary(UnOp::Neg, x, _) => match Self::fold(x)? {
                Value::Int(i) => i.checked_neg().map(Value::Int),
                Value::Float(f) => Some(Value::Float(-f)),
                _ => None,
            },
            Expr::Unary(UnOp::Not, x, _) => Self::fold(x).map(|v| Value::Bool(!truthy(&v))),
            Expr::Binary(l, op, r, _) => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    return None;
                }
                let lv = Self::fold(l)?;
                let rv = Self::fold(r)?;
                binary(*op, lv, rv, 0).ok()
            }
            _ => None,
        }
    }

    fn line_of(e: &Expr) -> usize {
        match e {
            Expr::Var(_, l)
            | Expr::Unary(_, _, l)
            | Expr::Binary(_, _, _, l)
            | Expr::Call(_, _, l)
            | Expr::Index(_, _, l)
            | Expr::Range(_, _, l) => *l,
            _ => 0,
        }
    }

    fn expr(&mut self, e: &Expr) -> Result<(), String> {
        // constants (including folded expressions) go through the pool
        if let Some(v) = Self::fold(e) {
            let l = Self::line_of(e);
            if l != 0 {
                self.cur_line = l;
            }
            let id = self.konst(v);
            self.emit(Op::Const(id));
            return Ok(());
        }

        match e {
            Expr::Var(name, line) => {
                self.cur_line = *line;
                let op = self.get_op(name);
                self.emit_at(op, *line);
            }
            Expr::Array(items) => {
                for it in items {
                    self.expr(it)?;
                }
                self.emit(Op::MakeArray(items.len() as u32));
            }
            Expr::Range(a, b, line) => {
                self.cur_line = *line;
                self.expr(a)?;
                self.expr(b)?;
                self.emit_at(Op::MakeRange, *line);
            }
            Expr::Unary(op, x, line) => {
                self.cur_line = *line;
                self.expr(x)?;
                let o = match op {
                    UnOp::Neg => Op::Neg,
                    UnOp::Not => Op::Not,
                };
                self.emit_at(o, *line);
            }
            Expr::Binary(l, op, r, line) => {
                self.cur_line = *line;
                match op {
                    BinOp::And => {
                        self.expr(l)?;
                        let jf = self.emit_at(Op::JumpIfFalsePop(0), *line);
                        self.expr(r)?;
                        self.emit_at(Op::ToBool, *line);
                        let end = self.emit_at(Op::Jump(0), *line);
                        let lfalse = self.here();
                        let id = self.konst(Value::Bool(false));
                        self.emit_at(Op::Const(id), *line);
                        let fin = self.here();
                        self.patch_jump(jf, lfalse);
                        self.patch_jump(end, fin);
                    }
                    BinOp::Or => {
                        self.expr(l)?;
                        let jt = self.emit_at(Op::JumpIfTruePop(0), *line);
                        self.expr(r)?;
                        self.emit_at(Op::ToBool, *line);
                        let end = self.emit_at(Op::Jump(0), *line);
                        let ltrue = self.here();
                        let id = self.konst(Value::Bool(true));
                        self.emit_at(Op::Const(id), *line);
                        let fin = self.here();
                        self.patch_jump(jt, ltrue);
                        self.patch_jump(end, fin);
                    }
                    _ => {
                        self.expr(l)?;
                        self.expr(r)?;
                        let b = Self::binop_op(*op)
                            .ok_or_else(|| format!("line {}: unsupported operator", line))?;
                        self.emit_at(b, *line);
                    }
                }
            }
            Expr::Call(callee, args, line) => {
                self.cur_line = *line;
                self.expr(callee)?;
                for a in args {
                    self.expr(a)?;
                }
                if args.len() > 255 {
                    return Err(format!("line {}: too many arguments (max 255)", line));
                }
                self.emit_at(Op::Call(args.len() as u8), *line);
            }
            Expr::Index(obj, idx, line) => {
                self.cur_line = *line;
                self.expr(obj)?;
                self.expr(idx)?;
                self.emit_at(Op::Index, *line);
            }
            _ => unreachable!("literal expressions are handled by fold()"),
        }
        Ok(())
    }
}

/// Compile a parsed program into bytecode. Global names are interned into the
/// VM's persistent table so they stay stable across REPL lines.
pub fn compile(
    stmts: &[Stmt],
    repl: bool,
    global_names: &mut Vec<String>,
    global_ids: &mut HashMap<String, u32>,
) -> Result<Program, String> {
    let mut c = Compiler::new(repl, global_names, global_ids);
    c.compile_body(stmts)?;
    c.emit_at(Op::Nil, 0);
    c.emit_at(Op::Return, 0);
    let main = Rc::new(FunctionProto {
        name: "<main>".to_string(),
        arity: 0,
        slots: c.slots,
        consts: c.consts,
        chunk: c.chunk,
    });
    Ok(Program { main, funcs: c.funcs })
}

// Vm

struct Frame {
    func: Rc<FunctionProto>,
    ip: usize,
    base: usize, // first local slot of this frame in the data stack
}

pub struct Vm {
    globals: Vec<Value>,  // dense slots, indexed by interned u32 ids
    undefined: Vec<bool>, // which global slots have never been assigned
    names: Vec<String>,   // id -> name (for error messages & dumps)
    ids: HashMap<String, u32>,
    protos: Vec<Rc<FunctionProto>>,
    stacks: Vec<Value>, // operand stack + frame locals
    frames: Vec<Frame>,
}

#[inline(always)]
fn arith(stack: &mut Vec<Value>, op: BinOp, line: usize) -> Result<(), String> {
    let r = stack.pop().unwrap();
    let l = stack.pop().unwrap();
    stack.push(binary(op, l, r, line)?);
    Ok(())
}

/// Fast path: both operands are ints. Returns None if the fast path does not
/// apply (caller falls back to the generic `binary`).
#[inline(always)]
fn int_bin(
    stack: &mut Vec<Value>,
    line: usize,
    f: impl FnOnce(i64, i64) -> Option<Value>,
) -> Option<Result<(), String>> {
    let n = stack.len();
    if n < 2 {
        return None;
    }
    if let (Value::Int(a), Value::Int(b)) = (&stack[n - 2], &stack[n - 1]) {
        let (a, b) = (*a, *b);
        let out = f(a, b);
        stack.truncate(n - 2);
        return Some(match out {
            Some(v) => {
                stack.push(v);
                Ok(())
            }
            None => Err(format!("line {}: integer overflow", line)),
        });
    }
    None
}

/// Fast path for fused compare-and-branch. Returns whether the condition is
/// true (fall through) — errors propagate.
#[inline(always)]
fn cmp_cond(stack: &mut Vec<Value>, op: BinOp, line: usize) -> Result<bool, String> {
    let n = stack.len();
    if n >= 2 {
        if let (Value::Int(a), Value::Int(b)) = (&stack[n - 2], &stack[n - 1]) {
            let (a, b) = (*a, *b);
            stack.truncate(n - 2);
            return Ok(match op {
                BinOp::Eq => a == b,
                BinOp::Ne => a != b,
                BinOp::Lt => a < b,
                BinOp::Le => a <= b,
                BinOp::Gt => a > b,
                BinOp::Ge => a >= b,
                _ => unreachable!(),
            });
        }
    }
    let r = stack.pop().unwrap();
    let l = stack.pop().unwrap();
    let v = binary(op, l, r, line)?;
    Ok(truthy(&v))
}

impl Vm {
    pub fn new() -> Self {
        Vm {
            globals: Vec::new(),
            undefined: Vec::new(),
            names: Vec::new(),
            ids: HashMap::new(),
            protos: Vec::new(),
            stacks: Vec::with_capacity(1024),
            frames: Vec::with_capacity(64),
        }
    }

    /// The compiler's view of the VM global table.
    pub fn tables(&mut self) -> (&mut Vec<String>, &mut HashMap<String, u32>) {
        (&mut self.names, &mut self.ids)
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn run(&mut self, prog: &Program) -> Result<(), String> {
        // grow the global storage to cover every interned name
        while self.globals.len() < self.names.len() {
            self.globals.push(Value::Nil);
            self.undefined.push(true);
        }
        // register built-ins in slots that have never been assigned
        for (i, name) in self.names.iter().enumerate() {
            if self.undefined[i] {
                if let Some(b) = BUILTINS.iter().find(|b| **b == name.as_str()) {
                    self.globals[i] = Value::Builtin(b);
                    self.undefined[i] = false;
                }
            }
        }

        self.protos = prog.funcs.clone();
        self.stacks.clear();
        self.frames.clear();

        let main = prog.main.clone();
        let base = self.stacks.len();
        self.stacks.resize(base + main.slots as usize, Value::Nil);
        self.frames.push(Frame { func: main, ip: 0, base });

        self.execute()
    }

    fn execute(&mut self) -> Result<(), String> {
        // move hot state into locals: no field indirection, no Rc cloning
        // per instruction. Everything is restored before returning.
        let mut stack = std::mem::take(&mut self.stacks);
        let mut globals = std::mem::take(&mut self.globals);
        let mut undefined = std::mem::take(&mut self.undefined);
        let mut frames = std::mem::take(&mut self.frames);
        let names = std::mem::take(&mut self.names);
        let protos = std::mem::take(&mut self.protos);

        let out = self.dispatch(
            &mut stack,
            &mut globals,
            &mut undefined,
            &mut frames,
            &names,
            &protos,
        );

        self.stacks = stack;
        self.globals = globals;
        self.undefined = undefined;
        self.frames = frames;
        self.names = names;
        self.protos = protos;
        out
    }

    fn dispatch(
        &mut self,
        stack: &mut Vec<Value>,
        globals: &mut Vec<Value>,
        undefined: &mut [bool],
        frames: &mut Vec<Frame>,
        names: &[String],
        protos: &[Rc<FunctionProto>],
    ) -> Result<(), String> {
        let mut args_scratch: Vec<Value> = Vec::with_capacity(8);
        let mut func: Rc<FunctionProto> = match frames.last() {
            Some(f) => f.func.clone(),
            None => return Ok(()),
        };
        let mut ip = frames.last().unwrap().ip;
        let mut base = frames.last().unwrap().base;

        loop {
            if ip >= func.chunk.code.len() {
                // safety net: treat as implicit `return nil`
                let fr = frames.pop().unwrap();
                stack.truncate(fr.base);
                if frames.is_empty() {
                    return Ok(());
                }
                stack.push(Value::Nil);
                let f = frames.last().unwrap();
                func = f.func.clone();
                ip = f.ip;
                base = f.base;
                continue;
            }

            let line = func.chunk.lines[ip] as usize;
            let op = func.chunk.code[ip];
            ip += 1;

            match op {
                Op::Const(i) => stack.push(func.consts[i as usize].clone()),
                Op::Nil => stack.push(Value::Nil),

                Op::GetLocal(i) => {
                    let v = stack[base + i as usize].clone();
                    stack.push(v);
                }
                Op::SetLocal(i) => {
                    let v = stack.pop().unwrap();
                    stack[base + i as usize] = v;
                }
                Op::GetGlobal(g) => {
                    let id = g as usize;
                    if undefined[id] {
                        return Err(format!("line {}: name '{}' is not defined", line, names[id]));
                    }
                    let v = globals[id].clone();
                    stack.push(v);
                }
                Op::SetGlobal(g) => {
                    let v = stack.pop().unwrap();
                    let id = g as usize;
                    globals[id] = v;
                    undefined[id] = false;
                }

                Op::Pop => {
                    stack.pop();
                }
                Op::Dup2 => {
                    let n = stack.len();
                    let a = stack[n - 2].clone();
                    let b = stack[n - 1].clone();
                    stack.push(a);
                    stack.push(b);
                }
                Op::ToBool => {
                    let v = stack.pop().unwrap();
                    stack.push(Value::Bool(truthy(&v)));
                }
                Op::Echo => {
                    let v = stack.pop().unwrap();
                    if !matches!(v, Value::Nil) {
                        println!("{}", repr(&v));
                    }
                }

                // ---- arithmetic: int fast paths, generic fallback ----
                Op::Add => {
                    if let Some(r) = int_bin(stack, line, |a, b| a.checked_add(b).map(Value::Int)) {
                        r?;
                    } else {
                        arith(stack, BinOp::Add, line)?;
                    }
                }
                Op::Sub => {
                    if let Some(r) = int_bin(stack, line, |a, b| a.checked_sub(b).map(Value::Int)) {
                        r?;
                    } else {
                        arith(stack, BinOp::Sub, line)?;
                    }
                }
                Op::Mul => {
                    if let Some(r) = int_bin(stack, line, |a, b| a.checked_mul(b).map(Value::Int)) {
                        r?;
                    } else {
                        arith(stack, BinOp::Mul, line)?;
                    }
                }
                Op::Div => {
                    let n = stack.len();
                    let mut done = false;
                    if n >= 2 {
                        if let (Value::Int(a), Value::Int(b)) = (&stack[n - 2], &stack[n - 1]) {
                            let (a, b) = (*a, *b);
                            if b == 0 {
                                return Err(format!("line {}: division by zero", line));
                            }
                            stack.truncate(n - 2);
                            stack.push(Value::Float(a as f64 / b as f64));
                            done = true;
                        }
                    }
                    if !done {
                        arith(stack, BinOp::Div, line)?;
                    }
                }
                Op::Mod => {
                    let n = stack.len();
                    let mut done = false;
                    if n >= 2 {
                        if let (Value::Int(a), Value::Int(b)) = (&stack[n - 2], &stack[n - 1]) {
                            let (a, b) = (*a, *b);
                            if b == 0 {
                                return Err(format!("line {}: modulo by zero", line));
                            }
                            stack.truncate(n - 2);
                            stack.push(Value::Int(a.rem_euclid(b)));
                            done = true;
                        }
                    }
                    if !done {
                        arith(stack, BinOp::Mod, line)?;
                    }
                }
                Op::Pow => arith(stack, BinOp::Pow, line)?,

                Op::Neg => {
                    let v = stack.pop().unwrap();
                    match v {
                        Value::Int(i) => match i.checked_neg() {
                            Some(n) => stack.push(Value::Int(n)),
                            None => return Err(format!("line {}: integer overflow", line)),
                        },
                        Value::Float(f) => stack.push(Value::Float(-f)),
                        v => return Err(format!("line {}: cannot negate a {}", line, type_name(&v))),
                    }
                }
                Op::Not => {
                    let v = stack.pop().unwrap();
                    stack.push(Value::Bool(!truthy(&v)));
                }

                // ---- comparisons: int fast paths ----
                Op::Eq => {
                    if let Some(r) = int_bin(stack, line, |a, b| Some(Value::Bool(a == b))) {
                        r?;
                    } else {
                        arith(stack, BinOp::Eq, line)?;
                    }
                }
                Op::Ne => {
                    if let Some(r) = int_bin(stack, line, |a, b| Some(Value::Bool(a != b))) {
                        r?;
                    } else {
                        arith(stack, BinOp::Ne, line)?;
                    }
                }
                Op::Lt => {
                    if let Some(r) = int_bin(stack, line, |a, b| Some(Value::Bool(a < b))) {
                        r?;
                    } else {
                        arith(stack, BinOp::Lt, line)?;
                    }
                }
                Op::Le => {
                    if let Some(r) = int_bin(stack, line, |a, b| Some(Value::Bool(a <= b))) {
                        r?;
                    } else {
                        arith(stack, BinOp::Le, line)?;
                    }
                }
                Op::Gt => {
                    if let Some(r) = int_bin(stack, line, |a, b| Some(Value::Bool(a > b))) {
                        r?;
                    } else {
                        arith(stack, BinOp::Gt, line)?;
                    }
                }
                Op::Ge => {
                    if let Some(r) = int_bin(stack, line, |a, b| Some(Value::Bool(a >= b))) {
                        r?;
                    } else {
                        arith(stack, BinOp::Ge, line)?;
                    }
                }

                Op::Jump(t) => ip = t as usize,
                Op::JumpIfFalsePop(t) => {
                    let v = stack.pop().unwrap();
                    if !truthy(&v) {
                        ip = t as usize;
                    }
                }
                Op::JumpIfTruePop(t) => {
                    let v = stack.pop().unwrap();
                    if truthy(&v) {
                        ip = t as usize;
                    }
                }

                // ---- fused compare-and-branch ----
                Op::EqJf(t) => {
                    if !cmp_cond(stack, BinOp::Eq, line)? {
                        ip = t as usize;
                    }
                }
                Op::NeJf(t) => {
                    if !cmp_cond(stack, BinOp::Ne, line)? {
                        ip = t as usize;
                    }
                }
                Op::LtJf(t) => {
                    if !cmp_cond(stack, BinOp::Lt, line)? {
                        ip = t as usize;
                    }
                }
                Op::LeJf(t) => {
                    if !cmp_cond(stack, BinOp::Le, line)? {
                        ip = t as usize;
                    }
                }
                Op::GtJf(t) => {
                    if !cmp_cond(stack, BinOp::Gt, line)? {
                        ip = t as usize;
                    }
                }
                Op::GeJf(t) => {
                    if !cmp_cond(stack, BinOp::Ge, line)? {
                        ip = t as usize;
                    }
                }

                // ---- data ----
                Op::MakeArray(n) => {
                    let items = stack.split_off(stack.len() - n as usize);
                    stack.push(Value::Array(Rc::new(RefCell::new(items))));
                }
                Op::MakeRange => {
                    let b = stack.pop().unwrap();
                    let a = stack.pop().unwrap();
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => stack.push(Value::Range(x, y)),
                        (x, y) => {
                            return Err(format!(
                                "line {}: range needs two ints, got {} and {}",
                                line,
                                type_name(&x),
                                type_name(&y)
                            ))
                        }
                    }
                }
                Op::MakeClosure(i) => {
                    let f = protos[i as usize].clone();
                    stack.push(Value::Closure(f));
                }

                // ---- calls ----
                Op::Call(argc) => {
                    let n = argc as usize;
                    let pos = stack.len() - n - 1;
                    let callee = stack[pos].clone();
                    match callee {
                        Value::Builtin(name) => {
                            // move args out of the operand stack without the
                            // per-call Vec allocation: drain into scratch
                            args_scratch.clear();
                            args_scratch.extend(stack.drain(pos + 1..));
                            stack.truncate(pos);
                            let v = call_builtin(name, std::mem::take(&mut args_scratch), line)?;
                            stack.push(v);
                        }
                        Value::Closure(f) => {
                            if n != f.arity {
                                return Err(format!(
                                    "line {}: {}() takes {} argument(s) but {} were given",
                                    line, f.name, f.arity, n
                                ));
                            }
                            if frames.len() >= 10_000 {
                                return Err(format!(
                                    "line {}: maximum recursion depth exceeded",
                                    line
                                ));
                            }
                            // shift args down over the callee slot in one memmove
                            stack.remove(pos);
                            frames.last_mut().unwrap().ip = ip;
                            base = pos;
                            let top = base + f.slots as usize;
                            if stack.len() < top {
                                stack.resize(top, Value::Nil);
                            }
                            func = f.clone();
                            frames.push(Frame { func: f, ip: 0, base });
                            ip = 0;
                        }
                        other => {
                            return Err(format!(
                                "line {}: {} is not callable",
                                line,
                                type_name(&other)
                            ))
                        }
                    }
                }

                // ---- indexing ----
                Op::Index => {
                    let idx = stack.pop().unwrap();
                    let obj = stack.pop().unwrap();
                    match (obj, idx) {
                        (Value::Array(arr), Value::Int(i)) => {
                            let items = arr.borrow();
                            let ix = norm_index(i, items.len(), line)?;
                            let v = items[ix].clone();
                            stack.push(v);
                        }
                        (Value::Str(s), Value::Int(i)) => {
                            let chars: Vec<char> = s.chars().collect();
                            let ix = norm_index(i, chars.len(), line)?;
                            stack.push(Value::Str(chars[ix].to_string()));
                        }
                        (o, i) => {
                            return Err(format!(
                                "line {}: cannot index a {} with a {}",
                                line,
                                type_name(&o),
                                type_name(&i)
                            ))
                        }
                    }
                }
                Op::SetIndex => {
                    let value = stack.pop().unwrap();
                    let idx = stack.pop().unwrap();
                    let obj = stack.pop().unwrap();
                    match (obj, idx) {
                        (Value::Array(arr), Value::Int(i)) => {
                            let len = arr.borrow().len();
                            let ix = norm_index(i, len, line)?;
                            arr.borrow_mut()[ix] = value;
                        }
                        (Value::Str(_), _) => {
                            return Err(format!("line {}: strings cannot be modified in place", line))
                        }
                        (o, _) => {
                            return Err(format!(
                                "line {}: cannot index-assign into a {}",
                                line,
                                type_name(&o)
                            ))
                        }
                    }
                }

                // ---- loops ----
                Op::IterInit { iter, cursor } => {
                    let v = stack.pop().unwrap();
                    let it = match v {
                        Value::Range(a, b) => Value::Range(a, b),
                        Value::Array(items) => {
                            Value::Array(Rc::new(RefCell::new(items.borrow().clone())))
                        }
                        Value::Str(s) => {
                            let chars: Vec<Value> =
                                s.chars().map(|c| Value::Str(c.to_string())).collect();
                            Value::Array(Rc::new(RefCell::new(chars)))
                        }
                        v => {
                            return Err(format!(
                                "line {}: cannot loop over a {} (need range, array, or string)",
                                line,
                                type_name(&v)
                            ))
                        }
                    };
                    stack[base + iter as usize] = it;
                    stack[base + cursor as usize] = Value::Int(0);
                }
                Op::IterNext { iter, cursor, target, exit } => {
                    // fast path: integer ranges, no allocation
                    if let Value::Range(lo, hi) = stack[base + iter as usize] {
                        let c = match stack[base + cursor as usize] {
                            Value::Int(c) => c,
                            _ => 0,
                        };
                        if lo + c < hi {
                            stack[base + cursor as usize] = Value::Int(c + 1);
                            let v = Value::Int(lo + c);
                            if target & GLOBAL_FLAG != 0 {
                                let id = (target & !GLOBAL_FLAG) as usize;
                                globals[id] = v;
                                undefined[id] = false;
                            } else {
                                stack[base + (target & !GLOBAL_FLAG) as usize] = v;
                            }
                        } else {
                            ip = exit as usize;
                        }
                        continue;
                    }

                    let it = stack[base + iter as usize].clone();
                    let c = match stack[base + cursor as usize] {
                        Value::Int(c) => c,
                        _ => 0,
                    };
                    let next = match &it {
                        Value::Array(items) => items.borrow().get(c as usize).cloned(),
                        v => {
                            return Err(format!("line {}: cannot loop over a {}", line, type_name(v)))
                        }
                    };
                    match next {
                        Some(v) => {
                            stack[base + cursor as usize] = Value::Int(c + 1);
                            if target & GLOBAL_FLAG != 0 {
                                let id = (target & !GLOBAL_FLAG) as usize;
                                globals[id] = v;
                                undefined[id] = false;
                            } else {
                                stack[base + (target & !GLOBAL_FLAG) as usize] = v;
                            }
                        }
                        None => {
                            ip = exit as usize;
                        }
                    }
                }

                Op::Return => {
                    let v = stack.pop().unwrap_or(Value::Nil);
                    let fr = frames.pop().unwrap();
                    stack.truncate(fr.base);
                    if frames.is_empty() {
                        return Ok(());
                    }
                    stack.push(v);
                    let f = frames.last().unwrap();
                    func = f.func.clone();
                    ip = f.ip;
                    base = f.base;
                }
            }
        }
    }
}

// Disassembler

fn describe(op: &Op, names: &[String]) -> String {
    fn g(names: &[String], id: u32) -> String {
        match names.get(id as usize) {
            Some(n) => format!("{} '{}'", id, n),
            None => format!("{}", id),
        }
    }
    match op {
        Op::Const(i) => format!("CONST     #{}", i),
        Op::Nil => "NIL".to_string(),
        Op::GetLocal(i) => format!("GETL      {:<4}", i),
        Op::SetLocal(i) => format!("SETL      {:<4}", i),
        Op::GetGlobal(id) => format!("GETG      {}", g(names, *id)),
        Op::SetGlobal(id) => format!("SETG      {}", g(names, *id)),
        Op::Pop => "POP".to_string(),
        Op::Dup2 => "DUP2".to_string(),
        Op::ToBool => "TOBOOL".to_string(),
        Op::Echo => "ECHO".to_string(),
        Op::Add => "ADD".to_string(),
        Op::Sub => "SUB".to_string(),
        Op::Mul => "MUL".to_string(),
        Op::Div => "DIV".to_string(),
        Op::Mod => "MOD".to_string(),
        Op::Pow => "POW".to_string(),
        Op::Neg => "NEG".to_string(),
        Op::Not => "NOT".to_string(),
        Op::Eq => "EQ".to_string(),
        Op::Ne => "NE".to_string(),
        Op::Lt => "LT".to_string(),
        Op::Le => "LE".to_string(),
        Op::Gt => "GT".to_string(),
        Op::Ge => "GE".to_string(),
        Op::Jump(t) => format!("JUMP      -> {}", t),
        Op::JumpIfFalsePop(t) => format!("JFALSE    -> {}", t),
        Op::JumpIfTruePop(t) => format!("JTRUE     -> {}", t),
        Op::EqJf(t) => format!("EQ_JF     -> {}", t),
        Op::NeJf(t) => format!("NE_JF     -> {}", t),
        Op::LtJf(t) => format!("LT_JF     -> {}", t),
        Op::LeJf(t) => format!("LE_JF     -> {}", t),
        Op::GtJf(t) => format!("GT_JF     -> {}", t),
        Op::GeJf(t) => format!("GE_JF     -> {}", t),
        Op::MakeArray(n) => format!("ARRAY     {}", n),
        Op::MakeRange => "RANGE".to_string(),
        Op::MakeClosure(i) => format!("CLOSURE   fn#{}", i),
        Op::Call(n) => format!("CALL      {}", n),
        Op::Index => "INDEX".to_string(),
        Op::SetIndex => "SETINDEX".to_string(),
        Op::IterInit { iter, cursor } => format!("ITERINIT  iter={} cursor={}", iter, cursor),
        Op::IterNext { iter, cursor, target, exit } => {
            let t = if target & GLOBAL_FLAG != 0 {
                format!("glob#{}", target & !GLOBAL_FLAG)
            } else {
                format!("local#{}", target)
            };
            format!("ITERNEXT iter={} cursor={} -> {} exit-> {}", iter, cursor, t, exit)
        }
        Op::Return => "RETURN".to_string(),
    }
}

/// Human-readable bytecode listing.
pub fn disassemble(prog: &Program, names: &[String]) -> String {
    let mut out = String::new();
    fn dump(f: &FunctionProto, names: &[String], out: &mut String) {
        out.push_str(&format!(
            "\n--- function {} (arity {}, slots {}, consts {}) ---\n",
            f.name,
            f.arity,
            f.slots,
            f.consts.len()
        ));
        for (i, op) in f.chunk.code.iter().enumerate() {
            out.push_str(&format!(
                "{:>4}  line {:<3}  {}\n",
                i,
                f.chunk.lines[i],
                describe(op, names)
            ));
        }
    }
    dump(&prog.main, names, &mut out);
    for f in &prog.funcs {
        dump(f, names, &mut out);
    }
    out
}
