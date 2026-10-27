//! Macro expansion engine (cpplib-style context stack).
//!
//! A *context* is a token list being rescanned; the file stream is the
//! bottom-most source. A macro is disabled while its own context is live and
//! re-enabled when the context is popped, which is also when lookahead for a
//! function-like macro's `(` is allowed to run past the end of an expansion
//! (the classic `#define f(x) x f` / `f(1)(2)` case). Identifiers that hit a
//! disabled macro are painted [`F_NOEXP`] forever, so they are never expanded
//! later even when re-read outside the context.
//!
//! All scratch vectors come from per-worker pools; steady-state expansion does
//! not allocate.

use std::fmt::Write as _;

use super::macros::{BodyTok, Macro, MacroKind, NONE, NO_PARAM};
use super::vfs::FileProvider;
use super::{DiagKind, Input, PpOutput, Preprocessor};
use crate::lexer::{Keyword, Lexer, Token, TokenFlags, TokenKind as K};

pub(super) const F_SPACE: u8 = 1;
pub(super) const F_BOL: u8 = 2;
/// Text lives in the macro-table arena rather than the source file.
pub(super) const F_ARENA: u8 = 4;
/// Token passed through a macro expansion.
pub(super) const F_MACRO: u8 = 8;
/// Painted: never expand (self-referential macro name).
pub(super) const F_NOEXP: u8 = 16;
/// Empty-argument placeholder used while pasting.
pub(super) const F_PLACE: u8 = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExpTok {
    pub off: u32,
    pub len: u32,
    /// Offset in the file being preprocessed this token is attributed to.
    pub loc: u32,
    pub kind: K,
    pub keyword: Keyword,
    pub flags: u8,
}

impl ExpTok {
    pub fn from_src(t: Token) -> Self {
        let mut flags = 0;
        if t.flags.contains(TokenFlags::SPACE_BEFORE) {
            flags |= F_SPACE;
        }
        if t.flags.contains(TokenFlags::BOL) {
            flags |= F_BOL;
        }
        Self { off: t.start, len: t.len, loc: t.start, kind: t.kind, keyword: t.keyword, flags }
    }

    /// `0` or `1` using the digits pre-seeded at the head of every arena.
    pub fn digit(v: bool, loc: u32) -> Self {
        Self { off: v as u32, len: 1, loc, kind: K::Number, keyword: Keyword::None, flags: F_ARENA }
    }

    fn placemarker(loc: u32) -> Self {
        Self { off: 0, len: 0, loc, kind: K::Unknown, keyword: Keyword::None, flags: F_PLACE }
    }
}

/// Resolves token text from the two possible backing buffers.
#[derive(Clone, Copy)]
pub(super) struct Texts<'a> {
    pub arena: &'a str,
    pub src: &'a str,
}

impl<'a> Texts<'a> {
    #[inline]
    pub fn get(&self, t: &ExpTok) -> &'a str {
        let s = if t.flags & F_ARENA != 0 { self.arena } else { self.src };
        &s[t.off as usize..(t.off + t.len) as usize]
    }
}

pub(crate) struct Ctx {
    pub toks: Vec<ExpTok>,
    pub pos: usize,
    pub mac: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Special {
    None,
    Defined,
    HasInclude,
    HasOther,
}

impl<P: FileProvider> Preprocessor<'_, P> {
    pub(super) fn take_vec(&mut self) -> Vec<ExpTok> {
        self.vec_pool.pop().unwrap_or_default()
    }

    pub(super) fn give_vec(&mut self, mut v: Vec<ExpTok>) {
        v.clear();
        self.vec_pool.push(v);
    }

    fn take_ranges(&mut self) -> Vec<(u32, u32)> {
        self.range_pool.pop().unwrap_or_default()
    }

    fn give_ranges(&mut self, mut v: Vec<(u32, u32)>) {
        v.clear();
        self.range_pool.push(v);
    }

    /// Next token before macro expansion: pushback, then contexts down to
    /// `barrier`, then (if allowed) the file, where directives are executed.
    pub(super) fn raw_next(
        &mut self,
        inp: &mut Input<'_>,
        out: &mut PpOutput,
        barrier: usize,
        file_ok: bool,
    ) -> Option<ExpTok> {
        if let Some(t) = self.pushback.take() {
            return Some(t);
        }
        while self.ctx.len() > barrier {
            let c = self.ctx.last_mut().unwrap();
            if let Some(&t) = c.toks.get(c.pos) {
                c.pos += 1;
                return Some(t);
            }
            let c = self.ctx.pop().unwrap();
            if c.mac != NONE {
                self.t.defs[c.mac as usize].disabled = false;
            }
            self.give_vec(c.toks);
        }
        if !file_ok {
            return None;
        }
        loop {
            let t = *inp.toks.get(inp.pos)?;
            inp.pos += 1;
            if t.kind == K::Hash && t.flags.contains(TokenFlags::BOL) {
                self.directive(inp, out);
                continue;
            }
            return Some(ExpTok::from_src(t));
        }
    }

    /// Next fully macro-expanded token.
    pub(super) fn expand_next(
        &mut self,
        inp: &mut Input<'_>,
        out: &mut PpOutput,
        barrier: usize,
        file_ok: bool,
    ) -> Option<ExpTok> {
        loop {
            let t = self.raw_next(inp, out, barrier, file_ok)?;
            if !matches!(t.kind, K::Ident | K::Keyword) || t.flags & F_NOEXP != 0 {
                return Some(t);
            }
            let (mi, special) = {
                let name = Texts { arena: &self.t.arena, src: inp.src }.get(&t);
                let special = if self.in_if {
                    match name {
                        "defined" => Special::Defined,
                        "__has_include" | "__has_include_next" => Special::HasInclude,
                        n if n.starts_with("__has_") => Special::HasOther,
                        _ => Special::None,
                    }
                } else {
                    Special::None
                };
                (self.t.lookup(name), special)
            };
            match special {
                Special::Defined => return Some(self.do_defined(&t, inp, out, barrier)),
                Special::HasInclude => return Some(self.do_has_include(&t, inp, out, barrier)),
                Special::HasOther => {
                    self.skip_call(inp, out, barrier);
                    return Some(ExpTok::digit(false, t.loc));
                }
                Special::None => {}
            }
            let Some(mi) = mi else { return Some(t) };
            let m = self.t.defs[mi as usize];
            if m.disabled {
                let mut t = t;
                t.flags |= F_NOEXP;
                return Some(t);
            }
            if self.budget == 0 {
                out.diag(inp.depth, t.loc, DiagKind::ExpansionLimit);
                return Some(t);
            }
            match m.kind {
                MacroKind::Object => self.push_expansion(mi, &t, &[], &[], inp, out),
                MacroKind::Function => {
                    if !self.try_function(mi, &t, inp, out, barrier, file_ok) {
                        return Some(t);
                    }
                }
                MacroKind::Line => {
                    let mut line = inp.line_at(t.loc);
                    // Body tokens report the line of the invocation's closing paren.
                    if t.flags & F_MACRO != 0 {
                        line = line.max(self.call_line);
                    }
                    return Some(self.generated_number(line, &t));
                }
                MacroKind::IncludeLevel => return Some(self.generated_number(inp.depth, &t)),
                MacroKind::Counter => {
                    let c = self.counter;
                    self.counter += 1;
                    return Some(self.generated_number(c, &t));
                }
                MacroKind::File => {
                    self.scratch.clear();
                    self.scratch.push('"');
                    if let Some(f) = self.files.get(self.file_depth) {
                        self.scratch.push_str(&f.path);
                    }
                    self.scratch.push('"');
                    let sp = self.t.intern(&self.scratch);
                    return Some(ExpTok {
                        off: sp.off,
                        len: sp.len,
                        loc: t.loc,
                        kind: K::String,
                        keyword: Keyword::None,
                        flags: F_ARENA | F_MACRO | (t.flags & F_SPACE),
                    });
                }
            }
        }
    }

    fn generated_number(&mut self, v: u32, t: &ExpTok) -> ExpTok {
        self.scratch.clear();
        let _ = write!(self.scratch, "{v}");
        let sp = self.t.intern(&self.scratch);
        ExpTok {
            off: sp.off,
            len: sp.len,
            loc: t.loc,
            kind: K::Number,
            keyword: Keyword::None,
            flags: F_ARENA | F_MACRO | (t.flags & F_SPACE),
        }
    }

    /// Expands `list` in isolation (no file lookahead) into `dst`.
    pub(super) fn expand_list(
        &mut self,
        list: &[ExpTok],
        inp: &mut Input<'_>,
        out: &mut PpOutput,
        dst: &mut Vec<ExpTok>,
    ) {
        // Deeply nested macro calls in arguments would recurse here; past the
        // limit arguments are passed through unexpanded.
        if self.expand_depth >= 128 {
            dst.extend_from_slice(list);
            return;
        }
        self.expand_depth += 1;
        let barrier = self.ctx.len();
        let mut v = self.take_vec();
        v.extend_from_slice(list);
        self.ctx.push(Ctx { toks: v, pos: 0, mac: NONE });
        let saved = self.pushback.take();
        while let Some(t) = self.expand_next(inp, out, barrier, false) {
            dst.push(t);
        }
        self.pushback = saved;
        self.expand_depth -= 1;
    }

    fn do_defined(&mut self, t: &ExpTok, inp: &mut Input<'_>, out: &mut PpOutput, barrier: usize) -> ExpTok {
        let Some(n) = self.raw_next(inp, out, barrier, false) else {
            return ExpTok::digit(false, t.loc);
        };
        let paren = n.kind == K::LParen;
        let name = if paren { self.raw_next(inp, out, barrier, false) } else { Some(n) };
        let defined = name.is_some_and(|nt| {
            let name = Texts { arena: &self.t.arena, src: inp.src }.get(&nt);
            self.is_defined(name)
        });
        if paren {
            match self.raw_next(inp, out, barrier, false) {
                Some(c) if c.kind == K::RParen => {}
                Some(c) => self.pushback = Some(c),
                None => {}
            }
        }
        ExpTok::digit(defined, t.loc)
    }

    /// Consumes `( ... )` (balanced) without interpreting it.
    fn skip_call(&mut self, inp: &mut Input<'_>, out: &mut PpOutput, barrier: usize) {
        match self.raw_next(inp, out, barrier, false) {
            Some(n) if n.kind == K::LParen => {}
            Some(n) => {
                self.pushback = Some(n);
                return;
            }
            None => return,
        }
        let mut depth = 1;
        while depth > 0 {
            match self.raw_next(inp, out, barrier, false).map(|t| t.kind) {
                Some(K::LParen) => depth += 1,
                Some(K::RParen) => depth -= 1,
                Some(_) => {}
                None => break,
            }
        }
    }

    fn do_has_include(&mut self, t: &ExpTok, inp: &mut Input<'_>, out: &mut PpOutput, barrier: usize) -> ExpTok {
        let mut args = self.take_vec();
        match self.raw_next(inp, out, barrier, false) {
            Some(n) if n.kind == K::LParen => {
                let mut depth = 1;
                while let Some(a) = self.raw_next(inp, out, barrier, false) {
                    match a.kind {
                        K::LParen => depth += 1,
                        K::RParen => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    args.push(a);
                }
            }
            Some(n) => self.pushback = Some(n),
            None => {}
        }
        let mut name = std::mem::take(&mut self.scratch2);
        let found = match self.header_name(&args, inp.src, &mut name) {
            Some(quote) => self.resolve(&name, quote, false).is_some(),
            None => false,
        };
        self.scratch2 = name;
        self.give_vec(args);
        ExpTok::digit(found, t.loc)
    }

    /// Rebuilds an include name from `"..."` or `<...>` tokens. Returns whether
    /// it was the quoted form.
    pub(super) fn header_name(&self, toks: &[ExpTok], src: &str, dst: &mut String) -> Option<bool> {
        dst.clear();
        let x = Texts { arena: &self.t.arena, src };
        let first = toks.first()?;
        match first.kind {
            K::String => {
                let s = x.get(first);
                if s.len() < 2 {
                    return None;
                }
                dst.push_str(&s[1..s.len() - 1]);
                Some(true)
            }
            K::Lt => {
                let end = toks.iter().rposition(|t| t.kind == K::Gt).unwrap_or(toks.len());
                for (i, t) in toks[1..end.max(1)].iter().enumerate() {
                    if i > 0 && t.flags & F_SPACE != 0 {
                        dst.push(' ');
                    }
                    dst.push_str(x.get(t));
                }
                Some(false)
            }
            _ => None,
        }
    }

    /// If `t` (a function-like macro name) is followed by `(`, expands it and
    /// returns true. Otherwise the peeked token is pushed back.
    fn try_function(
        &mut self,
        mi: u32,
        t: &ExpTok,
        inp: &mut Input<'_>,
        out: &mut PpOutput,
        barrier: usize,
        file_ok: bool,
    ) -> bool {
        match self.raw_next(inp, out, barrier, file_ok) {
            Some(n) if n.kind == K::LParen => {}
            Some(n) => {
                self.pushback = Some(n);
                return false;
            }
            None => return false,
        }
        let m = self.t.defs[mi as usize];
        let np = m.nparams as usize;
        let mut flat = self.take_vec();
        let mut ranges = self.take_ranges();
        let mut depth = 1u32;
        let mut start = 0u32;
        loop {
            let Some(a) = self.raw_next(inp, out, barrier, file_ok) else {
                out.diag(inp.depth, t.loc, DiagKind::UnterminatedMacroArgs);
                ranges.push((start, flat.len() as u32));
                break;
            };
            match a.kind {
                K::LParen => {
                    depth += 1;
                    flat.push(a);
                }
                K::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        if a.flags & F_MACRO == 0 && a.flags & F_ARENA == 0 {
                            self.call_line = inp.line_at(a.loc);
                        }
                        ranges.push((start, flat.len() as u32));
                        break;
                    }
                    flat.push(a);
                }
                K::Comma if depth == 1 && !(m.variadic && ranges.len() + 1 >= np) => {
                    ranges.push((start, flat.len() as u32));
                    start = flat.len() as u32;
                }
                _ => flat.push(a),
            }
        }
        self.push_expansion(mi, t, &flat, &ranges, inp, out);
        self.give_vec(flat);
        self.give_ranges(ranges);
        true
    }

    fn push_expansion(
        &mut self,
        mi: u32,
        t: &ExpTok,
        flat: &[ExpTok],
        ranges: &[(u32, u32)],
        inp: &mut Input<'_>,
        out: &mut PpOutput,
    ) {
        out.stats.expansions += 1;
        let res = self.substitute(mi, t, flat, ranges, inp, out);
        self.budget = self.budget.saturating_sub(res.len() as u64 + 1);
        self.t.defs[mi as usize].disabled = true;
        self.ctx.push(Ctx { toks: res, pos: 0, mac: mi });
    }

    fn substitute(
        &mut self,
        mi: u32,
        inv: &ExpTok,
        flat: &[ExpTok],
        ranges: &[(u32, u32)],
        inp: &mut Input<'_>,
        out: &mut PpOutput,
    ) -> Vec<ExpTok> {
        let m: Macro = self.t.defs[mi as usize];
        let n = m.body_len as usize;
        let base = m.body_start as usize;
        let np = m.nparams as usize;
        let func = m.kind == MacroKind::Function;
        let mut res = self.take_vec();
        let mut eflat = self.take_vec();
        let mut eranges = self.take_ranges();
        eranges.resize(np, (u32::MAX, 0));

        let raw = |p: usize| -> &[ExpTok] {
            match ranges.get(p) {
                Some(&(s, e)) => &flat[s as usize..e as usize],
                None => &[],
            }
        };
        let va_param = if m.variadic { np.wrapping_sub(1) } else { usize::MAX };
        let va_present = m.variadic && !raw(va_param).is_empty();
        let mut vopt_close: Option<usize> = None;

        let mut i = 0;
        while i < n {
            let b: BodyTok = self.t.body[base + i];
            if vopt_close == Some(i) {
                vopt_close = None;
                i += 1;
                continue;
            }
            let next: Option<BodyTok> = (i + 1 < n).then(|| self.t.body[base + i + 1]);

            if func
                && b.kind == K::Ident
                && m.variadic
                && self.t.arena[b.off as usize..(b.off + b.len) as usize] == *"__VA_OPT__"
                && next.is_some_and(|x| x.kind == K::LParen)
            {
                let mut depth = 0;
                let mut j = i + 1;
                while j < n {
                    match self.t.body[base + j].kind {
                        K::LParen => depth += 1,
                        K::RParen => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if va_present {
                    vopt_close = Some(j);
                    i += 2;
                } else {
                    i = j + 1;
                }
                continue;
            }

            if func && b.kind == K::Hash
                && let Some(nb) = next.filter(|x| x.param != NO_PARAM) {
                    let s = self.stringify(raw(nb.param as usize), inp.src);
                    res.push(ExpTok {
                        off: s.off,
                        len: s.len,
                        loc: inv.loc,
                        kind: K::String,
                        keyword: Keyword::None,
                        flags: F_ARENA | F_MACRO | (b.flags & F_SPACE),
                    });
                    i += 2;
                    continue;
                }

            if b.kind == K::HashHash && i > 0 && i + 1 < n {
                let rb = self.t.body[base + i + 1];
                if rb.param != NO_PARAM {
                    let rhs = raw(rb.param as usize);
                    let gnu_comma = rb.param as usize == va_param && res.last().is_some_and(|l| l.kind == K::Comma);
                    if gnu_comma {
                        if rhs.is_empty() {
                            res.pop();
                        } else {
                            res.extend(rhs.iter().map(|&x| mark_macro(x)));
                        }
                    } else if let Some((first, rest)) = rhs.split_first() {
                        self.paste_onto(&mut res, *first, inp.src);
                        res.extend(rest.iter().map(|&x| mark_macro(x)));
                    }
                } else {
                    let tok = self.body_tok(&rb, inv);
                    self.paste_onto(&mut res, tok, inp.src);
                }
                i += 2;
                continue;
            }

            if b.param != NO_PARAM && func {
                let p = b.param as usize;
                let lhs_paste = next.is_some_and(|x| x.kind == K::HashHash);
                let first_len = res.len();
                if lhs_paste {
                    let a = raw(p);
                    if a.is_empty() {
                        res.push(ExpTok::placemarker(inv.loc));
                    } else {
                        res.extend(a.iter().map(|&x| mark_macro(x)));
                    }
                } else {
                    let (s, e) = eranges.get(p).copied().unwrap_or((0, 0));
                    let (s, e) = if s == u32::MAX {
                        let before = eflat.len() as u32;
                        let a = raw(p);
                        self.expand_list(a, inp, out, &mut eflat);
                        let r = (before, eflat.len() as u32);
                        eranges[p] = r;
                        r
                    } else {
                        (s, e)
                    };
                    res.extend(eflat[s as usize..e as usize].iter().map(|&x| mark_macro(x)));
                }
                if res.len() > first_len {
                    let f = &mut res[first_len];
                    f.flags = (f.flags & !F_SPACE) | (b.flags & F_SPACE);
                }
                i += 1;
                continue;
            }

            let tok = self.body_tok(&b, inv);
            res.push(tok);
            i += 1;
        }

        res.retain(|t| t.flags & F_PLACE == 0);
        if let Some(f) = res.first_mut() {
            f.flags = (f.flags & !(F_SPACE | F_BOL)) | (inv.flags & (F_SPACE | F_BOL));
        }
        self.give_vec(eflat);
        self.give_ranges(eranges);
        res
    }

    fn body_tok(&self, b: &BodyTok, inv: &ExpTok) -> ExpTok {
        ExpTok {
            off: b.off,
            len: b.len,
            loc: inv.loc,
            kind: b.kind,
            keyword: b.keyword,
            flags: F_ARENA | F_MACRO | (b.flags & F_SPACE),
        }
    }

    /// `##`: glues `rhs` onto the last token of `res` and re-lexes the result.
    /// If the glued text is not a single token (invalid paste) both stay.
    fn paste_onto(&mut self, res: &mut Vec<ExpTok>, rhs: ExpTok, src: &str) {
        let rhs = mark_macro(rhs);
        let Some(lhs) = res.last().copied() else {
            res.push(rhs);
            return;
        };
        if lhs.flags & F_PLACE != 0 {
            *res.last_mut().unwrap() = rhs;
            return;
        }
        if rhs.flags & F_PLACE != 0 {
            return;
        }
        let x = Texts { arena: &self.t.arena, src };
        self.scratch.clear();
        self.scratch.push_str(x.get(&lhs));
        self.scratch.push_str(x.get(&rhs));
        let mut lx = Lexer::new(&self.scratch);
        let one = match (lx.next_token(), lx.next_token()) {
            (Some(tok), None) if tok.start == 0 && tok.len as usize == self.scratch.len() => Some(tok),
            _ => None,
        };
        match one {
            Some(tok) => {
                let sp = self.t.intern(&self.scratch);
                *res.last_mut().unwrap() = ExpTok {
                    off: sp.off,
                    len: sp.len,
                    loc: lhs.loc,
                    kind: tok.kind,
                    keyword: tok.keyword,
                    flags: F_ARENA | F_MACRO | (lhs.flags & (F_SPACE | F_BOL)),
                };
            }
            None => res.push(rhs),
        }
    }

    /// `#param`: spells the raw argument as a string literal.
    fn stringify(&mut self, toks: &[ExpTok], src: &str) -> super::macros::Span {
        self.scratch.clear();
        self.scratch.push('"');
        let x = Texts { arena: &self.t.arena, src };
        for (i, t) in toks.iter().enumerate() {
            if i > 0 && t.flags & F_SPACE != 0 {
                self.scratch.push(' ');
            }
            let s = x.get(t);
            if matches!(t.kind, K::String | K::Char) {
                for c in s.chars() {
                    if c == '"' || c == '\\' {
                        self.scratch.push('\\');
                    }
                    self.scratch.push(c);
                }
            } else {
                self.scratch.push_str(s);
            }
        }
        self.scratch.push('"');
        self.t.intern(&self.scratch)
    }
}

#[inline]
fn mark_macro(mut t: ExpTok) -> ExpTok {
    t.flags |= F_MACRO;
    t
}
