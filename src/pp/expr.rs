//! `#if` / `#elif` constant-expression evaluation.
//!
//! Input is the already macro-expanded token list (`defined` and
//! `__has_include` are resolved during expansion). Semantics follow C/C++
//! `intmax_t`/`uintmax_t` arithmetic, including unsigned promotion in mixed
//! comparisons, but are tolerant: unknown identifiers are `0`, division by
//! zero yields `0`, and a syntax error makes the whole condition false.
//! Short-circuit (`&&`, `||`, `?:`) operands that are not selected are parsed
//! but never trip errors such as division by zero.

use super::expand::{ExpTok, Texts, F_SPACE};
use crate::lexer::TokenKind as K;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EvalError {
    Syntax,
    DivByZero,
}

#[derive(Clone, Copy, Debug)]
struct V {
    v: i64,
    u: bool,
}

impl V {
    const ZERO: V = V { v: 0, u: false };

    fn int(v: i64) -> V {
        V { v, u: false }
    }
    fn truthy(self) -> bool {
        self.v != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Xor,
    Or,
    LAnd,
    LOr,
}

impl Op {
    fn prec(self) -> u8 {
        match self {
            Op::Mul | Op::Div | Op::Rem => 10,
            Op::Add | Op::Sub => 9,
            Op::Shl | Op::Shr => 8,
            Op::Lt | Op::Le | Op::Gt | Op::Ge => 7,
            Op::Eq | Op::Ne => 6,
            Op::And => 5,
            Op::Xor => 4,
            Op::Or => 3,
            Op::LAnd => 2,
            Op::LOr => 1,
        }
    }
}

struct Parser<'a> {
    t: &'a [ExpTok],
    i: usize,
    x: Texts<'a>,
    err: Option<EvalError>,
    depth: u32,
}

/// Evaluates a directive condition. An empty expression is a syntax error.
pub(super) fn eval(toks: &[ExpTok], x: Texts<'_>) -> Result<bool, EvalError> {
    let mut p = Parser { t: toks, i: 0, x, err: None, depth: 0 };
    let v = p.comma(false);
    if p.err.is_none() && p.i != toks.len() {
        p.err = Some(EvalError::Syntax);
    }
    match p.err {
        Some(e) => Err(e),
        None => Ok(v.truthy()),
    }
}

impl Parser<'_> {
    fn fail(&mut self, e: EvalError) -> V {
        self.err.get_or_insert(e);
        V::ZERO
    }

    fn peek(&self) -> Option<&ExpTok> {
        self.t.get(self.i)
    }

    fn peek_kind(&self) -> Option<K> {
        self.peek().map(|t| t.kind)
    }

    fn comma(&mut self, skip: bool) -> V {
        let mut v = self.ternary(skip);
        while self.peek_kind() == Some(K::Comma) {
            self.i += 1;
            v = self.ternary(skip);
        }
        v
    }

    fn ternary(&mut self, skip: bool) -> V {
        let c = self.binary(1, skip);
        if self.peek_kind() != Some(K::Question) {
            return c;
        }
        self.i += 1;
        if self.depth >= 128 {
            return self.fail(EvalError::Syntax);
        }
        self.depth += 1;
        let a = self.comma(skip || !c.truthy());
        if self.peek_kind() != Some(K::Colon) {
            self.depth -= 1;
            return self.fail(EvalError::Syntax);
        }
        self.i += 1;
        let b = self.ternary(skip || c.truthy());
        self.depth -= 1;
        let u = a.u || b.u;
        let pick = if c.truthy() { a } else { b };
        V { v: pick.v, u }
    }

    /// Operator at the cursor and how many tokens it spans.
    fn peek_op(&self) -> Option<(Op, usize)> {
        let t = self.peek()?;
        let next = self.t.get(self.i + 1);
        let adjacent = |k: K| next.is_some_and(|n| n.kind == k && n.flags & F_SPACE == 0);
        Some(match t.kind {
            K::Star => (Op::Mul, 1),
            K::Slash => (Op::Div, 1),
            K::Percent => (Op::Rem, 1),
            K::Plus => (Op::Add, 1),
            K::Minus => (Op::Sub, 1),
            K::Shl => (Op::Shl, 1),
            // The lexer never glues `>`, so `>>` and `>=` are re-assembled here.
            K::Gt if adjacent(K::Gt) => (Op::Shr, 2),
            K::Gt if adjacent(K::Eq) => (Op::Ge, 2),
            K::Gt => (Op::Gt, 1),
            K::Lt => (Op::Lt, 1),
            K::Le => (Op::Le, 1),
            K::EqEq => (Op::Eq, 1),
            K::NotEq => (Op::Ne, 1),
            K::Amp => (Op::And, 1),
            K::Caret => (Op::Xor, 1),
            K::Pipe => (Op::Or, 1),
            K::AmpAmp => (Op::LAnd, 1),
            K::PipePipe => (Op::LOr, 1),
            _ => return None,
        })
    }

    fn binary(&mut self, min: u8, skip: bool) -> V {
        let mut lhs = self.unary(skip);
        while let Some((op, n)) = self.peek_op() {
            if op.prec() < min || self.err.is_some() {
                break;
            }
            self.i += n;
            lhs = match op {
                Op::LAnd => {
                    let r = self.binary(op.prec() + 1, skip || !lhs.truthy());
                    V::int((lhs.truthy() && r.truthy()) as i64)
                }
                Op::LOr => {
                    let r = self.binary(op.prec() + 1, skip || lhs.truthy());
                    V::int((lhs.truthy() || r.truthy()) as i64)
                }
                _ => {
                    let r = self.binary(op.prec() + 1, skip);
                    if skip { V::ZERO } else { self.apply(op, lhs, r) }
                }
            };
        }
        lhs
    }

    fn apply(&mut self, op: Op, a: V, b: V) -> V {
        let u = a.u || b.u;
        let (x, y) = (a.v, b.v);
        let cmp = |s: bool| V::int(s as i64);
        match op {
            Op::Add => V { v: x.wrapping_add(y), u },
            Op::Sub => V { v: x.wrapping_sub(y), u },
            Op::Mul => V { v: x.wrapping_mul(y), u },
            Op::Div | Op::Rem => {
                if y == 0 {
                    return self.fail(EvalError::DivByZero);
                }
                let v = match (u, op == Op::Div) {
                    (true, true) => ((x as u64) / (y as u64)) as i64,
                    (true, false) => ((x as u64) % (y as u64)) as i64,
                    (false, true) => x.wrapping_div(y),
                    (false, false) => x.wrapping_rem(y),
                };
                V { v, u }
            }
            Op::Shl => V { v: shift(x, y, true, a.u), u: a.u },
            Op::Shr => V { v: shift(x, y, false, a.u), u: a.u },
            Op::Lt => cmp(if u { (x as u64) < (y as u64) } else { x < y }),
            Op::Le => cmp(if u { (x as u64) <= (y as u64) } else { x <= y }),
            Op::Gt => cmp(if u { (x as u64) > (y as u64) } else { x > y }),
            Op::Ge => cmp(if u { (x as u64) >= (y as u64) } else { x >= y }),
            Op::Eq => cmp(x == y),
            Op::Ne => cmp(x != y),
            Op::And => V { v: x & y, u },
            Op::Xor => V { v: x ^ y, u },
            Op::Or => V { v: x | y, u },
            Op::LAnd | Op::LOr => unreachable!(),
        }
    }

    fn unary(&mut self, skip: bool) -> V {
        if self.depth >= 128 {
            return self.fail(EvalError::Syntax);
        }
        self.depth += 1;
        let v = self.unary_inner(skip);
        self.depth -= 1;
        v
    }

    fn unary_inner(&mut self, skip: bool) -> V {
        let Some(&t) = self.peek() else { return self.fail(EvalError::Syntax) };
        self.i += 1;
        match t.kind {
            K::Plus => self.unary(skip),
            K::Minus => {
                let v = self.unary(skip);
                V { v: v.v.wrapping_neg(), u: v.u }
            }
            K::Tilde => {
                let v = self.unary(skip);
                V { v: !v.v, u: v.u }
            }
            K::Bang => {
                let v = self.unary(skip);
                V::int(!v.truthy() as i64)
            }
            K::LParen => {
                let v = self.comma(skip);
                if self.peek_kind() == Some(K::RParen) {
                    self.i += 1;
                    v
                } else {
                    self.fail(EvalError::Syntax)
                }
            }
            K::Number => match parse_int(self.x.get(&t)) {
                Some(v) => v,
                None => self.fail(EvalError::Syntax),
            },
            K::Char => parse_char(self.x.get(&t)),
            K::Ident | K::Keyword => V::int((self.x.get(&t) == "true") as i64),
            _ => self.fail(EvalError::Syntax),
        }
    }
}

fn shift(x: i64, n: i64, left: bool, unsigned: bool) -> i64 {
    // A negative count shifts the other way, like GCC; oversized counts saturate.
    let (left, n) = if n < 0 { (!left, n.unsigned_abs()) } else { (left, n as u64) };
    if left {
        if n >= 64 { 0 } else { x.wrapping_shl(n as u32) }
    } else if unsigned {
        if n >= 64 { 0 } else { ((x as u64) >> n) as i64 }
    } else if n >= 64 {
        if x < 0 { -1 } else { 0 }
    } else {
        x >> n
    }
}

fn parse_int(s: &str) -> Option<V> {
    let b = s.as_bytes();
    let (radix, digits) = match b {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', b'b' | b'B', rest @ ..] => (2, rest),
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, b),
    };
    let mut v: u64 = 0;
    let mut unsigned = false;
    let mut seen = false;
    for &c in digits {
        match c {
            b'\'' => continue,
            b'u' | b'U' => unsigned = true,
            b'l' | b'L' | b'z' | b'Z' => {}
            _ => {
                let d = (c as char).to_digit(radix)?;
                v = v.wrapping_mul(radix as u64).wrapping_add(d as u64);
                seen = true;
            }
        }
    }
    if !seen && radix != 8 {
        return None;
    }
    Some(V { v: v as i64, u: unsigned || v > i64::MAX as u64 })
}

fn parse_char(s: &str) -> V {
    let Some(q) = s.find('\'') else { return V::ZERO };
    let prefix = &s[..q];
    let body = s[q + 1..].strip_suffix('\'').unwrap_or(&s[q + 1..]);
    let mut vals: [i64; 8] = [0; 8];
    let mut n = 0;
    let mut it = body.chars().peekable();
    while let Some(c) = it.next() {
        let v = if c != '\\' {
            c as i64
        } else {
            match it.next() {
                Some('n') => 10,
                Some('t') => 9,
                Some('r') => 13,
                Some('a') => 7,
                Some('b') => 8,
                Some('f') => 12,
                Some('v') => 11,
                Some('e') => 27,
                Some('x') => {
                    let mut v = 0i64;
                    while let Some(d) = it.peek().and_then(|c| c.to_digit(16)) {
                        v = v.wrapping_mul(16).wrapping_add(d as i64);
                        it.next();
                    }
                    v
                }
                Some(d @ '0'..='7') => {
                    let mut v = d.to_digit(8).unwrap() as i64;
                    for _ in 0..2 {
                        match it.peek().and_then(|c| c.to_digit(8)) {
                            Some(d) => {
                                v = v * 8 + d as i64;
                                it.next();
                            }
                            None => break,
                        }
                    }
                    v
                }
                Some(o) => o as i64,
                None => b'\\' as i64,
            }
        };
        if n < vals.len() {
            vals[n] = v;
            n += 1;
        }
    }
    match n {
        0 => V::ZERO,
        1 if prefix.is_empty() => V::int(vals[0] as i8 as i64),
        1 => V::int(vals[0]),
        _ => {
            let mut v: i64 = 0;
            for &c in &vals[..n] {
                v = (v << 8) | (c & 0xFF);
            }
            V::int(v as i32 as i64)
        }
    }
}
