//! Constant folding for hover: the value of a `const`/`constexpr` variable,
//! an enumerator or an object-like macro. The evaluator works on lexed tokens
//! of the declaring file and asks the caller what each identifier means, so
//! names resolve with the same scope rules as go-to-definition.
//!
//! Tolerant by design: integers are exact (`i128`, no 32-bit wrap-around),
//! anything it does not understand (calls, templates, `sizeof(expr)`) is
//! simply unknown.

use crate::lexer::{Token, TokenKind as K};
use crate::symbols::{SymbolId, SymbolKind};

use super::{Query, sig_tokens};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Val {
    Int(i128),
    Float(f64),
    Bool(bool),
}

impl Val {
    fn truthy(self) -> bool {
        match self {
            Val::Int(n) => n != 0,
            Val::Float(f) => f != 0.0,
            Val::Bool(b) => b,
        }
    }

    fn int(self) -> Option<i128> {
        match self {
            Val::Int(n) => Some(n),
            Val::Bool(b) => Some(b as i128),
            Val::Float(_) => None,
        }
    }

    fn float(self) -> f64 {
        match self {
            Val::Int(n) => n as f64,
            Val::Float(f) => f,
            Val::Bool(b) => b as u8 as f64,
        }
    }

    /// `47 (0x2F)`, `true`, `1.5`.
    pub fn display(self) -> String {
        match self {
            Val::Int(n) if n >= 16 => format!("{n} (0x{n:X})"),
            Val::Int(n) => n.to_string(),
            Val::Bool(b) => b.to_string(),
            Val::Float(f) => format!("{f:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ty {
    Bool,
    Float(bool),
    Int { bits: u32, signed: bool },
}

fn type_of(words: &[&str]) -> Option<Ty> {
    let int = |bits, signed| Some(Ty::Int { bits, signed });
    if let [w] = words {
        return match *w {
            "bool" => Some(Ty::Bool),
            "float" => Some(Ty::Float(true)),
            "double" => Some(Ty::Float(false)),
            "char" | "int8_t" | "__int8" => int(8, true),
            "uint8_t" | "char8_t" => int(8, false),
            "short" | "int16_t" | "__int16" => int(16, true),
            "uint16_t" | "char16_t" | "wchar_t" => int(16, false),
            "int" | "long" | "signed" | "int32_t" | "__int32" => int(32, true),
            "unsigned" | "uint32_t" | "char32_t" => int(32, false),
            "int64_t" | "__int64" | "ssize_t" | "ptrdiff_t" | "intptr_t" | "intmax_t" => int(64, true),
            "uint64_t" | "size_t" | "uintptr_t" | "uintmax_t" => int(64, false),
            _ => None,
        };
    }
    let ok = !words.is_empty() && words.iter().all(|w| matches!(*w, "unsigned" | "signed" | "long" | "short" | "char" | "int" | "double"));
    if !ok {
        return None;
    }
    let has = |w: &str| words.contains(&w);
    if has("double") {
        return Some(Ty::Float(false));
    }
    let longs = words.iter().filter(|w| **w == "long").count();
    let bits = if has("char") {
        8
    } else if has("short") {
        16
    } else if longs >= 2 {
        64
    } else {
        32
    };
    int(bits, !has("unsigned"))
}

fn cast(v: Val, t: Ty) -> Option<Val> {
    Some(match t {
        Ty::Bool => Val::Bool(v.truthy()),
        Ty::Float(single) => {
            let f = v.float();
            Val::Float(if single { f as f32 as f64 } else { f })
        }
        Ty::Int { bits, signed } => {
            let n = match v {
                Val::Float(f) if f.is_finite() => f as i128,
                Val::Float(_) => return None,
                v => v.int()?,
            };
            let m = 1i128 << bits;
            let mut r = n.rem_euclid(m);
            if signed && r >= m / 2 {
                r -= m;
            }
            Val::Int(r)
        }
    })
}

fn number(s: &str) -> Option<Val> {
    let s: String = s.chars().filter(|&c| c != '\'').collect();
    let lower = s.to_ascii_lowercase();
    let int_suffix = |x: &str| x.trim_end_matches(['u', 'l', 'z']).to_owned();
    if let Some(h) = lower.strip_prefix("0x") {
        if h.contains('p') {
            return None;
        }
        return i128::from_str_radix(&int_suffix(h), 16).ok().map(Val::Int);
    }
    if let Some(b) = lower.strip_prefix("0b") {
        return i128::from_str_radix(&int_suffix(b), 2).ok().map(Val::Int);
    }
    if lower.contains('.') || lower.contains('e') || lower.ends_with('f') {
        return lower.trim_end_matches(['f', 'l']).parse::<f64>().ok().map(Val::Float);
    }
    let d = int_suffix(&lower);
    if d.len() > 1 && d.starts_with('0') {
        return i128::from_str_radix(&d[1..], 8).ok().map(Val::Int);
    }
    d.parse::<i128>().ok().map(Val::Int)
}

fn char_lit(s: &str) -> Option<Val> {
    let body = s.get(s.find('\'')? + 1..s.rfind('\'')?)?;
    let mut it = body.chars();
    let c = it.next()?;
    if c != '\\' {
        return it.next().is_none().then_some(Val::Int(c as i128));
    }
    let rest = it.as_str();
    let v = match rest.chars().next()? {
        'n' => 10,
        't' => 9,
        'r' => 13,
        'a' => 7,
        'b' => 8,
        'f' => 12,
        'v' => 11,
        '\\' => 92,
        '\'' => 39,
        '"' => 34,
        'x' => i128::from_str_radix(&rest[1..], 16).ok()?,
        d if d.is_digit(8) => i128::from_str_radix(rest, 8).ok()?,
        _ => return None,
    };
    Some(Val::Int(v))
}

#[derive(Clone, Copy, PartialEq)]
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

fn arith(op: Op, a: Val, b: Val) -> Option<Val> {
    if matches!(a, Val::Float(_)) || matches!(b, Val::Float(_)) {
        let (x, y) = (a.float(), b.float());
        return Some(match op {
            Op::Add => Val::Float(x + y),
            Op::Sub => Val::Float(x - y),
            Op::Mul => Val::Float(x * y),
            Op::Div if y != 0.0 => Val::Float(x / y),
            Op::Lt => Val::Bool(x < y),
            Op::Le => Val::Bool(x <= y),
            Op::Gt => Val::Bool(x > y),
            Op::Ge => Val::Bool(x >= y),
            Op::Eq => Val::Bool(x == y),
            Op::Ne => Val::Bool(x != y),
            _ => return None,
        });
    }
    let (x, y) = (a.int()?, b.int()?);
    Some(match op {
        Op::Add => Val::Int(x.checked_add(y)?),
        Op::Sub => Val::Int(x.checked_sub(y)?),
        Op::Mul => Val::Int(x.checked_mul(y)?),
        Op::Div if y != 0 => Val::Int(x.checked_div(y)?),
        Op::Rem if y != 0 => Val::Int(x.checked_rem(y)?),
        Op::Shl if (0..100).contains(&y) => Val::Int(x.checked_mul(1i128 << y)?),
        Op::Shr if (0..127).contains(&y) => Val::Int(x >> y),
        Op::And => Val::Int(x & y),
        Op::Or => Val::Int(x | y),
        Op::Xor => Val::Int(x ^ y),
        Op::Lt => Val::Bool(x < y),
        Op::Le => Val::Bool(x <= y),
        Op::Gt => Val::Bool(x > y),
        Op::Ge => Val::Bool(x >= y),
        Op::Eq => Val::Bool(x == y),
        Op::Ne => Val::Bool(x != y),
        _ => return None,
    })
}

struct Eval<'a> {
    toks: &'a [Token],
    text: &'a str,
    i: usize,
    end: usize,
    bad: bool,
    depth: u32,
    resolve: &'a mut dyn FnMut(usize) -> Option<Val>,
}

/// Evaluates `toks[range]` of `text`. `resolve` gets the index of the last
/// identifier of every (qualified) name and returns its value, if known.
pub fn eval(toks: &[Token], text: &str, range: std::ops::Range<usize>, resolve: &mut dyn FnMut(usize) -> Option<Val>) -> Option<Val> {
    if range.is_empty() || range.end > toks.len() {
        return None;
    }
    let mut e = Eval { toks, text, i: range.start, end: range.end, bad: false, depth: 0, resolve };
    let v = e.ternary();
    (!e.bad && e.i == e.end).then_some(v).flatten()
}

impl Eval<'_> {
    fn kind(&self, at: usize) -> Option<K> {
        (at < self.end).then(|| self.toks[at].kind)
    }

    fn word(&self, at: usize) -> Option<&str> {
        (at < self.end && matches!(self.toks[at].kind, K::Ident | K::Keyword)).then(|| self.toks[at].text(self.text))
    }

    fn adjacent(&self, at: usize) -> bool {
        at + 1 < self.end && self.toks[at].end() == self.toks[at + 1].start
    }

    fn binop(&self) -> Option<(u8, Op, usize)> {
        let k = self.kind(self.i)?;
        Some(match k {
            K::Star => (10, Op::Mul, 1),
            K::Slash => (10, Op::Div, 1),
            K::Percent => (10, Op::Rem, 1),
            K::Plus => (9, Op::Add, 1),
            K::Minus => (9, Op::Sub, 1),
            K::Shl => (8, Op::Shl, 1),
            K::Lt => (7, Op::Lt, 1),
            K::Le => (7, Op::Le, 1),
            K::Gt if self.kind(self.i + 1) == Some(K::Gt) && self.adjacent(self.i) => (8, Op::Shr, 2),
            K::Gt if self.kind(self.i + 1) == Some(K::Eq) && self.adjacent(self.i) => (7, Op::Ge, 2),
            K::Gt => (7, Op::Gt, 1),
            K::EqEq => (6, Op::Eq, 1),
            K::NotEq => (6, Op::Ne, 1),
            K::Amp => (5, Op::And, 1),
            K::Caret => (4, Op::Xor, 1),
            K::Pipe => (3, Op::Or, 1),
            K::AmpAmp => (2, Op::LAnd, 1),
            K::PipePipe => (1, Op::LOr, 1),
            _ => return None,
        })
    }

    fn ternary(&mut self) -> Option<Val> {
        let c = self.binary(1);
        if self.kind(self.i) != Some(K::Question) {
            return c;
        }
        self.i += 1;
        self.depth += 1;
        if self.depth > 64 {
            self.bad = true;
            return None;
        }
        let a = self.ternary();
        if self.kind(self.i) != Some(K::Colon) {
            self.bad = true;
            return None;
        }
        self.i += 1;
        let b = self.ternary();
        self.depth -= 1;
        c.and_then(|c| if c.truthy() { a } else { b })
    }

    fn binary(&mut self, min: u8) -> Option<Val> {
        let mut lhs = self.unary();
        while let Some((prec, op, n)) = self.binop() {
            if prec < min {
                break;
            }
            self.i += n;
            let rhs = self.binary(prec + 1);
            lhs = match op {
                Op::LAnd => match (lhs.map(Val::truthy), rhs.map(Val::truthy)) {
                    (Some(false), _) | (_, Some(false)) => Some(Val::Bool(false)),
                    (Some(true), Some(true)) => Some(Val::Bool(true)),
                    _ => None,
                },
                Op::LOr => match (lhs.map(Val::truthy), rhs.map(Val::truthy)) {
                    (Some(true), _) | (_, Some(true)) => Some(Val::Bool(true)),
                    (Some(false), Some(false)) => Some(Val::Bool(false)),
                    _ => None,
                },
                _ => lhs.zip(rhs).and_then(|(a, b)| arith(op, a, b)),
            };
        }
        lhs
    }

    fn unary(&mut self) -> Option<Val> {
        self.depth += 1;
        if self.depth > 64 {
            self.bad = true;
            return None;
        }
        let v = self.unary_inner();
        self.depth -= 1;
        v
    }

    fn unary_inner(&mut self) -> Option<Val> {
        let Some(k) = self.kind(self.i) else {
            self.bad = true;
            return None;
        };
        match k {
            K::Minus | K::Plus | K::Bang | K::Tilde => {
                self.i += 1;
                let v = self.unary()?;
                match k {
                    K::Minus => match v {
                        Val::Float(f) => Some(Val::Float(-f)),
                        v => v.int()?.checked_neg().map(Val::Int),
                    },
                    K::Plus => Some(v),
                    K::Bang => Some(Val::Bool(!v.truthy())),
                    _ => Some(Val::Int(!v.int()?)),
                }
            }
            K::LParen => self.paren(),
            K::Number => {
                let v = number(self.toks[self.i].text(self.text));
                self.i += 1;
                v
            }
            K::Char => {
                let v = char_lit(self.toks[self.i].text(self.text));
                self.i += 1;
                v
            }
            K::Ident | K::Keyword | K::ColonColon => self.name(),
            _ => {
                self.bad = true;
                None
            }
        }
    }

    /// Index of the `)` matching the `(` at `open`.
    fn close_of(&self, open: usize) -> Option<usize> {
        let mut d = 0;
        for j in open..self.end {
            match self.toks[j].kind {
                K::LParen => d += 1,
                K::RParen => {
                    d -= 1;
                    if d == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The type named by `toks[a..b]` and whether it is a pointer.
    fn type_in(&self, a: usize, b: usize) -> Option<(Option<Ty>, bool)> {
        let mut words = Vec::new();
        let mut ptr = false;
        for j in a..b {
            match self.toks[j].kind {
                K::Ident | K::Keyword => {
                    let w = self.toks[j].text(self.text);
                    if !matches!(w, "std" | "const" | "volatile" | "constexpr") {
                        words.push(w);
                    }
                }
                K::ColonColon => {}
                K::Star => ptr = true,
                _ => return None,
            }
        }
        if words.is_empty() {
            return None;
        }
        let t = type_of(&words);
        (t.is_some() || ptr).then_some((t, ptr))
    }

    fn paren(&mut self) -> Option<Val> {
        let open = self.i;
        let Some(close) = self.close_of(open) else {
            self.bad = true;
            return None;
        };
        let starts_operand = matches!(self.kind(close + 1), Some(K::Number | K::Char | K::Ident | K::Keyword | K::LParen | K::Minus | K::Plus | K::Bang | K::Tilde));
        if starts_operand && let Some((Some(t), false)) = self.type_in(open + 1, close) {
            self.i = close + 1;
            let v = self.unary()?;
            return cast(v, t);
        }
        self.i = open + 1;
        let v = self.ternary();
        if self.i != close {
            self.bad = true;
            return None;
        }
        self.i = close + 1;
        v
    }

    /// Skips a balanced `(...)` or `{...}` starting at `self.i`.
    fn skip_group(&mut self) {
        let mut d = 0;
        while self.i < self.end {
            match self.toks[self.i].kind {
                K::LParen | K::LBrace | K::LBracket => d += 1,
                K::RParen | K::RBrace | K::RBracket => d -= 1,
                _ => {}
            }
            self.i += 1;
            if d <= 0 {
                break;
            }
        }
    }

    fn name(&mut self) -> Option<Val> {
        let first = self.i;
        match self.word(first) {
            Some("true") => {
                self.i += 1;
                return Some(Val::Bool(true));
            }
            Some("false") => {
                self.i += 1;
                return Some(Val::Bool(false));
            }
            Some("sizeof") if self.kind(first + 1) == Some(K::LParen) => {
                let close = self.close_of(first + 1);
                self.i = close.map_or(self.end, |c| c + 1);
                let close = close?;
                return match self.type_in(first + 2, close)? {
                    (_, true) => Some(Val::Int(8)),
                    (Some(Ty::Bool), _) => Some(Val::Int(1)),
                    (Some(Ty::Float(single)), _) => Some(Val::Int(if single { 4 } else { 8 })),
                    (Some(Ty::Int { bits, .. }), _) => Some(Val::Int(bits as i128 / 8)),
                    _ => None,
                };
            }
            Some("static_cast") if self.kind(first + 1) == Some(K::Lt) => {
                let gt = (first + 2..self.end).find(|&j| self.toks[j].kind == K::Gt);
                let Some(gt) = gt.filter(|&g| self.kind(g + 1) == Some(K::LParen)) else {
                    self.bad = true;
                    return None;
                };
                let ty = self.type_in(first + 2, gt).and_then(|(t, p)| if p { None } else { t });
                let close = self.close_of(gt + 1);
                let Some(close) = close else {
                    self.bad = true;
                    return None;
                };
                self.i = gt + 2;
                let v = self.ternary();
                if self.i != close {
                    self.bad = true;
                    return None;
                }
                self.i = close + 1;
                return cast(v?, ty?);
            }
            _ => {}
        }

        // Functional cast: `uint32_t(5)`, `float{1}`.
        if let Some(w) = self.word(first)
            && let Some(t) = type_of(&[w])
            && matches!(self.kind(first + 1), Some(K::LParen | K::LBrace))
        {
            let open = first + 1;
            let braces = self.toks[open].kind == K::LBrace;
            let close = if braces { (open..self.end).find(|&j| self.toks[j].kind == K::RBrace) } else { self.close_of(open) };
            let Some(close) = close else {
                self.bad = true;
                return None;
            };
            self.i = open + 1;
            let v = self.ternary();
            if self.i != close {
                self.bad = true;
                return None;
            }
            self.i = close + 1;
            return cast(v?, t);
        }

        let mut last = first;
        if self.kind(last) == Some(K::ColonColon) {
            last += 1;
        }
        if !matches!(self.kind(last), Some(K::Ident | K::Keyword)) {
            self.bad = true;
            return None;
        }
        while self.kind(last + 1) == Some(K::ColonColon) && matches!(self.kind(last + 2), Some(K::Ident | K::Keyword)) {
            last += 2;
        }
        self.i = last + 1;
        if matches!(self.kind(self.i), Some(K::LParen | K::LBrace)) {
            self.skip_group();
            return None;
        }
        if self.kind(self.i) == Some(K::Lt) && self.kind(last).is_some() && self.template_args_follow(self.i) {
            return None;
        }
        (self.resolve)(last)
    }

    /// True for `name<...>::x` / `name<...>(`: consumes the template arguments.
    fn template_args_follow(&mut self, lt: usize) -> bool {
        let mut d = 0;
        for j in lt..self.end {
            match self.toks[j].kind {
                K::Lt => d += 1,
                K::Gt => {
                    d -= 1;
                    if d == 0 {
                        let next = self.kind(j + 1);
                        if matches!(next, Some(K::ColonColon | K::LParen | K::LBrace)) {
                            self.i = j + 1;
                            while self.i < self.end && self.kind(self.i) == Some(K::ColonColon) {
                                self.i += 2;
                            }
                            if matches!(self.kind(self.i), Some(K::LParen | K::LBrace)) {
                                self.skip_group();
                            }
                            return true;
                        }
                        return false;
                    }
                }
                K::Semi | K::LBrace | K::RBrace => return false,
                _ => {}
            }
        }
        false
    }
}

impl Query<'_> {
    /// Value of the `const`/`constexpr` variable, enumerator or object-like
    /// macro `id`, when its initializer is a constant expression.
    pub fn const_value(&self, id: SymbolId) -> Option<Val> {
        self.const_value_in(id, &mut Vec::new())
    }

    fn const_value_in(&self, id: SymbolId, seen: &mut Vec<SymbolId>) -> Option<Val> {
        if seen.len() >= 24 || seen.contains(&id) {
            return None;
        }
        seen.push(id);
        let v = self.const_value_inner(id, seen);
        seen.pop();
        v
    }

    fn const_value_inner(&self, id: SymbolId, seen: &mut Vec<SymbolId>) -> Option<Val> {
        let db = &self.idx.db;
        let s = db.symbol(id);
        if !matches!(s.kind, SymbolKind::Macro | SymbolKind::Variable | SymbolKind::Field | SymbolKind::EnumConstant) {
            return None;
        }
        let path = db.files.path(s.file);
        let text = self.texts.text(path)?;
        let src = text.as_str();
        if matches!(s.kind, SymbolKind::Variable | SymbolKind::Field) {
            let head = src.get(s.extent_start as usize..s.name_offset as usize)?;
            if !head.contains("const") {
                return None;
            }
        }
        let toks = sig_tokens(src);
        let at = toks.partition_point(|t| t.start < s.name_offset);
        if toks.get(at)?.start != s.name_offset {
            return None;
        }
        let mut resolve = |k: usize| -> Option<Val> {
            let cands = self.definition_in(&toks, path, src, k);
            cands.into_iter().find_map(|c| self.const_value_in(c, seen))
        };
        match s.kind {
            SymbolKind::Macro => {
                let next = toks.get(at + 1)?;
                if next.kind == K::LParen && !next.space_before() && !next.at_line_start() {
                    return None;
                }
                let end = (at + 1..toks.len()).find(|&j| toks[j].at_line_start()).unwrap_or(toks.len());
                eval(&toks, src, at + 1..end, &mut resolve)
            }
            SymbolKind::EnumConstant => self.enum_value(id, &toks, src, &mut resolve),
            _ => {
                let range = initializer(&toks, at + 1)?;
                eval(&toks, src, range, &mut resolve)
            }
        }
    }

    /// Enumerators count up from the previous one unless they have an initializer.
    fn enum_value(&self, id: SymbolId, toks: &[Token], src: &str, resolve: &mut dyn FnMut(usize) -> Option<Val>) -> Option<Val> {
        let db = &self.idx.db;
        let target = db.symbol(id);
        let mut sibs: Vec<SymbolId> = db.children(target.parent?).iter().copied().filter(|&c| db.symbol(c).kind == SymbolKind::EnumConstant && db.symbol(c).file == target.file && db.symbol(c).name_offset <= target.name_offset).collect();
        sibs.sort_by_key(|&c| db.symbol(c).name_offset);
        let mut known: Vec<(&str, Val)> = Vec::new();
        let mut next = Some(Val::Int(0));
        let mut value = None;
        for c in sibs {
            let cs = db.symbol(c);
            let at = toks.partition_point(|t| t.start < cs.name_offset);
            if toks.get(at)?.start != cs.name_offset {
                return None;
            }
            let name = toks[at].text(src);
            value = if toks.get(at + 1).is_some_and(|t| t.kind == K::Eq) {
                let end = enumerator_end(toks, at + 2);
                let mut with_siblings = |k: usize| {
                    let plain = k == 0 || toks[k - 1].kind != K::ColonColon;
                    if plain && let Some(&(_, v)) = known.iter().rev().find(|(n, _)| *n == toks[k].text(src)) {
                        return Some(v);
                    }
                    resolve(k)
                };
                eval(toks, src, at + 2..end, &mut with_siblings)
            } else {
                next
            };
            next = match value {
                Some(Val::Int(n)) => Some(Val::Int(n + 1)),
                _ => None,
            };
            if let Some(v) = value {
                known.push((name, v));
            }
        }
        value
    }
}

/// Token range of the initializer after the declarator name: `= expr` or `{ expr }`.
fn initializer(toks: &[Token], from: usize) -> Option<std::ops::Range<usize>> {
    let t = toks.get(from)?;
    match t.kind {
        K::Eq => {
            let start = from + 1;
            let mut d = 0i32;
            for (j, t) in toks.iter().enumerate().skip(start) {
                match t.kind {
                    K::LParen | K::LBracket | K::LBrace => d += 1,
                    K::RParen | K::RBracket | K::RBrace => d -= 1,
                    K::Semi | K::Comma if d == 0 => return Some(start..j),
                    _ => {}
                }
                if d < 0 {
                    return Some(start..j);
                }
            }
            Some(start..toks.len())
        }
        K::LBrace => {
            let mut d = 0i32;
            for (j, t) in toks.iter().enumerate().skip(from) {
                match t.kind {
                    K::LBrace => d += 1,
                    K::RBrace => {
                        d -= 1;
                        if d == 0 {
                            return Some(from + 1..j);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

fn enumerator_end(toks: &[Token], start: usize) -> usize {
    let mut d = 0i32;
    for (j, t) in toks.iter().enumerate().skip(start) {
        match t.kind {
            K::LParen | K::LBracket | K::LBrace => d += 1,
            K::RParen | K::RBracket | K::RBrace => d -= 1,
            K::Comma if d == 0 => return j,
            _ => {}
        }
        if d < 0 {
            return j;
        }
    }
    toks.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str, names: &[(&str, Val)]) -> Option<Val> {
        let toks = sig_tokens(src);
        let mut resolve = |k: usize| names.iter().find(|(n, _)| *n == toks[k].text(src)).map(|&(_, v)| v);
        eval(&toks, src, 0..toks.len(), &mut resolve)
    }

    fn int(n: i128) -> Option<Val> {
        Some(Val::Int(n))
    }

    #[test]
    fn arithmetic_precedence_and_literals() {
        assert_eq!(run("5 + 2 * 3", &[]), int(11));
        assert_eq!(run("(5 + 2) * 3", &[]), int(21));
        assert_eq!(run("0x10 | 0b101 | 010", &[]), int(16 | 5 | 8));
        assert_eq!(run("1'000'000 / 7u", &[]), int(142857));
        assert_eq!(run("1 << 4 >> 2", &[]), int(4));
        assert_eq!(run("-3 % 2", &[]), int(-1));
        assert_eq!(run("~0", &[]), int(-1));
        assert_eq!(run("'a' + 1", &[]), int(98));
        assert_eq!(run("'\\n'", &[]), int(10));
        assert_eq!(run("10 > 3 && 2 >= 2", &[]), Some(Val::Bool(true)));
        assert_eq!(run("1 ? 10 : 20", &[]), int(10));
        assert_eq!(run("0 ? 10 : 1 ? 20 : 30", &[]), int(20));
    }

    #[test]
    fn floats_and_casts() {
        assert_eq!(run("1.5 * 2", &[]), Some(Val::Float(3.0)));
        assert_eq!(run("1.5f + 1", &[]), Some(Val::Float(2.5)));
        assert_eq!(run("(int)3.9", &[]), int(3));
        assert_eq!(run("(unsigned char)300", &[]), int(44));
        assert_eq!(run("(int)-1 + 1", &[]), int(0));
        assert_eq!(run("static_cast<uint8_t>(257)", &[]), int(1));
        assert_eq!(run("uint32_t(1) << 31", &[]), int(1 << 31));
        assert_eq!(run("sizeof(int) + sizeof(uint64_t) + sizeof(char*)", &[]), int(4 + 8 + 8));
        assert_eq!(run("(float)1 / 4", &[]), Some(Val::Float(0.25)));
    }

    #[test]
    fn names_come_from_the_resolver_and_unknowns_stay_unknown() {
        let names = [("K", Val::Int(40)), ("Ns", Val::Int(2))];
        assert_eq!(run("K + 2", &names), int(42));
        assert_eq!(run("ns::Ns * K", &names), int(80));
        assert_eq!(run("::Ns", &names), int(2));
        assert_eq!(run("K + missing", &names), None);
        assert_eq!(run("0 && missing", &names), Some(Val::Bool(false)));
        assert_eq!(run("1 || missing", &names), Some(Val::Bool(true)));
        assert_eq!(run("1 ? K : missing", &names), int(40));
        assert_eq!(run("compute(K) + 1", &names), None);
        assert_eq!(run("Wrapper<int>::value + 1", &names), None);
        assert_eq!(run("1 / 0", &[]), None);
        assert_eq!(run("1 +", &[]), None);
        assert_eq!(run("(1", &[]), None);
        assert_eq!(run("K K", &names), None);
    }

    #[test]
    fn display() {
        assert_eq!(Val::Int(47).display(), "47 (0x2F)");
        assert_eq!(Val::Int(5).display(), "5");
        assert_eq!(Val::Int(-5).display(), "-5");
        assert_eq!(Val::Bool(true).display(), "true");
        assert_eq!(Val::Float(2.0).display(), "2.0");
    }
}
