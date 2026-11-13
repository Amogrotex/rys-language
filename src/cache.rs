// ============================================================
//  RYS compiled-bytecode cache (.rysc)
//
//  Writing a compiled program to disk means a rerun of an unchanged
//  source file skips lexing, parsing and compiling entirely.
//
//  File layout (little-endian):
//
//    magic      "RYSC"          4 bytes
//    version    u16             format version
//    flags      u8              bit 0: REPL mode program
//    src_hash   u64             FNV-1a over the exact source bytes
//    n_funcs    u32             number of function prototypes
//    n_globals  u32             size of the global name table
//    global[i]  len u16 + utf8  global names, in id order
//    func[i]    (see below)     main first, then declared functions
//    crc        u32             CRC-32 over everything above
//
//  Function record:
//    name       len u16 + utf8
//    arity      u32
//    slots      u16
//    n_consts   u32
//    const[j]   tag u8 + payload
//    n_code     u32
//    instr[k]   tag u8 + operands (see encode_op)
//    lines[k]   u32
//
//  Any structural problem — bad magic, wrong version, source hash
//  mismatch, checksum failure, truncated data — makes load() return
//  None and the caller falls back to compiling the source. A cache
//  miss is always safe; the verifier then re-checks whatever loads.
// ============================================================

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::bytecode::{Chunk, Op, Program, FunctionProto as Proto};
use crate::Value;

pub const MAGIC: &[u8; 4] = b"RYSC";
pub const VERSION: u16 = 1;

const FLAG_REPL: u8 = 1;

// ---- instruction tags (stable on disk: never renumber) ----
mod tag {
    pub const CONST: u8 = 1;
    pub const NIL: u8 = 2;
    pub const GETLOCAL: u8 = 3;
    pub const SETLOCAL: u8 = 4;
    pub const GETGLOBAL: u8 = 5;
    pub const SETGLOBAL: u8 = 6;
    pub const POP: u8 = 7;
    pub const DUP2: u8 = 8;
    pub const TOBOOL: u8 = 9;
    pub const ECHO: u8 = 10;
    pub const ADD: u8 = 11;
    pub const SUB: u8 = 12;
    pub const MUL: u8 = 13;
    pub const DIV: u8 = 14;
    pub const MOD: u8 = 15;
    pub const POW: u8 = 16;
    pub const NEG: u8 = 17;
    pub const NOT: u8 = 18;
    pub const EQ: u8 = 19;
    pub const NE: u8 = 20;
    pub const LT: u8 = 21;
    pub const LE: u8 = 22;
    pub const GT: u8 = 23;
    pub const GE: u8 = 24;
    pub const JUMP: u8 = 25;
    pub const JFALSE: u8 = 26;
    pub const JTRUE: u8 = 27;
    pub const EQJF: u8 = 28;
    pub const NEJF: u8 = 29;
    pub const LTJF: u8 = 30;
    pub const LEJF: u8 = 31;
    pub const GTJF: u8 = 32;
    pub const GEJF: u8 = 33;
    pub const MAKEARRAY: u8 = 34;
    pub const MAKERANGE: u8 = 35;
    pub const MAKECLOSURE: u8 = 36;
    pub const CALL: u8 = 37;
    pub const INDEX: u8 = 38;
    pub const SETINDEX: u8 = 39;
    pub const ITERINIT: u8 = 40;
    pub const ITERNEXT: u8 = 41;
    pub const RETURN: u8 = 42;
}

// ---- constant tags ----
mod ctag {
    pub const INT: u8 = 1;
    pub const FLOAT: u8 = 2;
    pub const STR: u8 = 3;
    pub const BOOL: u8 = 4;
    pub const NIL: u8 = 5;
}

// ============================= HASHING =============================

/// FNV-1a over the source bytes: stable across processes, unlike DefaultHasher.
pub fn source_hash(src: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in src.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// CRC-32 (IEEE) for corruption detection.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Cache path for a source file, e.g. examples/hello.rys -> <cache>/hello-<hash>.rysc
pub fn cache_path_for(src_path: &Path) -> Option<PathBuf> {
    let dir = std::env::var_os("RYS_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(|p| PathBuf::from(p).join("rys")))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache").join("rys")))?;

    // hash the absolute path so different directories never collide
    let mut path_hasher = DefaultHasher::new();
    std::fs::canonicalize(src_path)
        .unwrap_or_else(|_| src_path.to_path_buf())
        .hash(&mut path_hasher);
    let stem = src_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("program")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>();

    Some(dir.join(format!("{}-{:016x}.rysc", stem, path_hasher.finish())))
}

// ============================= WRITER =============================

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Writer { buf: Vec::with_capacity(4096) }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn string(&mut self, s: &str) {
        let b = s.as_bytes();
        let len = b.len().min(u16::MAX as usize);
        self.u16(len as u16);
        self.bytes(&b[..len]);
    }
}

fn write_op(w: &mut Writer, op: &Op) {
    use tag::*;
    match op {
        Op::Const(i) => {
            w.u8(CONST);
            w.u32(*i);
        }
        Op::Nil => w.u8(NIL),
        Op::GetLocal(s) => {
            w.u8(GETLOCAL);
            w.u16(*s);
        }
        Op::SetLocal(s) => {
            w.u8(SETLOCAL);
            w.u16(*s);
        }
        Op::GetGlobal(g) => {
            w.u8(GETGLOBAL);
            w.u32(*g);
        }
        Op::SetGlobal(g) => {
            w.u8(SETGLOBAL);
            w.u32(*g);
        }
        Op::Pop => w.u8(POP),
        Op::Dup2 => w.u8(DUP2),
        Op::ToBool => w.u8(TOBOOL),
        Op::Echo => w.u8(ECHO),
        Op::Add => w.u8(ADD),
        Op::Sub => w.u8(SUB),
        Op::Mul => w.u8(MUL),
        Op::Div => w.u8(DIV),
        Op::Mod => w.u8(MOD),
        Op::Pow => w.u8(POW),
        Op::Neg => w.u8(NEG),
        Op::Not => w.u8(NOT),
        Op::Eq => w.u8(EQ),
        Op::Ne => w.u8(NE),
        Op::Lt => w.u8(LT),
        Op::Le => w.u8(LE),
        Op::Gt => w.u8(GT),
        Op::Ge => w.u8(GE),
        Op::Jump(t) => {
            w.u8(JUMP);
            w.u32(*t);
        }
        Op::JumpIfFalsePop(t) => {
            w.u8(JFALSE);
            w.u32(*t);
        }
        Op::JumpIfTruePop(t) => {
            w.u8(JTRUE);
            w.u32(*t);
        }
        Op::EqJf(t) => {
            w.u8(EQJF);
            w.u32(*t);
        }
        Op::NeJf(t) => {
            w.u8(NEJF);
            w.u32(*t);
        }
        Op::LtJf(t) => {
            w.u8(LTJF);
            w.u32(*t);
        }
        Op::LeJf(t) => {
            w.u8(LEJF);
            w.u32(*t);
        }
        Op::GtJf(t) => {
            w.u8(GTJF);
            w.u32(*t);
        }
        Op::GeJf(t) => {
            w.u8(GEJF);
            w.u32(*t);
        }
        Op::MakeArray(n) => {
            w.u8(MAKEARRAY);
            w.u32(*n);
        }
        Op::MakeRange => w.u8(MAKERANGE),
        Op::MakeClosure(i) => {
            w.u8(MAKECLOSURE);
            w.u32(*i);
        }
        Op::Call(a) => {
            w.u8(CALL);
            w.u8(*a);
        }
        Op::Index => w.u8(INDEX),
        Op::SetIndex => w.u8(SETINDEX),
        Op::IterInit { iter, cursor } => {
            w.u8(ITERINIT);
            w.u16(*iter);
            w.u16(*cursor);
        }
        Op::IterNext { iter, cursor, target, exit } => {
            w.u8(ITERNEXT);
            w.u16(*iter);
            w.u16(*cursor);
            w.u32(*target);
            w.u32(*exit);
        }
        Op::Return => w.u8(RETURN),
    }
}

fn write_const(w: &mut Writer, v: &Value) {
    match v {
        Value::Int(i) => {
            w.u8(ctag::INT);
            w.bytes(&i.to_le_bytes());
        }
        Value::Float(f) => {
            w.u8(ctag::FLOAT);
            w.bytes(&f.to_bits().to_le_bytes());
        }
        Value::Str(s) => {
            w.u8(ctag::STR);
            w.string(s);
        }
        Value::Bool(b) => {
            w.u8(ctag::BOOL);
            w.u8(if *b { 1 } else { 0 });
        }
        Value::Nil => w.u8(ctag::NIL),
        // Constant pools only ever hold primitives; this is a compiler invariant.
        _ => w.u8(ctag::NIL),
    }
}

fn write_proto(w: &mut Writer, f: &Proto) {
    w.string(&f.name);
    w.u32(f.arity as u32);
    w.u16(f.slots);

    w.u32(f.consts.len() as u32);
    for c in &f.consts {
        write_const(w, c);
    }

    w.u32(f.chunk.code.len() as u32);
    for op in &f.chunk.code {
        write_op(w, op);
    }
    for line in &f.chunk.lines {
        w.u32(*line);
    }
}

/// Serialise a compiled program. `globals` is the VM global name table so that
/// global ids mean the same thing on reload.
pub fn serialize(prog: &Program, globals: &[String], src_hash: u64, repl: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.bytes(MAGIC);
    w.u16(VERSION);
    w.u8(if repl { FLAG_REPL } else { 0 });
    w.u64(src_hash);

    w.u32((prog.funcs.len() + 1) as u32);
    w.u32(globals.len() as u32);
    for g in globals {
        w.string(g);
    }

    write_proto(&mut w, &prog.main);
    for f in &prog.funcs {
        write_proto(&mut w, f);
    }

    let crc = crc32(&w.buf);
    w.u32(crc);
    w.buf
}

// ============================= READER =============================

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

type R<T> = Result<T, ()>;

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> R<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> R<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> R<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
    fn string(&mut self) -> R<String> {
        let len = self.u16()? as usize;
        let b = self.take(len)?;
        String::from_utf8(b.to_vec()).map_err(|_| ())
    }
}

fn read_op(r: &mut Reader) -> R<Op> {
    use tag::*;
    let t = r.u8()?;
    Ok(match t {
        CONST => Op::Const(r.u32()?),
        NIL => Op::Nil,
        GETLOCAL => Op::GetLocal(r.u16()?),
        SETLOCAL => Op::SetLocal(r.u16()?),
        GETGLOBAL => Op::GetGlobal(r.u32()?),
        SETGLOBAL => Op::SetGlobal(r.u32()?),
        POP => Op::Pop,
        DUP2 => Op::Dup2,
        TOBOOL => Op::ToBool,
        ECHO => Op::Echo,
        ADD => Op::Add,
        SUB => Op::Sub,
        MUL => Op::Mul,
        DIV => Op::Div,
        MOD => Op::Mod,
        POW => Op::Pow,
        NEG => Op::Neg,
        NOT => Op::Not,
        EQ => Op::Eq,
        NE => Op::Ne,
        LT => Op::Lt,
        LE => Op::Le,
        GT => Op::Gt,
        GE => Op::Ge,
        JUMP => Op::Jump(r.u32()?),
        JFALSE => Op::JumpIfFalsePop(r.u32()?),
        JTRUE => Op::JumpIfTruePop(r.u32()?),
        EQJF => Op::EqJf(r.u32()?),
        NEJF => Op::NeJf(r.u32()?),
        LTJF => Op::LtJf(r.u32()?),
        LEJF => Op::LeJf(r.u32()?),
        GTJF => Op::GtJf(r.u32()?),
        GEJF => Op::GeJf(r.u32()?),
        MAKEARRAY => Op::MakeArray(r.u32()?),
        MAKERANGE => Op::MakeRange,
        MAKECLOSURE => Op::MakeClosure(r.u32()?),
        CALL => Op::Call(r.u8()?),
        INDEX => Op::Index,
        SETINDEX => Op::SetIndex,
        ITERINIT => Op::IterInit { iter: r.u16()?, cursor: r.u16()? },
        ITERNEXT => Op::IterNext {
            iter: r.u16()?,
            cursor: r.u16()?,
            target: r.u32()?,
            exit: r.u32()?,
        },
        RETURN => Op::Return,
        _ => return Err(()),
    })
}

fn read_const(r: &mut Reader) -> R<Value> {
    Ok(match r.u8()? {
        ctag::INT => {
            let b = r.take(8)?;
            Value::Int(i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        }
        ctag::FLOAT => {
            let b = r.take(8)?;
            Value::Float(f64::from_bits(u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ])))
        }
        ctag::STR => Value::Str(r.string()?),
        ctag::BOOL => Value::Bool(r.u8()? != 0),
        ctag::NIL => Value::Nil,
        _ => return Err(()),
    })
}

fn read_proto(r: &mut Reader) -> R<Proto> {
    let name = r.string()?;
    let arity = r.u32()? as usize;
    let slots = r.u16()?;

    let n_consts = r.u32()? as usize;
    let mut consts = Vec::with_capacity(n_consts.min(4096));
    for _ in 0..n_consts {
        consts.push(read_const(r)?);
    }

    let n_code = r.u32()? as usize;
    let mut code = Vec::with_capacity(n_code.min(65536));
    for _ in 0..n_code {
        code.push(read_op(r)?);
    }
    let mut lines = Vec::with_capacity(n_code);
    for _ in 0..n_code {
        lines.push(r.u32()?);
    }

    Ok(Proto {
        name,
        arity,
        slots,
        consts,
        chunk: Chunk { code, lines },
    })
}

/// Deserialised cache contents.
pub struct Loaded {
    pub program: Program,
    pub globals: Vec<String>,
    #[allow(dead_code)] // recorded now so REPL sessions can share the cache later
    pub repl: bool,
}

/// Parse a cache blob. Any inconsistency yields Err(()) and the caller
/// recompiles from source.
pub fn deserialize(buf: &[u8]) -> R<Loaded> {
    if buf.len() < 4 + 2 + 1 + 8 + 4 + 4 {
        return Err(());
    }
    // checksum covers everything but the trailing 4-byte crc
    let (body, tail) = buf.split_at(buf.len() - 4);
    let stored = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
    if crc32(body) != stored {
        return Err(());
    }

    let mut r = Reader::new(body);
    if r.take(4)? != MAGIC {
        return Err(());
    }
    if r.u16()? != VERSION {
        return Err(());
    }
    let flags = r.u8()?;
    let _src_hash = r.u64()?;

    let n_funcs = r.u32()? as usize;
    let n_globals = r.u32()? as usize;
    if n_funcs == 0 || n_funcs > 1_000_000 || n_globals > 1_000_000 {
        return Err(());
    }
    let mut globals = Vec::with_capacity(n_globals.min(65536));
    for _ in 0..n_globals {
        globals.push(r.string()?);
    }

    let main = std::rc::Rc::new(read_proto(&mut r)?);
    let mut funcs = Vec::with_capacity(n_funcs - 1);
    for _ in 1..n_funcs {
        funcs.push(std::rc::Rc::new(read_proto(&mut r)?));
    }

    Ok(Loaded {
        program: Program { main, funcs },
        globals,
        repl: flags & FLAG_REPL != 0,
    })
}

/// Write the cache for a source file. Failures are silent: a cache that cannot
/// be written is not an error the user needs to hear about.
pub fn store(src_path: &Path, src: &str, prog: &Program, globals: &[String]) -> Option<PathBuf> {
    let path = cache_path_for(src_path)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let blob = serialize(prog, globals, source_hash(src), false);
    // write to a temp file then rename, so a crash never leaves a torn cache
    let tmp = path.with_extension("rysc.tmp");
    std::fs::write(&tmp, &blob).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(path)
}

/// Load the cache for a source file if it is present, intact, and was built
/// from exactly this source text.
pub fn load(src_path: &Path, src: &str) -> Option<Loaded> {
    let path = cache_path_for(src_path)?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() < 4 || &bytes[0..4] != MAGIC {
        return None;
    }
    // version + hash pre-check without trusting the rest
    let mut r = Reader::new(&bytes[..bytes.len() - 4]);
    r.take(4).ok()?;
    if r.u16().ok()? != VERSION {
        return None;
    }
    r.u8().ok()?;
    if r.u64().ok()? != source_hash(src) {
        return None;
    }
    deserialize(&bytes).ok()
}
