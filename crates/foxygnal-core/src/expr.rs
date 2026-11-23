//! A small, safe arithmetic language for parameter values and graph
//! variables: `"$samp_rate / 10"`, `"2.4M"`, `"-($freq - 100k) * 2"`.
//!
//! Numbers (with an optional SI prefix and unit word, as the UI accepts
//! them), `$name` variable references, `+ - * /`, unary minus and
//! parentheses. Nothing else: no functions, no code.

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Var(String),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

/// SI prefixes, matching `ui/src/lib/si.ts`.
fn prefix(c: char) -> Option<f64> {
    Some(match c {
        'p' => 1e-12,
        'n' => 1e-9,
        'u' | 'µ' => 1e-6,
        'm' => 1e-3,
        'k' | 'K' => 1e3,
        'M' => 1e6,
        'G' => 1e9,
        'T' => 1e12,
        _ => return None,
    })
}

const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.src[self.pos..]
    }

    fn skip_ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.pos = self.src.len() - trimmed.len();
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.rest().chars().next()
    }

    fn bump(&mut self, c: char) {
        self.pos += c.len_utf8();
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.term()?;
        while let Some(c @ ('+' | '-')) = self.peek() {
            self.bump(c);
            let rhs = self.term()?;
            let op = if c == '+' { Op::Add } else { Op::Sub };
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some(c @ ('*' | '/')) = self.peek() {
            self.bump(c);
            let rhs = self.unary()?;
            let op = if c == '*' { Op::Mul } else { Op::Div };
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("expression is nested too deeply".into());
        }
        let r = match self.peek() {
            Some('-') => {
                self.bump('-');
                self.unary().map(|e| Expr::Neg(Box::new(e)))
            }
            Some('+') => {
                self.bump('+');
                self.unary()
            }
            _ => self.atom(),
        };
        self.depth -= 1;
        r
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.peek() {
            None => Err("expected a value, found the end".into()),
            Some('(') => {
                self.bump('(');
                let e = self.expr()?;
                match self.peek() {
                    Some(')') => {
                        self.bump(')');
                        Ok(e)
                    }
                    _ => Err("missing `)`".into()),
                }
            }
            Some('$') => {
                self.bump('$');
                let name: String = self
                    .rest()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
                    return Err("`$` must be followed by a variable name".into());
                }
                self.pos += name.len();
                Ok(Expr::Var(name))
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(c) if c.is_alphabetic() || c == '_' => {
                let word: String = self
                    .rest()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                Err(format!(
                    "`{word}` isn't a number; variables are written `${word}`"
                ))
            }
            Some(c) => Err(format!("unexpected `{c}`")),
        }
    }

    fn number(&mut self) -> Result<Expr, String> {
        let rest = self.rest();
        let b = rest.as_bytes();
        let mut i = 0;
        let digits = |i: &mut usize| {
            while *i < b.len() && (b[*i].is_ascii_digit() || b[*i] == b'_') {
                *i += 1;
            }
        };
        digits(&mut i);
        if i < b.len() && b[i] == b'.' {
            i += 1;
            digits(&mut i);
        }
        // Exponent, only if digits follow (`2e3`, but `2e` is not one).
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            let mut j = i + 1;
            if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                j += 1;
            }
            if j < b.len() && b[j].is_ascii_digit() {
                i = j;
                digits(&mut i);
            }
        }
        let text = rest[..i].replace('_', "");
        let mut v: f64 = text
            .parse()
            .map_err(|_| format!("`{}` isn't a number", &rest[..i]))?;
        self.pos += i;
        // An optional SI prefix and unit word: `2.4M`, `100 kHz`, `-30 dB`.
        self.skip_ws();
        // Letters, and `/` between letters (`kS/s`, but not `k/2`).
        let rest = self.rest();
        let mut word_len = 0;
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let next_is_letter = chars.peek().is_some_and(|(_, n)| n.is_alphabetic());
            if c.is_alphabetic() || (c == '/' && word_len > 0 && next_is_letter) {
                word_len = i + c.len_utf8();
            } else {
                break;
            }
        }
        let word = rest[..word_len].to_owned();
        if !word.is_empty() {
            let mut chars = word.chars();
            let first = chars.next().expect("not empty");
            if let Some(p) = prefix(first) {
                // A lone prefix, or a prefix followed by a unit (`kHz`).
                v *= p;
            }
            // Else a bare unit we don't scale (`dB`, `Hz`).
            self.pos += word.len();
        }
        Ok(Expr::Num(v))
    }
}

impl Expr {
    pub fn parse(src: &str) -> Result<Expr, String> {
        let mut p = Parser {
            src,
            pos: 0,
            depth: 0,
        };
        let e = p.expr()?;
        match p.peek() {
            None => Ok(e),
            Some(')') => Err("unmatched `)`".into()),
            Some(c) => Err(format!("unexpected `{c}`")),
        }
    }

    /// Evaluate with `var` looking up `$name` references.
    pub fn eval(&self, var: &mut dyn FnMut(&str) -> Result<f64, String>) -> Result<f64, String> {
        let v = match self {
            Expr::Num(n) => *n,
            Expr::Var(name) => var(name)?,
            Expr::Neg(e) => -e.eval(var)?,
            Expr::Bin(op, a, b) => {
                let (a, b) = (a.eval(var)?, b.eval(var)?);
                match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div if b == 0.0 => return Err("division by zero".into()),
                    Op::Div => a / b,
                }
            }
        };
        if v.is_finite() {
            Ok(v)
        } else {
            Err("the result is too large".into())
        }
    }

    /// The variables this expression refers to, in order of appearance.
    pub fn vars(&self) -> Vec<&str> {
        fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a str>) {
            match e {
                Expr::Num(_) => {}
                Expr::Var(v) => out.push(v),
                Expr::Neg(e) => walk(e, out),
                Expr::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out
    }
}

/// Parse and evaluate in one go.
pub fn eval(src: &str, var: &mut dyn FnMut(&str) -> Result<f64, String>) -> Result<f64, String> {
    Expr::parse(src)?.eval(var)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> Result<f64, String> {
        eval(s, &mut |name| match name {
            "samp_rate" => Ok(2.4e6),
            "a" => Ok(3.0),
            _ => Err(format!("unknown variable `${name}`")),
        })
    }

    #[test]
    fn arithmetic() {
        assert_eq!(ev("1 + 2 * 3"), Ok(7.0));
        assert_eq!(ev("(1 + 2) * 3"), Ok(9.0));
        assert_eq!(ev("10 / 4 - 1"), Ok(1.5));
        assert_eq!(ev("8 / 2 / 2"), Ok(2.0));
        assert_eq!(ev("1 - 2 - 3"), Ok(-4.0));
        assert_eq!(ev("-3 * -2"), Ok(6.0));
        assert_eq!(ev("-(1 + 1)"), Ok(-2.0));
        assert_eq!(ev("--1"), Ok(1.0));
        assert_eq!(ev("  42  "), Ok(42.0));
        assert_eq!(ev("1e3 + .5 + 2.e1"), Ok(1020.5));
        assert_eq!(ev("1_000_000"), Ok(1e6));
    }

    #[test]
    fn si_prefixes_and_units() {
        assert_eq!(ev("2.4M"), Ok(2.4e6));
        assert_eq!(ev("100k"), Ok(1e5));
        assert_eq!(ev("100 kHz"), Ok(1e5));
        assert_eq!(ev("1.5 ms"), Ok(1.5e-3));
        assert_eq!(ev("-30dB"), Ok(-30.0));
        assert_eq!(ev("48 kS/s"), Ok(48e3));
        assert_eq!(ev("2.4M / 10"), Ok(2.4e5));
        assert_eq!(ev("1e3k"), Ok(1e6));
        assert_eq!(ev("10 / 4"), Ok(2.5));
        assert_eq!(ev("100k/2"), Ok(5e4));
        assert_eq!(ev("10 m / 2"), Ok(5e-3));
    }

    #[test]
    fn variables() {
        assert_eq!(ev("$samp_rate / 10"), Ok(2.4e5));
        assert_eq!(ev("$a*$a"), Ok(9.0));
        assert_eq!(ev("-$a"), Ok(-3.0));
        assert_eq!(ev("$nope"), Err("unknown variable `$nope`".into()));
        let e = Expr::parse("$a + ($b * 2) - $a").unwrap();
        assert_eq!(e.vars(), ["a", "b", "a"]);
    }

    #[test]
    fn errors() {
        assert!(ev("").unwrap_err().contains("end"));
        assert!(ev("1 +").unwrap_err().contains("end"));
        assert_eq!(ev("(1 + 2"), Err("missing `)`".into()));
        assert_eq!(ev("1 + 2)"), Err("unmatched `)`".into()));
        assert_eq!(ev("1 / 0"), Err("division by zero".into()));
        assert_eq!(ev("1 / (2 - 2)"), Err("division by zero".into()));
        assert_eq!(ev("2 ^ 3"), Err("unexpected `^`".into()));
        assert_eq!(
            ev("$"),
            Err("`$` must be followed by a variable name".into())
        );
        assert_eq!(
            ev("$1"),
            Err("`$` must be followed by a variable name".into())
        );
        assert!(ev("samp_rate").unwrap_err().contains("`$samp_rate`"));
        assert_eq!(ev("1 2"), Err("unexpected `2`".into()));
        assert!(ev("1e308 * 10").is_err());
        // No way to run code.
        assert!(ev("std::process::exit(1)").is_err());
        assert!(ev(&"(".repeat(1000)).is_err());
    }
}
