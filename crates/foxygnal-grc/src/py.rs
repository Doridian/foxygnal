//! The small subset of Python that GRC parameter values use: numbers,
//! strings, names (`samp_rate`, `math.pi`, `firdes.WIN_HAMMING`), arithmetic,
//! calls (`int(x)`, `firdes.low_pass(1, samp_rate, 100e3, 10e3)`), lists and
//! tuples. Anything else is a parse error and the caller falls back.
//!
//! Three things can be done with a parsed expression: evaluate it (with the
//! values of the graph's variables), translate it to foxygnal's expression
//! syntax (`$samp_rate / 10`, see `foxygnal_core::expr`), or pick it apart
//! (the mapper reads `firdes.low_pass(...)` arguments).

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Py {
    /// A number and its source text (kept so `2.4e6` stays `2.4e6`).
    Num(f64, String),
    Str(String),
    Bool(bool),
    None,
    /// A name, possibly dotted: `samp_rate`, `math.pi`, `firdes.WIN_HAMMING`.
    Name(String),
    Neg(Box<Py>),
    Bin(BinOp, Box<Py>, Box<Py>),
    /// `f(args, key=value)`, `f` a (dotted) name.
    Call(String, Vec<Py>, Vec<(String, Py)>),
    /// A list or tuple.
    List(Vec<Py>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
}

/// A value an expression evaluates to.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Num(f64),
    Str(String),
    Bool(bool),
    List(Vec<Value>),
    None,
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(v) => Some(*v),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            _ => None,
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Num(v) => *v != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Bool(b) => *b,
            Value::List(l) => !l.is_empty(),
            Value::None => false,
        }
    }
}

// ---- Parsing ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64, String),
    Str(String),
    Name(String),
    Op(&'static str),
}

const OPS: &[&str] = &[
    "**", "//", "+", "-", "*", "/", "%", "(", ")", "[", "]", ",", ".", "=",
];

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = i;
            if c == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B')) {
                let radix = match b[i + 1] {
                    b'x' | b'X' => 16,
                    b'o' | b'O' => 8,
                    _ => 2,
                };
                i += 2;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let digits = src[start + 2..i].replace('_', "");
                let v = i64::from_str_radix(&digits, radix)
                    .map_err(|_| format!("`{}` isn't a number", &src[start..i]))?;
                out.push(Tok::Num(v as f64, src[start..i].to_owned()));
                continue;
            }
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                i += 1;
            }
            if i < b.len() && b[i] == b'.' {
                i += 1;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    i = j;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            if i < b.len() && (b[i] == b'j' || b[i] == b'J') {
                return Err("complex numbers aren't supported".into());
            }
            let text = &src[start..i];
            let v: f64 = text
                .replace('_', "")
                .parse()
                .map_err(|_| format!("`{text}` isn't a number"))?;
            out.push(Tok::Num(v, text.to_owned()));
        } else if c == b'\'' || c == b'"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            let mut chars = src[i..].char_indices();
            let mut closed = false;
            while let Some((k, ch)) = chars.next() {
                match ch {
                    '\\' => {
                        let (_, e) = chars.next().ok_or("unterminated string")?;
                        s.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '0' => '\0',
                            other => other,
                        });
                    }
                    ch if ch as u32 == u32::from(quote) => {
                        i += k + 1;
                        closed = true;
                        break;
                    }
                    ch => s.push(ch),
                }
            }
            if !closed {
                return Err("unterminated string".into());
            }
            out.push(Tok::Str(s));
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            // String prefixes: r'…', b'…', u'…'.
            let word = &src[start..i];
            if matches!(word, "r" | "b" | "u" | "R" | "B" | "U")
                && matches!(b.get(i), Some(b'\'' | b'"'))
            {
                continue;
            }
            out.push(Tok::Name(word.to_owned()));
        } else {
            let rest = &src[i..];
            let op = OPS
                .iter()
                .find(|op| rest.starts_with(**op))
                .ok_or_else(|| {
                    format!("`{}` isn't supported", rest.chars().next().unwrap_or(' '))
                })?;
            out.push(Tok::Op(op));
            i += op.len();
        }
    }
    Ok(out)
}

const MAX_DEPTH: usize = 64;

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if *o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, op: &str) -> Result<(), String> {
        if self.eat(op) {
            Ok(())
        } else {
            Err(format!("expected `{op}`"))
        }
    }

    fn expr(&mut self) -> Result<Py, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("expression is nested too deeply".into());
        }
        let mut lhs = self.term()?;
        loop {
            let op = if self.eat("+") {
                BinOp::Add
            } else if self.eat("-") {
                BinOp::Sub
            } else {
                break;
            };
            let rhs = self.term()?;
            lhs = Py::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        self.depth -= 1;
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Py, String> {
        let mut lhs = self.factor()?;
        loop {
            let op = if self.eat("*") {
                BinOp::Mul
            } else if self.eat("/") {
                BinOp::Div
            } else if self.eat("//") {
                BinOp::FloorDiv
            } else if self.eat("%") {
                BinOp::Mod
            } else {
                break;
            };
            let rhs = self.factor()?;
            lhs = Py::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn factor(&mut self) -> Result<Py, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("expression is nested too deeply".into());
        }
        let r = if self.eat("-") {
            self.factor().map(|e| match e {
                // Fold literal negatives so `-1` stays a number.
                Py::Num(v, t) => Py::Num(-v, format!("-{t}")),
                e => Py::Neg(Box::new(e)),
            })
        } else if self.eat("+") {
            self.factor()
        } else {
            let base = self.postfix()?;
            if self.eat("**") {
                let exp = self.factor()?;
                Ok(Py::Bin(BinOp::Pow, Box::new(base), Box::new(exp)))
            } else {
                Ok(base)
            }
        };
        self.depth -= 1;
        r
    }

    fn postfix(&mut self) -> Result<Py, String> {
        let mut e = self.atom()?;
        loop {
            if self.eat(".") {
                let Some(Tok::Name(attr)) = self.peek().cloned() else {
                    return Err("expected a name after `.`".into());
                };
                self.pos += 1;
                e = match e {
                    Py::Name(n) => Py::Name(format!("{n}.{attr}")),
                    _ => return Err("attributes are only supported on names".into()),
                };
            } else if self.eat("(") {
                let Py::Name(f) = e else {
                    return Err("only named functions can be called".into());
                };
                let (mut args, mut kwargs) = (Vec::new(), Vec::new());
                while !self.eat(")") {
                    if let (Some(Tok::Name(k)), Some(Tok::Op("="))) =
                        (self.toks.get(self.pos), self.toks.get(self.pos + 1))
                    {
                        let k = k.clone();
                        self.pos += 2;
                        kwargs.push((k, self.expr()?));
                    } else {
                        args.push(self.expr()?);
                    }
                    if !self.eat(",") {
                        self.expect(")")?;
                        break;
                    }
                }
                e = Py::Call(f, args, kwargs);
            } else {
                return Ok(e);
            }
        }
    }

    fn atom(&mut self) -> Result<Py, String> {
        let t = self
            .peek()
            .cloned()
            .ok_or("expected a value, found the end")?;
        self.pos += 1;
        match t {
            Tok::Num(v, s) => Ok(Py::Num(v, s)),
            Tok::Str(mut s) => {
                // Adjacent literals concatenate.
                while let Some(Tok::Str(more)) = self.peek().cloned() {
                    self.pos += 1;
                    s.push_str(&more);
                }
                Ok(Py::Str(s))
            }
            Tok::Name(n) => Ok(match n.as_str() {
                "True" => Py::Bool(true),
                "False" => Py::Bool(false),
                "None" => Py::None,
                "if" | "else" | "and" | "or" | "not" | "lambda" | "for" | "in" | "is" => {
                    return Err(format!("`{n}` isn't supported"));
                }
                _ => Py::Name(n),
            }),
            Tok::Op("(") => {
                if self.eat(")") {
                    return Ok(Py::List(vec![]));
                }
                let first = self.expr()?;
                if self.eat(")") {
                    return Ok(first);
                }
                let mut items = vec![first];
                while self.eat(",") {
                    if self.eat(")") {
                        return Ok(Py::List(items));
                    }
                    items.push(self.expr()?);
                }
                self.expect(")")?;
                Ok(Py::List(items))
            }
            Tok::Op("[") => {
                let mut items = Vec::new();
                while !self.eat("]") {
                    items.push(self.expr()?);
                    if !self.eat(",") {
                        self.expect("]")?;
                        break;
                    }
                }
                Ok(Py::List(items))
            }
            Tok::Op(o) => Err(format!("unexpected `{o}`")),
        }
    }
}

impl Py {
    pub fn parse(src: &str) -> Result<Py, String> {
        let mut p = Parser {
            toks: tokenize(src)?,
            pos: 0,
            depth: 0,
        };
        let e = p.expr()?;
        // A bare tuple: `1, 2`.
        if p.eat(",") {
            let mut items = vec![e];
            while p.peek().is_some() {
                items.push(p.expr()?);
                if !p.eat(",") {
                    break;
                }
            }
            return match p.peek() {
                None => Ok(Py::List(items)),
                Some(t) => Err(format!("unexpected {}", describe(t))),
            };
        }
        match p.peek() {
            None => Ok(e),
            Some(t) => Err(format!("unexpected {}", describe(t))),
        }
    }

    /// The names (variables) this expression refers to, outside known
    /// constants and function names.
    pub fn names(&self) -> Vec<&str> {
        fn walk<'a>(e: &'a Py, out: &mut Vec<&'a str>) {
            match e {
                Py::Name(n) if constant(n).is_none() => out.push(n),
                Py::Neg(e) => walk(e, out),
                Py::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Py::Call(_, args, kwargs) => {
                    args.iter().for_each(|a| walk(a, out));
                    kwargs.iter().for_each(|(_, a)| walk(a, out));
                }
                Py::List(items) => items.iter().for_each(|a| walk(a, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Num(_, s) => format!("`{s}`"),
        Tok::Str(s) => format!("'{s}'"),
        Tok::Name(n) => format!("`{n}`"),
        Tok::Op(o) => format!("`{o}`"),
    }
}

/// Strip a `math.` / `numpy.` / `np.` module prefix.
fn unmodule(name: &str) -> &str {
    for m in ["math.", "numpy.", "np.", "cmath."] {
        if let Some(rest) = name.strip_prefix(m) {
            return rest;
        }
    }
    name
}

/// Known numeric constants.
pub fn constant(name: &str) -> Option<f64> {
    match name {
        "math.pi" | "numpy.pi" | "np.pi" | "pi" | "cmath.pi" => Some(std::f64::consts::PI),
        "math.e" | "numpy.e" | "np.e" => Some(std::f64::consts::E),
        "math.tau" => Some(std::f64::consts::TAU),
        _ => None,
    }
}

// ---- Evaluation -------------------------------------------------------------

/// Evaluate with `var` looking up names (variables). Errors say why.
pub fn eval(e: &Py, var: &mut dyn FnMut(&str) -> Option<Value>) -> Result<Value, String> {
    let num = |v: Value, what: &Py| {
        v.as_f64()
            .ok_or_else(|| format!("`{}` isn't a number", show(what)))
    };
    Ok(match e {
        Py::Num(v, _) => Value::Num(*v),
        Py::Str(s) => Value::Str(s.clone()),
        Py::Bool(b) => Value::Bool(*b),
        Py::None => Value::None,
        Py::Name(n) => match constant(n) {
            Some(v) => Value::Num(v),
            None => var(n).ok_or_else(|| format!("unknown name `{n}`"))?,
        },
        Py::Neg(x) => Value::Num(-num(eval(x, var)?, x)?),
        Py::Bin(op, a, b) => {
            let (va, vb) = (eval(a, var)?, eval(b, var)?);
            if let (BinOp::Add, Value::Str(x), Value::Str(y)) = (op, &va, &vb) {
                return Ok(Value::Str(format!("{x}{y}")));
            }
            let (x, y) = (num(va, a)?, num(vb, b)?);
            let zero = || Err("division by zero".to_string());
            let v = match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div if y == 0.0 => return zero(),
                BinOp::Div => x / y,
                BinOp::FloorDiv if y == 0.0 => return zero(),
                BinOp::FloorDiv => (x / y).floor(),
                BinOp::Mod if y == 0.0 => return zero(),
                BinOp::Mod => x - y * (x / y).floor(),
                BinOp::Pow => x.powf(y),
            };
            if !v.is_finite() {
                return Err("the result is too large".into());
            }
            Value::Num(v)
        }
        Py::Call(f, args, kwargs) => {
            if !kwargs.is_empty() {
                return Err(format!(
                    "`{f}(…)` with keyword arguments can't be evaluated"
                ));
            }
            let vals: Vec<Value> = args
                .iter()
                .map(|a| eval(a, var))
                .collect::<Result<_, _>>()?;
            let nums = || -> Result<Vec<f64>, String> {
                vals.iter()
                    .zip(args)
                    .map(|(v, a)| num(v.clone(), a))
                    .collect()
            };
            let one = || -> Result<f64, String> {
                match nums()?.as_slice() {
                    [x] => Ok(*x),
                    _ => Err(format!("`{f}` takes one argument")),
                }
            };
            let v = match unmodule(f) {
                "int" => match vals.as_slice() {
                    [Value::Str(s)] => s
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| format!("int('{s}') isn't a number"))?
                        as f64,
                    _ => one()?.trunc(),
                },
                "float" => match vals.as_slice() {
                    [Value::Str(s)] => s
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| format!("float('{s}') isn't a number"))?,
                    _ => one()?,
                },
                "bool" => return Ok(Value::Bool(vals.first().is_some_and(Value::truthy))),
                "str" => {
                    return match vals.as_slice() {
                        [Value::Str(s)] => Ok(Value::Str(s.clone())),
                        [Value::Num(n)] if n.fract() == 0.0 && n.abs() < 1e15 => {
                            Ok(Value::Str(format!("{}", *n as i64)))
                        }
                        _ => Err("`str(…)` of that can't be evaluated".into()),
                    };
                }
                "round" => match nums()?.as_slice() {
                    [x] => round_half_even(*x),
                    [x, n] => {
                        let p = 10f64.powi(*n as i32);
                        round_half_even(x * p) / p
                    }
                    _ => return Err("`round` takes one or two arguments".into()),
                },
                "abs" | "fabs" => one()?.abs(),
                "sqrt" => one()?.sqrt(),
                "exp" => one()?.exp(),
                "log10" => one()?.log10(),
                "log2" => one()?.log2(),
                "log" => match nums()?.as_slice() {
                    [x] => x.ln(),
                    [x, b] => x.ln() / b.ln(),
                    _ => return Err("`log` takes one or two arguments".into()),
                },
                "floor" => one()?.floor(),
                "ceil" => one()?.ceil(),
                "sin" => one()?.sin(),
                "cos" => one()?.cos(),
                "radians" => one()?.to_radians(),
                "degrees" => one()?.to_degrees(),
                "pow" => match nums()?.as_slice() {
                    [x, y] => x.powf(*y),
                    _ => return Err("`pow` takes two arguments".into()),
                },
                "min" | "max" => {
                    let n = nums()?;
                    let it = n.iter().copied();
                    let v = if unmodule(f) == "min" {
                        it.reduce(f64::min)
                    } else {
                        it.reduce(f64::max)
                    };
                    v.ok_or_else(|| format!("`{f}` needs arguments"))?
                }
                _ => return Err(format!("`{f}(…)` can't be evaluated")),
            };
            if !v.is_finite() {
                return Err(format!("`{}` isn't a finite number", show(e)));
            }
            Value::Num(v)
        }
        Py::List(items) => Value::List(
            items
                .iter()
                .map(|a| eval(a, var))
                .collect::<Result<_, _>>()?,
        ),
    })
}

/// Python 3's `round`: halves go to the even neighbour.
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - x.signum()
    } else {
        r
    }
}

// ---- Translation to foxygnal expressions --------------------------------------

/// Translate to foxygnal's expression syntax. `is_var` says whether a name
/// is a numeric graph variable (written `$name`). Only `+ - * /`, unary
/// minus, numbers, known constants and variables translate; `int()` /
/// `float()` are passed through (the caller checks the result still has the
/// same value), anything else is an error.
pub fn translate(e: &Py, is_var: &dyn Fn(&str) -> bool) -> Result<String, String> {
    emit(e, is_var).map(|(s, _)| s)
}

const P_ADD: u8 = 1;
const P_MUL: u8 = 2;
const P_UNARY: u8 = 3;
const P_ATOM: u8 = 4;

fn emit(e: &Py, is_var: &dyn Fn(&str) -> bool) -> Result<(String, u8), String> {
    let wrap = |(s, p): (String, u8), min: u8| if p < min { format!("({s})") } else { s };
    Ok(match e {
        Py::Num(v, text) => {
            if *v < 0.0 {
                (
                    format!("-{}", num_text(-v, text.trim_start_matches('-'))),
                    P_UNARY,
                )
            } else {
                (num_text(*v, text), P_ATOM)
            }
        }
        Py::Bool(b) => (u8::from(*b).to_string(), P_ATOM),
        Py::Name(n) => match constant(n) {
            Some(v) => (fmt_num(v), P_ATOM),
            None if is_var(n) => (format!("${n}"), P_ATOM),
            None => return Err(format!("`{n}` isn't a numeric variable")),
        },
        Py::Neg(x) => (format!("-{}", wrap(emit(x, is_var)?, P_UNARY)), P_UNARY),
        Py::Bin(op, a, b) => {
            let (sym, p, rmin) = match op {
                BinOp::Add => ("+", P_ADD, P_ADD),
                BinOp::Sub => ("-", P_ADD, P_MUL),
                BinOp::Mul => ("*", P_MUL, P_MUL),
                // `//` becomes `/`; the caller's value check catches the
                // difference when the operands aren't multiples.
                BinOp::Div | BinOp::FloorDiv => ("/", P_MUL, P_UNARY),
                BinOp::Mod | BinOp::Pow => {
                    return Err(format!(
                        "`{}` has no foxygnal equivalent",
                        if *op == BinOp::Mod { "%" } else { "**" }
                    ));
                }
            };
            let l = wrap(emit(a, is_var)?, p);
            let r = wrap(emit(b, is_var)?, rmin);
            (format!("{l} {sym} {r}"), p)
        }
        Py::Call(f, args, kwargs) if kwargs.is_empty() && args.len() == 1 => match unmodule(f) {
            "int" | "float" => emit(&args[0], is_var)?,
            _ => return Err(format!("`{f}(…)` has no foxygnal equivalent")),
        },
        Py::Call(f, ..) => return Err(format!("`{f}(…)` has no foxygnal equivalent")),
        Py::Str(_) => return Err("a string isn't a number".into()),
        Py::None => return Err("`None` isn't a number".into()),
        Py::List(_) => return Err("a list isn't a number".into()),
    })
}

/// A number literal as foxygnal reads it: the Python text when it is plain
/// decimal (`2.4e6`, `100_000`), else the value.
fn num_text(v: f64, text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'e' | b'E' | b'+' | b'-'))
        && text.as_bytes()[0] != b'.';
    if plain { text.to_owned() } else { fmt_num(v) }
}

/// A number written plainly (whole numbers without a fraction).
pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let mut s = String::new();
        let _ = write!(s, "{v}");
        s
    }
}

/// The expression as Python-ish text, for messages.
pub fn show(e: &Py) -> String {
    match e {
        Py::Num(_, t) => t.clone(),
        Py::Str(s) => format!("'{s}'"),
        Py::Bool(b) => if *b { "True" } else { "False" }.into(),
        Py::None => "None".into(),
        Py::Name(n) => n.clone(),
        Py::Neg(x) => format!("-{}", show(x)),
        Py::Bin(op, a, b) => {
            let sym = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::FloorDiv => "//",
                BinOp::Mod => "%",
                BinOp::Pow => "**",
            };
            format!("({} {sym} {})", show(a), show(b))
        }
        Py::Call(f, args, kwargs) => {
            let mut parts: Vec<String> = args.iter().map(show).collect();
            parts.extend(kwargs.iter().map(|(k, v)| format!("{k}={}", show(v))));
            format!("{f}({})", parts.join(", "))
        }
        Py::List(items) => format!(
            "[{}]",
            items.iter().map(show).collect::<Vec<_>>().join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(name: &str) -> Option<Value> {
        match name {
            "samp_rate" => Some(Value::Num(2.4e6)),
            "decim" => Some(Value::Num(10.0)),
            "fm_dev" => Some(Value::Num(75e3)),
            "name" => Some(Value::Str("x".into())),
            _ => None,
        }
    }

    fn ev(s: &str) -> Result<Value, String> {
        eval(&Py::parse(s)?, &mut vars)
    }

    fn tr(s: &str) -> Result<String, String> {
        translate(&Py::parse(s)?, &|n| {
            matches!(n, "samp_rate" | "decim" | "fm_dev")
        })
    }

    #[test]
    fn numbers() {
        assert_eq!(ev("1e6"), Ok(Value::Num(1e6)));
        assert_eq!(ev("2.4e6"), Ok(Value::Num(2.4e6)));
        assert_eq!(ev("100_000"), Ok(Value::Num(1e5)));
        assert_eq!(ev("0x10"), Ok(Value::Num(16.0)));
        assert_eq!(ev(".5"), Ok(Value::Num(0.5)));
        assert_eq!(ev("-3"), Ok(Value::Num(-3.0)));
        assert!(ev("1j").is_err());
    }

    #[test]
    fn arithmetic_matches_python() {
        assert_eq!(ev("7 / 2"), Ok(Value::Num(3.5)));
        assert_eq!(ev("7 // 2"), Ok(Value::Num(3.0)));
        assert_eq!(ev("-7 // 2"), Ok(Value::Num(-4.0)));
        assert_eq!(ev("-7 % 3"), Ok(Value::Num(2.0)));
        assert_eq!(ev("2 ** 10"), Ok(Value::Num(1024.0)));
        assert_eq!(ev("-2 ** 2"), Ok(Value::Num(-4.0)));
        assert_eq!(ev("2 * (3 + 4)"), Ok(Value::Num(14.0)));
        assert_eq!(ev("int(samp_rate / 7)"), Ok(Value::Num(342857.0)));
        assert_eq!(ev("int(-2.5)"), Ok(Value::Num(-2.0)));
        assert_eq!(ev("round(2.5)"), Ok(Value::Num(2.0)));
        assert_eq!(ev("round(3.5)"), Ok(Value::Num(4.0)));
        assert_eq!(ev("float('1e3')"), Ok(Value::Num(1000.0)));
        assert_eq!(ev("max(1, decim, 3)"), Ok(Value::Num(10.0)));
        assert_eq!(ev("math.pi * 2"), Ok(Value::Num(std::f64::consts::TAU)));
        assert_eq!(ev("numpy.sqrt(16)"), Ok(Value::Num(4.0)));
        assert_eq!(ev("True"), Ok(Value::Bool(true)));
        assert_eq!(ev("'a' 'b' + name"), Ok(Value::Str("abx".into())));
        assert!(ev("1 / 0").unwrap_err().contains("division"));
        assert!(ev("nope + 1").unwrap_err().contains("nope"));
    }

    #[test]
    fn parses_calls_lists_and_attributes() {
        let e = Py::parse("firdes.low_pass(1, samp_rate, cutoff_freq=100e3, transition_width=10e3, window=window.WIN_HAMMING)").unwrap();
        let Py::Call(f, args, kwargs) = e else {
            panic!("not a call")
        };
        assert_eq!(f, "firdes.low_pass");
        assert_eq!(args.len(), 2);
        assert_eq!(kwargs[0].0, "cutoff_freq");
        assert_eq!(kwargs[2].1, Py::Name("window.WIN_HAMMING".into()));
        assert_eq!(
            Py::parse("[1, 2,]").unwrap(),
            Py::List(vec![Py::Num(1.0, "1".into()), Py::Num(2.0, "2".into())])
        );
        assert_eq!(
            Py::parse("(1,)").unwrap(),
            Py::List(vec![Py::Num(1.0, "1".into())])
        );
        assert_eq!(
            ev("(1, 2)"),
            Ok(Value::List(vec![Value::Num(1.0), Value::Num(2.0)]))
        );
        assert!(Py::parse("a if b else c").is_err());
        assert!(Py::parse("x[0]").is_err());
        assert!(Py::parse("lambda: 1").is_err());
        assert!(Py::parse("1 +").is_err());
        assert!(Py::parse(&"(".repeat(500)).is_err());
    }

    #[test]
    fn translates_to_foxygnal() {
        assert_eq!(tr("2.4e6"), Ok("2.4e6".into()));
        assert_eq!(tr("samp_rate / decim"), Ok("$samp_rate / $decim".into()));
        assert_eq!(
            tr("samp_rate/(2*decim)"),
            Ok("$samp_rate / (2 * $decim)".into())
        );
        assert_eq!(
            tr("(samp_rate/2)/decim"),
            Ok("$samp_rate / 2 / $decim".into())
        );
        assert_eq!(
            tr("samp_rate - (decim - 1)"),
            Ok("$samp_rate - ($decim - 1)".into())
        );
        assert_eq!(tr("(samp_rate + 1) * 2"), Ok("($samp_rate + 1) * 2".into()));
        assert_eq!(tr("-samp_rate"), Ok("-$samp_rate".into()));
        assert_eq!(tr("-(samp_rate + 1)"), Ok("-($samp_rate + 1)".into()));
        assert_eq!(tr("100.3e6 - -1"), Ok("100.3e6 - -1".into()));
        assert_eq!(tr("int(samp_rate/decim)"), Ok("$samp_rate / $decim".into()));
        assert_eq!(tr("float(fm_dev)"), Ok("$fm_dev".into()));
        assert_eq!(tr("0x10 * 2"), Ok("16 * 2".into()));
        assert_eq!(tr("samp_rate // decim"), Ok("$samp_rate / $decim".into()));
        assert_eq!(tr("2*math.pi"), Ok("2 * 3.141592653589793".into()));
        assert!(tr("samp_rate ** 2").is_err());
        assert!(tr("samp_rate % 2").is_err());
        assert!(tr("unknown_var * 2").unwrap_err().contains("unknown_var"));
        assert!(tr("'rtl=0'").is_err());
        assert!(tr("firdes.low_pass(1, 2, 3, 4)").is_err());
        assert!(tr("round(samp_rate)").is_err());
    }

    #[test]
    fn translations_evaluate_like_python() {
        // Every translation must read back in foxygnal with the Python value.
        for src in [
            "2.4e6",
            "samp_rate / decim",
            "samp_rate/(2*decim)",
            "(samp_rate/2)/decim",
            "samp_rate - (decim - 1) * 3",
            "-(samp_rate + 1) / -2",
            "1e3 - -2",
            "2*math.pi*fm_dev",
            "100_000 / 4",
        ] {
            let py = ev(src).unwrap().as_f64().unwrap();
            let fx = foxygnal_core::expr::eval(&tr(src).unwrap(), &mut |n| {
                vars(n)
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| n.to_string())
            })
            .unwrap();
            assert!((py - fx).abs() <= 1e-9 * py.abs(), "{src}: {py} vs {fx}");
        }
    }
}
