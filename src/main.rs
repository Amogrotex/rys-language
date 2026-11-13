//! RYS language frontend and command-line entry point.
//!
//! Source files use the `.rys` extension. This module tokenizes and parses
//! them, defines runtime values and built-ins, and starts the VM or REPL.

mod bytecode;
mod cache;
mod verify;

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::rc::Rc;

use bytecode::Vm;

// Lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    // keywords
    Def, If, Elif, Else, While, For, In, Return, Break, Continue,
    And, Or, Not, True, False, Nil,
    // symbols
    Plus, Minus, Star, Slash, Percent, StarStar,
    Assign, Eq, Ne, Lt, Le, Gt, Ge,
    PlusEq, MinusEq, StarEq, SlashEq,
    LParen, RParen, LBracket, RBracket, Comma, Colon, DotDot,
    Newline, Indent, Dedent, Eof,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: usize,
}

fn show_tok(t: &Tok) -> String {
    match t {
        Tok::Int(v) => format!("{}", v),
        Tok::Float(v) => format!("{}", v),
        Tok::Str(s) => format!("\"{}\"", s),
        Tok::Ident(s) => s.clone(),
        Tok::Newline => "end of line".into(),
        Tok::Indent => "indent".into(),
        Tok::Dedent => "dedent".into(),
        Tok::Eof => "end of file".into(),
        Tok::Assign => "=".into(),
        Tok::Colon => ":".into(),
        other => format!("{:?}", other).to_lowercase(),
    }
}

fn lex(src: &str) -> Result<Vec<Token>, String> {
    let mut toks: Vec<Token> = Vec::new();
    let mut indents: Vec<usize> = vec![0];

    for (i, raw) in src.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.replace('\t', "    ");
        let indent = line.len() - line.trim_start_matches(' ').len();
        let content = &line[indent..];
        if content.is_empty() || content.starts_with('#') {
            continue; // blank line or full-line comment
        }

        let mut line_toks: Vec<Token> = Vec::new();
        lex_line(content, line_no, &mut line_toks)?;
        if line_toks.is_empty() {
            continue;
        }

        // indentation handling
        let current = *indents.last().unwrap();
        if indent > current {
            indents.push(indent);
            toks.push(Token { tok: Tok::Indent, line: line_no });
        } else if indent < current {
            while *indents.last().unwrap() > indent {
                indents.pop();
                toks.push(Token { tok: Tok::Dedent, line: line_no });
            }
            if *indents.last().unwrap() != indent {
                return Err(format!("line {}: inconsistent indentation", line_no));
            }
        }

        toks.extend(line_toks);
        toks.push(Token { tok: Tok::Newline, line: line_no });
    }

    let last_line = src.lines().count();
    while *indents.last().unwrap() > 0 {
        indents.pop();
        toks.push(Token { tok: Tok::Dedent, line: last_line });
    }
    toks.push(Token { tok: Tok::Eof, line: last_line });
    Ok(toks)
}

fn lex_line(content: &str, line: usize, out: &mut Vec<Token>) -> Result<(), String> {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        let c = chars[i];
        if c == ' ' {
            i += 1;
            continue;
        }
        if c == '#' {
            break; // comment until end of line
        }

        // string literal
        if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            loop {
                if i >= n {
                    return Err(format!("line {}: unterminated string", line));
                }
                let ch = chars[i];
                if ch == quote {
                    i += 1;
                    break;
                }
                if ch == '\\' {
                    i += 1;
                    if i >= n {
                        return Err(format!("line {}: unterminated string", line));
                    }
                    let e = chars[i];
                    s.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' => '\\',
                        '"' => '"',
                        '\'' => '\'',
                        '0' => '\0',
                        other => other,
                    });
                    i += 1;
                } else {
                    s.push(ch);
                    i += 1;
                }
            }
            out.push(Token { tok: Tok::Str(s), line });
            continue;
        }

        // number literal
        if c.is_ascii_digit() {
            let start = i;
            while i < n && chars[i].is_ascii_digit() {
                i += 1;
            }
            let mut is_float = false;
            // a '.' is a decimal point only if NOT '..' (range) and followed by a digit
            if i + 1 < n && chars[i] == '.' && chars[i + 1] != '.' && chars[i + 1].is_ascii_digit() {
                is_float = true;
                i += 1;
                while i < n && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text: String = chars[start..i].iter().collect();
            let tok = if is_float {
                Tok::Float(text.parse().map_err(|_| format!("line {}: bad number '{}'", line, text))?)
            } else {
                Tok::Int(text.parse().map_err(|_| format!("line {}: number too large '{}'", line, text))?)
            };
            out.push(Token { tok, line });
            continue;
        }

        // identifier / keyword
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let tok = match word.as_str() {
                "def" => Tok::Def,
                "if" => Tok::If,
                "elif" => Tok::Elif,
                "else" => Tok::Else,
                "while" => Tok::While,
                "for" => Tok::For,
                "in" => Tok::In,
                "return" => Tok::Return,
                "break" => Tok::Break,
                "continue" => Tok::Continue,
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                "true" => Tok::True,
                "false" => Tok::False,
                "nil" => Tok::Nil,
                _ => Tok::Ident(word),
            };
            out.push(Token { tok, line });
            continue;
        }

        // symbols
        let two = if i + 1 < n { Some(chars[i + 1]) } else { None };
        let tok = match (c, two) {
            ('*', Some('*')) => { i += 2; Tok::StarStar }
            ('*', Some('=')) => { i += 2; Tok::StarEq }
            ('+', Some('=')) => { i += 2; Tok::PlusEq }
            ('-', Some('=')) => { i += 2; Tok::MinusEq }
            ('/', Some('=')) => { i += 2; Tok::SlashEq }
            ('=', Some('=')) => { i += 2; Tok::Eq }
            ('!', Some('=')) => { i += 2; Tok::Ne }
            ('<', Some('=')) => { i += 2; Tok::Le }
            ('>', Some('=')) => { i += 2; Tok::Ge }
            ('.', Some('.')) => { i += 2; Tok::DotDot }
            ('+', _) => { i += 1; Tok::Plus }
            ('-', _) => { i += 1; Tok::Minus }
            ('*', _) => { i += 1; Tok::Star }
            ('/', _) => { i += 1; Tok::Slash }
            ('%', _) => { i += 1; Tok::Percent }
            ('=', _) => { i += 1; Tok::Assign }
            ('<', _) => { i += 1; Tok::Lt }
            ('>', _) => { i += 1; Tok::Gt }
            ('(', _) => { i += 1; Tok::LParen }
            (')', _) => { i += 1; Tok::RParen }
            ('[', _) => { i += 1; Tok::LBracket }
            (']', _) => { i += 1; Tok::RBracket }
            (',', _) => { i += 1; Tok::Comma }
            (':', _) => { i += 1; Tok::Colon }
            _ => return Err(format!("line {}: unexpected character '{}'", line, c)),
        };
        out.push(Token { tok, line });
    }
    Ok(())
}

// Ast

#[derive(Debug, Clone, Copy, PartialEq)]
enum BinOp {
    Add, Sub, Mul, Div, Mod, Pow,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or,
}

#[derive(Debug, Clone, Copy)]
enum UnOp {
    Neg,
    Not,
}

#[derive(Debug)]
enum Expr {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Nil,
    Var(String, usize),
    Array(Vec<Expr>),
    Range(Box<Expr>, Box<Expr>, usize),
    Unary(UnOp, Box<Expr>, usize),
    Binary(Box<Expr>, BinOp, Box<Expr>, usize),
    Call(Box<Expr>, Vec<Expr>, usize),
    Index(Box<Expr>, Box<Expr>, usize),
}

#[derive(Debug)]
enum Stmt {
    Expr(Expr, usize),
    Assign(Expr, Expr, usize),
    AugAssign(Expr, BinOp, Expr, usize),
    Def(String, Vec<String>, Rc<Vec<Stmt>>, usize),
    If(Vec<(Expr, Vec<Stmt>)>, Option<Vec<Stmt>>, usize),
    While(Expr, Vec<Stmt>, usize),
    For(String, Expr, Vec<Stmt>, usize),
    Return(Option<Expr>, usize),
    Break(usize),
    Continue(usize),
}

// Parser

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Token>) -> Self {
        Parser { toks, pos: 0 }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn line(&self) -> usize {
        self.toks[self.pos].line
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Tok, what: &str) -> Result<(), String> {
        if self.peek() == &t {
            self.next();
            Ok(())
        } else {
            Err(format!(
                "line {}: expected {} but found '{}'",
                self.line(),
                what,
                show_tok(self.peek())
            ))
        }
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        while self.peek() != &Tok::Eof {
            if self.eat(&Tok::Newline) {
                continue;
            }
            stmts.push(self.parse_statement()?);
        }
        Ok(stmts)
    }

    fn parse_statement(&mut self) -> Result<Stmt, String> {
        match self.peek() {
            Tok::Def => self.parse_def(),
            Tok::If => self.parse_if(),
            Tok::While => self.parse_while(),
            Tok::For => self.parse_for(),
            _ => self.parse_simple_stmt(),
        }
    }

    fn parse_def(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        self.next(); // def
        let name = match self.next() {
            Tok::Ident(n) => n,
            t => return Err(format!("line {}: expected function name after 'def', found '{}'", line, show_tok(&t))),
        };
        self.expect(Tok::LParen, "'(' after function name")?;
        let mut params = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                match self.next() {
                    Tok::Ident(p) => params.push(p),
                    t => return Err(format!("line {}: expected parameter name, found '{}'", line, show_tok(&t))),
                }
                if !self.eat(&Tok::Comma) || self.peek() == &Tok::RParen {
                    break;
                }
            }
        }
        self.expect(Tok::RParen, "')' after parameters")?;
        let body = self.parse_block()?;
        Ok(Stmt::Def(name, params, Rc::new(body), line))
    }

    fn parse_if(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        self.next(); // if
        let mut branches = Vec::new();
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        branches.push((cond, body));
        let mut else_body = None;
        loop {
            if self.peek() == &Tok::Elif {
                self.next();
                let c = self.parse_expr()?;
                let b = self.parse_block()?;
                branches.push((c, b));
            } else if self.peek() == &Tok::Else {
                self.next();
                else_body = Some(self.parse_block()?);
                break;
            } else {
                break;
            }
        }
        Ok(Stmt::If(branches, else_body, line))
    }

    fn parse_while(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        self.next(); // while
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt::While(cond, body, line))
    }

    fn parse_for(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        self.next(); // for
        let var = match self.next() {
            Tok::Ident(n) => n,
            t => return Err(format!("line {}: expected loop variable after 'for', found '{}'", line, show_tok(&t))),
        };
        self.expect(Tok::In, "'in' in for loop")?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt::For(var, iter, body, line))
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(Tok::Colon, "':'")?;
        if self.eat(&Tok::Newline) {
            if !self.eat(&Tok::Indent) {
                return Err(format!("line {}: expected an indented block", self.line()));
            }
            let mut stmts = Vec::new();
            loop {
                if self.eat(&Tok::Dedent) {
                    break;
                }
                if self.peek() == &Tok::Eof {
                    return Err("unexpected end of file inside a block".to_string());
                }
                if self.eat(&Tok::Newline) {
                    continue;
                }
                stmts.push(self.parse_statement()?);
            }
            Ok(stmts)
        } else {
            // inline form:  if x < 2: return x
            let s = self.parse_simple_stmt()?;
            Ok(vec![s])
        }
    }

    fn end_simple(&mut self) -> Result<(), String> {
        if self.eat(&Tok::Newline) || self.peek() == &Tok::Eof || self.peek() == &Tok::Dedent {
            Ok(())
        } else {
            Err(format!(
                "line {}: unexpected '{}' at end of statement",
                self.line(),
                show_tok(self.peek())
            ))
        }
    }

    fn parse_simple_stmt(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        match self.peek() {
            Tok::Return => {
                self.next();
                if self.peek() == &Tok::Newline || self.peek() == &Tok::Eof || self.peek() == &Tok::Dedent {
                    self.end_simple()?;
                    Ok(Stmt::Return(None, line))
                } else {
                    let e = self.parse_expr()?;
                    self.end_simple()?;
                    Ok(Stmt::Return(Some(e), line))
                }
            }
            Tok::Break => {
                self.next();
                self.end_simple()?;
                Ok(Stmt::Break(line))
            }
            Tok::Continue => {
                self.next();
                self.end_simple()?;
                Ok(Stmt::Continue(line))
            }
            _ => {
                let e = self.parse_expr()?;
                let aug = match self.peek() {
                    Tok::Assign => None,
                    Tok::PlusEq => Some(BinOp::Add),
                    Tok::MinusEq => Some(BinOp::Sub),
                    Tok::StarEq => Some(BinOp::Mul),
                    Tok::SlashEq => Some(BinOp::Div),
                    _ => {
                        self.end_simple()?;
                        return Ok(Stmt::Expr(e, line));
                    }
                };
                if !matches!(e, Expr::Var(..) | Expr::Index(..)) {
                    return Err(format!("line {}: invalid assignment target", line));
                }
                self.next(); // consume = += -= *= /=
                let value = self.parse_expr()?;
                self.end_simple()?;
                match aug {
                    None => Ok(Stmt::Assign(e, value, line)),
                    Some(op) => Ok(Stmt::AugAssign(e, op, value, line)),
                }
            }
        }
    }

    // ---- expressions (precedence climbing) ----

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut l = self.parse_and()?;
        while self.peek() == &Tok::Or {
            let line = self.line();
            self.next();
            let r = self.parse_and()?;
            l = Expr::Binary(Box::new(l), BinOp::Or, Box::new(r), line);
        }
        Ok(l)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut l = self.parse_not()?;
        while self.peek() == &Tok::And {
            let line = self.line();
            self.next();
            let r = self.parse_not()?;
            l = Expr::Binary(Box::new(l), BinOp::And, Box::new(r), line);
        }
        Ok(l)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.peek() == &Tok::Not {
            let line = self.line();
            self.next();
            let e = self.parse_not()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(e), line));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let l = self.parse_range()?;
        let op = match self.peek() {
            Tok::Eq => BinOp::Eq,
            Tok::Ne => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            _ => return Ok(l),
        };
        let line = self.line();
        self.next();
        let r = self.parse_range()?;
        Ok(Expr::Binary(Box::new(l), op, Box::new(r), line))
    }

    fn parse_range(&mut self) -> Result<Expr, String> {
        let l = self.parse_add()?;
        if self.peek() == &Tok::DotDot {
            let line = self.line();
            self.next();
            let r = self.parse_add()?;
            return Ok(Expr::Range(Box::new(l), Box::new(r), line));
        }
        Ok(l)
    }

    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut l = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            let line = self.line();
            self.next();
            let r = self.parse_mul()?;
            l = Expr::Binary(Box::new(l), op, Box::new(r), line);
        }
        Ok(l)
    }

    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut l = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            let line = self.line();
            self.next();
            let r = self.parse_unary()?;
            l = Expr::Binary(Box::new(l), op, Box::new(r), line);
        }
        Ok(l)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        let line = self.line();
        if self.eat(&Tok::Minus) {
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::Neg, Box::new(e), line));
        }
        if self.eat(&Tok::Plus) {
            return self.parse_unary();
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<Expr, String> {
        let base = self.parse_postfix()?;
        if self.peek() == &Tok::StarStar {
            let line = self.line();
            self.next();
            let exp = self.parse_unary()?; // right-associative
            return Ok(Expr::Binary(Box::new(base), BinOp::Pow, Box::new(exp), line));
        }
        Ok(base)
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    let line = self.line();
                    self.next();
                    let mut args = Vec::new();
                    if self.peek() != &Tok::RParen {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&Tok::Comma) || self.peek() == &Tok::RParen {
                                break;
                            }
                        }
                    }
                    self.expect(Tok::RParen, "')' after arguments")?;
                    e = Expr::Call(Box::new(e), args, line);
                }
                Tok::LBracket => {
                    let line = self.line();
                    self.next();
                    let idx = self.parse_expr()?;
                    self.expect(Tok::RBracket, "']' after index")?;
                    e = Expr::Index(Box::new(e), Box::new(idx), line);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        let line = self.line();
        match self.next() {
            Tok::Int(v) => Ok(Expr::Int(v)),
            Tok::Float(v) => Ok(Expr::Float(v)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::True => Ok(Expr::Bool(true)),
            Tok::False => Ok(Expr::Bool(false)),
            Tok::Nil => Ok(Expr::Nil),
            Tok::Ident(n) => Ok(Expr::Var(n, line)),
            Tok::LParen => {
                let e = self.parse_expr()?;
                self.expect(Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                if self.peek() != &Tok::RBracket {
                    loop {
                        items.push(self.parse_expr()?);
                        if !self.eat(&Tok::Comma) || self.peek() == &Tok::RBracket {
                            break;
                        }
                    }
                }
                self.expect(Tok::RBracket, "']' to close array")?;
                Ok(Expr::Array(items))
            }
            t => Err(format!("line {}: unexpected '{}'", line, show_tok(&t))),
        }
    }
}

// Values

#[derive(Clone)]
enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Nil,
    Array(Rc<RefCell<Vec<Value>>>),
    Range(i64, i64),
    Closure(Rc<bytecode::FunctionProto>),
    Builtin(&'static str),
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "string",
        Value::Bool(_) => "bool",
        Value::Nil => "nil",
        Value::Array(_) => "array",
        Value::Range(_, _) => "range",
        Value::Closure(_) => "function",
        Value::Builtin(_) => "function",
    }
}

fn display(v: &Value) -> String {
    match v {
        Value::Int(i) => format!("{}", i),
        Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{:.1}", f)
            } else {
                format!("{}", f)
            }
        }
        Value::Str(s) => s.clone(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Nil => "nil".into(),
        Value::Array(items) => {
            let inner: Vec<String> = items.borrow().iter().map(repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Range(a, b) => format!("{}..{}", a, b),
        Value::Closure(c) => format!("<function {}>", c.name),
        Value::Builtin(n) => format!("<built-in {}>", n),
    }
}

fn repr(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{}\"", s),
        other => display(other),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => !s.is_empty(),
        Value::Nil => false,
        Value::Array(a) => !a.borrow().is_empty(),
        Value::Range(a, b) => a < b,
        Value::Closure(_) | Value::Builtin(_) => true,
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Nil, Value::Nil) => true,
        (Value::Range(a1, b1), Value::Range(a2, b2)) => a1 == a2 && b1 == b2,
        (Value::Array(x), Value::Array(y)) => {
            if Rc::ptr_eq(x, y) {
                return true;
            }
            let x = x.borrow();
            let y = y.borrow();
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| value_eq(a, b))
        }
        _ => false,
    }
}

fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

// Built-Ins

const BUILTINS: &[&str] = &[
    "print", "len", "str", "int", "float", "input", "push", "pop",
    "abs", "min", "max", "sqrt", "range", "type",
    "upper", "lower", "trim", "split", "join", "contains", "reverse",
];

fn norm_index(i: i64, len: usize, line: usize) -> Result<usize, String> {
    let ix = if i < 0 { i + len as i64 } else { i };
    if ix < 0 || ix as usize >= len {
        return Err(format!("line {}: index {} out of range (length {})", line, i, len));
    }
    Ok(ix as usize)
}

fn binary(op: BinOp, l: Value, r: Value, line: usize) -> Result<Value, String> {
    use BinOp::*;
    match op {
        Eq => return Ok(Value::Bool(value_eq(&l, &r))),
        Ne => return Ok(Value::Bool(!value_eq(&l, &r))),
        _ => {}
    }

    // comparisons
    if matches!(op, Lt | Le | Gt | Ge) {
        if let (Some(a), Some(b)) = (as_num(&l), as_num(&r)) {
            let res = match op {
                Lt => a < b,
                Le => a <= b,
                Gt => a > b,
                Ge => a >= b,
                _ => unreachable!(),
            };
            return Ok(Value::Bool(res));
        }
        if let (Value::Str(a), Value::Str(b)) = (&l, &r) {
            let res = match op {
                Lt => a < b,
                Le => a <= b,
                Gt => a > b,
                Ge => a >= b,
                _ => unreachable!(),
            };
            return Ok(Value::Bool(res));
        }
        return Err(format!(
            "line {}: cannot compare {} with {}",
            line,
            type_name(&l),
            type_name(&r)
        ));
    }

    // arithmetic & friends
    match (op, &l, &r) {
        (Add, Value::Int(a), Value::Int(b)) => a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or_else(|| format!("line {}: integer overflow", line)),
        (Sub, Value::Int(a), Value::Int(b)) => a
            .checked_sub(*b)
            .map(Value::Int)
            .ok_or_else(|| format!("line {}: integer overflow", line)),
        (Mul, Value::Int(a), Value::Int(b)) => a
            .checked_mul(*b)
            .map(Value::Int)
            .ok_or_else(|| format!("line {}: integer overflow", line)),

        (Add, Value::Str(a), Value::Str(b)) => Ok(Value::Str(format!("{}{}", a, b))),
        (Add, Value::Array(a), Value::Array(b)) => {
            let mut v = a.borrow().clone();
            v.extend(b.borrow().iter().cloned());
            Ok(Value::Array(Rc::new(RefCell::new(v))))
        }

        (Mul, Value::Str(s), Value::Int(n)) | (Mul, Value::Int(n), Value::Str(s)) => {
            if *n < 0 || *n > 1_000_000 {
                return Err(format!("line {}: bad repeat count {}", line, n));
            }
            Ok(Value::Str(s.repeat(*n as usize)))
        }
        (Mul, Value::Array(a), Value::Int(n)) | (Mul, Value::Int(n), Value::Array(a)) => {
            if *n < 0 || *n > 1_000_000 {
                return Err(format!("line {}: bad repeat count {}", line, n));
            }
            let src = a.borrow();
            let mut v = Vec::with_capacity(src.len() * (*n as usize));
            for _ in 0..*n {
                v.extend(src.iter().cloned());
            }
            Ok(Value::Array(Rc::new(RefCell::new(v))))
        }

        (Div, _, _) => {
            let (a, b) = match (as_num(&l), as_num(&r)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    return Err(format!(
                        "line {}: cannot divide {} by {}",
                        line,
                        type_name(&l),
                        type_name(&r)
                    ))
                }
            };
            if b == 0.0 {
                return Err(format!("line {}: division by zero", line));
            }
            Ok(Value::Float(a / b))
        }

        (Mod, Value::Int(a), Value::Int(b)) => {
            if *b == 0 {
                return Err(format!("line {}: modulo by zero", line));
            }
            Ok(Value::Int(a.rem_euclid(*b)))
        }
        (Mod, _, _) => {
            let (a, b) = match (as_num(&l), as_num(&r)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    return Err(format!(
                        "line {}: cannot take {} modulo {}",
                        line,
                        type_name(&l),
                        type_name(&r)
                    ))
                }
            };
            if b == 0.0 {
                return Err(format!("line {}: modulo by zero", line));
            }
            Ok(Value::Float(a - b * (a / b).floor()))
        }

        (Pow, Value::Int(a), Value::Int(b)) => {
            if *b >= 0 && *b <= 62 {
                if let Some(v) = a.checked_pow(*b as u32) {
                    return Ok(Value::Int(v));
                }
            }
            Ok(Value::Float((*a as f64).powf(*b as f64)))
        }
        (Pow, _, _) => {
            let (a, b) = match (as_num(&l), as_num(&r)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    return Err(format!(
                        "line {}: cannot raise {} to a {}",
                        line,
                        type_name(&l),
                        type_name(&r)
                    ))
                }
            };
            Ok(Value::Float(a.powf(b)))
        }

        (Add | Sub | Mul, _, _) => {
            if let (Some(a), Some(b)) = (as_num(&l), as_num(&r)) {
                let v = match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    _ => unreachable!(),
                };
                return Ok(Value::Float(v));
            }
            let sym = match op {
                Add => "+",
                Sub => "-",
                Mul => "*",
                _ => unreachable!(),
            };
            Err(format!(
                "line {}: unsupported operation: {} {} {}",
                line,
                type_name(&l),
                sym,
                type_name(&r)
            ))
        }

        _ => Err(format!("line {}: unsupported operation", line)),
    }
}

// Built-Ins

fn call_builtin(name: &str, args: Vec<Value>, line: usize) -> Result<Value, String> {
    match name {
        "print" => {
            let parts: Vec<String> = args.iter().map(display).collect();
            println!("{}", parts.join(" "));
            Ok(Value::Nil)
        }
        "len" => {
            if args.len() != 1 {
                return Err(format!("line {}: len() takes exactly 1 argument", line));
            }
            match &args[0] {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::Array(a) => Ok(Value::Int(a.borrow().len() as i64)),
                Value::Range(a, b) => Ok(Value::Int((b - a).max(0))),
                v => Err(format!("line {}: len() does not work on a {}", line, type_name(v))),
            }
        }
        "str" => {
            if args.len() != 1 {
                return Err(format!("line {}: str() takes exactly 1 argument", line));
            }
            Ok(Value::Str(display(&args[0])))
        }
        "int" => {
            if args.len() != 1 {
                return Err(format!("line {}: int() takes exactly 1 argument", line));
            }
            match &args[0] {
                Value::Int(i) => Ok(Value::Int(*i)),
                Value::Float(f) => Ok(Value::Int(*f as i64)),
                Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
                Value::Str(s) => s
                    .trim()
                    .parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| format!("line {}: cannot convert \"{}\" to int", line, s)),
                v => Err(format!("line {}: cannot convert {} to int", line, type_name(v))),
            }
        }
        "float" => {
            if args.len() != 1 {
                return Err(format!("line {}: float() takes exactly 1 argument", line));
            }
            match &args[0] {
                Value::Int(i) => Ok(Value::Float(*i as f64)),
                Value::Float(f) => Ok(Value::Float(*f)),
                Value::Str(s) => s
                    .trim()
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| format!("line {}: cannot convert \"{}\" to float", line, s)),
                v => Err(format!("line {}: cannot convert {} to float", line, type_name(v))),
            }
        }
        "input" => {
            if let Some(prompt) = args.first() {
                print!("{}", display(prompt));
                std::io::stdout().flush().ok();
            }
            let mut buf = String::new();
            std::io::stdin().lock().read_line(&mut buf).ok();
            while buf.ends_with('\n') || buf.ends_with('\r') {
                buf.pop();
            }
            Ok(Value::Str(buf))
        }
        "push" => {
            if args.len() != 2 {
                return Err(format!("line {}: push() takes 2 arguments: push(array, value)", line));
            }
            match &args[0] {
                Value::Array(a) => {
                    a.borrow_mut().push(args[1].clone());
                    Ok(Value::Nil)
                }
                v => Err(format!("line {}: push() needs an array, got {}", line, type_name(v))),
            }
        }
        "pop" => {
            if args.len() != 1 {
                return Err(format!("line {}: pop() takes exactly 1 argument", line));
            }
            match &args[0] {
                Value::Array(a) => a
                    .borrow_mut()
                    .pop()
                    .ok_or_else(|| format!("line {}: pop() from empty array", line)),
                v => Err(format!("line {}: pop() needs an array, got {}", line, type_name(v))),
            }
        }
        "abs" => {
            if args.len() != 1 {
                return Err(format!("line {}: abs() takes exactly 1 argument", line));
            }
            match &args[0] {
                Value::Int(i) => Ok(Value::Int(i.abs())),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                v => Err(format!("line {}: abs() needs a number, got {}", line, type_name(v))),
            }
        }
        "min" | "max" => {
            let items: Vec<Value> = if args.len() == 1 {
                match &args[0] {
                    Value::Array(a) => a.borrow().clone(),
                    v => vec![v.clone()],
                }
            } else {
                args.clone()
            };
            if items.is_empty() {
                return Err(format!("line {}: {}() of an empty array", line, name));
            }
            let mut best = items[0].clone();
            for v in &items[1..] {
                let (a, b) = match (as_num(&best), as_num(v)) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Err(format!("line {}: {}() needs numbers", line, name)),
                };
                let take = if name == "min" { b < a } else { b > a };
                if take {
                    best = v.clone();
                }
            }
            Ok(best)
        }
        "sqrt" => {
            if args.len() != 1 {
                return Err(format!("line {}: sqrt() takes exactly 1 argument", line));
            }
            match as_num(&args[0]) {
                Some(x) if x >= 0.0 => Ok(Value::Float(x.sqrt())),
                Some(_) => Err(format!("line {}: sqrt() of a negative number", line)),
                None => Err(format!("line {}: sqrt() needs a number", line)),
            }
        }
        "range" => match args.len() {
            1 => match &args[0] {
                Value::Int(n) => Ok(Value::Range(0, *n)),
                v => Err(format!("line {}: range() needs ints, got {}", line, type_name(v))),
            },
            2 => match (&args[0], &args[1]) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Range(*a, *b)),
                _ => Err(format!("line {}: range() needs ints", line)),
            },
            _ => Err(format!("line {}: range() takes 1 or 2 arguments", line)),
        },
        "upper" | "lower" | "trim" => {
            if args.len() != 1 {
                return Err(format!("line {}: {}() takes exactly 1 argument", line, name));
            }
            match &args[0] {
                Value::Str(s) => Ok(Value::Str(match name {
                    "upper" => s.to_uppercase(),
                    "lower" => s.to_lowercase(),
                    _ => s.trim().to_string(),
                })),
                v => Err(format!("line {}: {}() needs a string, got {}", line, name, type_name(v))),
            }
        }
        "split" | "contains" => {
            if args.len() != 2 {
                return Err(format!("line {}: {}() takes exactly 2 arguments", line, name));
            }
            match (&args[0], &args[1]) {
                (Value::Str(s), Value::Str(needle)) => {
                    if name == "contains" {
                        Ok(Value::Bool(s.contains(needle.as_str())))
                    } else if needle.is_empty() {
                        Err(format!("line {}: split() separator cannot be empty", line))
                    } else {
                        let parts = s.split(needle.as_str()).map(|part| Value::Str(part.to_string())).collect();
                        Ok(Value::Array(Rc::new(RefCell::new(parts))))
                    }
                }
                _ => Err(format!("line {}: {}() needs two strings", line, name)),
            }
        }
        "join" => {
            if args.len() != 2 {
                return Err(format!("line {}: join() takes an array and a separator", line));
            }
            match (&args[0], &args[1]) {
                (Value::Array(items), Value::Str(separator)) => {
                    let items = items.borrow();
                    let mut strings = Vec::with_capacity(items.len());
                    for item in items.iter() {
                        match item {
                            Value::Str(s) => strings.push(s.as_str()),
                            _ => return Err(format!("line {}: join() array must contain only strings", line)),
                        }
                    }
                    Ok(Value::Str(strings.join(separator)))
                }
                _ => Err(format!("line {}: join() needs an array and a string separator", line)),
            }
        }
        "reverse" => {
            if args.len() != 1 {
                return Err(format!("line {}: reverse() takes exactly 1 argument", line));
            }
            match &args[0] {
                Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
                Value::Array(items) => {
                    let mut reversed = items.borrow().clone();
                    reversed.reverse();
                    Ok(Value::Array(Rc::new(RefCell::new(reversed))))
                }
                v => Err(format!("line {}: reverse() needs a string or array, got {}", line, type_name(v))),
            }
        }
        "type" => {
            if args.len() != 1 {
                return Err(format!("line {}: type() takes exactly 1 argument", line));
            }
            Ok(Value::Str(type_name(&args[0]).to_string()))
        }
        _ => Err(format!("line {}: unknown built-in '{}'", line, name)),
    }
}

// Driver

fn parse(src: &str) -> Result<Vec<Stmt>, String> {
    let toks = lex(src)?;
    let mut parser = Parser::new(toks);
    parser.parse_program()
}

fn run(vm: &mut Vm, src: &str, repl_mode: bool) -> Result<(), String> {
    let program = parse(src)?;
    let (names, ids) = vm.tables();
    let prog = bytecode::compile(&program, repl_mode, names, ids)?;
    // never execute a chunk we cannot prove well-formed
    verify::verify(&prog)?;
    vm.run(&prog)
}


fn dump_bytecode(vm: &mut Vm, src: &str) -> Result<String, String> {
    let program = parse(src)?;
    let (names, ids) = vm.tables();
    let prog = bytecode::compile(&program, false, names, ids)?;
    Ok(bytecode::disassemble(&prog, vm.names()))
}


const LOGO: &str = r#"
     /\_/\
 =  ( o.- )  =
     > ^ <
"#;

fn repl() {
    print!("\x1b[33m{}\x1b[0m", LOGO);
    println!("    R  Y  S");
    println!("RYS v0.4.0 — Rapid Yet Safe • verified bytecode VM • type 'exit' to quit");
    println!();
    let mut vm = Vm::new();
    let stdin = std::io::stdin();
    loop {
        print!(">>> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim_end().to_string();
        if trimmed.trim() == "exit" || trimmed.trim() == "quit" {
            break;
        }
        if trimmed.trim().is_empty() {
            continue;
        }
        let mut src = trimmed.clone();
        if trimmed.ends_with(':') {
            // multi-line block: keep reading until an empty line
            loop {
                print!("... ");
                std::io::stdout().flush().ok();
                let mut more = String::new();
                match stdin.lock().read_line(&mut more) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if more.trim().is_empty() {
                    break;
                }
                src.push('\n');
                src.push_str(more.trim_end());
            }
        }
        if let Err(e) = run(&mut vm, &src, true) {
            eprintln!("\x1b[1;31mRYS error:\x1b[0m {}", e);
        }
    }
    println!("bye!");
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        repl();
        return 0;
    }
    if args[1] == "--help" || args[1] == "-h" {
        println!("RYS — Rapid Yet Safe\n\nUsage: rys [OPTIONS] [FILE]\n\nRun a .rys file, or start the REPL when no file is given.\n\nOptions:\n  -h, --help          Show this help\n  -v, --version       Show the version\n  -d, --dump FILE     Print compiled bytecode\n\nSet RYS_NO_CACHE to disable the bytecode cache.");
        return 0;
    }
    if args[1] == "--version" || args[1] == "-v" {
        print!("\x1b[33m{}\x1b[0m", LOGO);
        println!(" RYS v0.4.0 — Rapid Yet Safe 😼");
        return 0;
    }
    if args[1] == "-d" || args[1] == "--dump" {
        let Some(path) = args.get(2) else {
            eprintln!("usage: rys -d program.rys");
            return 2;
        };
        let mut vm = Vm::new();
        return match std::fs::read_to_string(path).map_err(|e| format!("cannot open '{}': {}", path, e))
            .and_then(|src| dump_bytecode(&mut vm, &src))
        {
            Ok(text) => {
                print!("{}", text);
                0
            }
            Err(e) => {
                eprintln!("\x1b[1;31mRYS error:\x1b[0m {}", e);
                1
            }
        };
    }
    let path = &args[1];
    if !path.ends_with(".rys") {
        eprintln!("\x1b[1;33mnote:\x1b[0m RYS programs usually use the .rys extension");
    }
    let mut vm = Vm::new();
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\x1b[1;31mRYS error:\x1b[0m cannot open '{}': {}", path, e);
            return 1;
        }
    };

    // --- fast path: compiled bytecode cached for exactly this source ---
    if !cache_disabled() {
        if let Some(loaded) = cache::load(std::path::Path::new(path), &source) {
            // rebuild the VM global table to match the cached ids
            let mut table_ok = true;
            {
                let (names, ids) = vm.tables();
                if names.len() > loaded.globals.len() {
                    table_ok = false; // stale VM from an earlier run in this process
                } else {
                    for (i, name) in loaded.globals.iter().enumerate() {
                        if i < names.len() {
                            if &names[i] != name {
                                table_ok = false;
                                break;
                            }
                        } else {
                            names.push(name.clone());
                            ids.insert(name.clone(), i as u32);
                        }
                    }
                }
            }
            if table_ok {
                if let Err(e) = verify::verify(&loaded.program).and_then(|_| vm.run(&loaded.program)) {
                    eprintln!("\x1b[1;31mRYS error:\x1b[0m {} ({})", e, path);
                    return 1;
                }
                return 0;
            }
        }
    }

    let compiled = parse(&source).and_then(|program| {
        let (names, ids) = vm.tables();
        let prog = bytecode::compile(&program, false, names, ids)?;
        verify::verify(&prog)?;
        Ok(prog)
    });

    match compiled {
        Ok(prog) => {
            // warm the cache for the next run
            if !cache_disabled() {
                let names = vm.names().to_vec();
                cache::store(std::path::Path::new(path), &source, &prog, &names);
            }
            if let Err(e) = vm.run(&prog) {
                eprintln!("\x1b[1;31mRYS error:\x1b[0m {} ({})", e, path);
                return 1;
            }
            0
        }
        Err(e) => {
            eprintln!("\x1b[1;31mRYS error:\x1b[0m {} ({})", e, path);
            1
        }
    }
}

fn cache_disabled() -> bool {
    std::env::var_os("RYS_NO_CACHE").is_some()
}

fn main() {
    // large stack so deep RYS recursion doesn't crash the interpreter
    let child = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(real_main)
        .expect("failed to start interpreter thread");
    let code = child.join().unwrap_or(1);
    std::process::exit(code);
}
