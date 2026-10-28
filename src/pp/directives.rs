//! Directive handling: conditionals, `#define`, `#include`, pragmas.

use super::expand::{ExpTok, F_SPACE, Texts};
use super::expr;
use super::macros::{BodyTok, Macro, MacroKind, NONE, NO_PARAM};
use super::vfs::FileProvider;
use super::{Cond, DefineRecord, DiagKind, IncludeRecord, Input, PpOutput, Preprocessor};
use crate::lexer::{Lexer, Token, TokenFlags, TokenKind as K, lex_into};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    If,
    Else,
    Elif,
    Endif,
    Other,
}

fn classify(s: &str) -> Class {
    match s {
        "if" | "ifdef" | "ifndef" => Class::If,
        "else" => Class::Else,
        "elif" | "elifdef" | "elifndef" => Class::Elif,
        "endif" => Class::Endif,
        _ => Class::Other,
    }
}

#[inline]
fn bol(t: &Token) -> bool {
    t.flags.contains(TokenFlags::BOL)
}

/// Reduces a header to its directive lines (header mode ignores everything
/// else), re-lexed into an owned buffer so the cache holds no file mapping.
/// Newlines between directives are kept, so line numbers match the original.
pub(super) fn compact_directives(src: &str, toks: &[Token], text: &mut String, out: &mut Vec<Token>) {
    text.clear();
    let (mut i, mut prev) = (0, 0usize);
    while i < toks.len() {
        if toks[i].kind == K::Hash && bol(&toks[i]) {
            let mut j = i + 1;
            while j < toks.len() && !bol(&toks[j]) {
                j += 1;
            }
            let start = toks[i].start as usize;
            let nl = memchr::memchr_iter(b'\n', &src.as_bytes()[prev..start]).count();
            text.extend(std::iter::repeat_n('\n', nl));
            prev = toks[j - 1].end() as usize;
            text.push_str(&src[start..prev]);
            i = j;
        } else {
            i += 1;
        }
    }
    text.push('\n');
    lex_into(text, out);
}

impl<P: FileProvider> Preprocessor<'_, P> {
    /// Header mode: only directives matter, everything else is skipped.
    pub(super) fn scan_directives(&mut self, inp: &mut Input<'_>, out: &mut PpOutput) {
        while let Some(&t) = inp.toks.get(inp.pos) {
            inp.pos += 1;
            if t.kind == K::Hash && bol(&t) {
                self.directive(inp, out);
            }
        }
    }

    /// Executes the directive whose `#` was just consumed. Leaves `inp.pos` at
    /// the start of the next line (or, after a skipped group, at the `#` of the
    /// directive that ends it).
    pub(super) fn directive(&mut self, inp: &mut Input<'_>, out: &mut PpOutput) {
        let toks = inp.toks;
        let src = inp.src;
        let hash_loc = toks[inp.pos - 1].start;
        let start = inp.pos;
        let mut end = start;
        while end < toks.len() && !bol(&toks[end]) {
            end += 1;
        }
        inp.pos = end;
        let line = &toks[start..end];
        let Some(name) = line.first() else { return };
        if name.kind == K::Number {
            return; // `# 12 "file"` line markers
        }
        let args = &line[1..];
        let depth = inp.depth;
        match name.text(src) {
            "define" => match self.do_define(args, src) {
                Some((loc, len, function_like)) => {
                    if depth == 0 {
                        out.defines.push(DefineRecord { name_loc: loc, name_len: len, function_like, undef: false });
                    }
                }
                None => out.diag(depth, hash_loc, DiagKind::BadDefine),
            },
            "undef" => match args.first().filter(|t| matches!(t.kind, K::Ident | K::Keyword)) {
                Some(t) => {
                    self.t.undef(t.text(src));
                    if depth == 0 {
                        out.defines.push(DefineRecord {
                            name_loc: t.start,
                            name_len: t.len,
                            function_like: false,
                            undef: true,
                        });
                    }
                }
                None => out.diag(depth, hash_loc, DiagKind::BadDirective),
            },
            "include" | "import" => self.do_include(args, inp, out, hash_loc, false),
            "include_next" => self.do_include(args, inp, out, hash_loc, true),
            "if" => {
                let v = self.eval_cond(args, inp, out, hash_loc);
                self.open_cond(v, hash_loc, inp, out);
            }
            n @ ("ifdef" | "ifndef") => {
                let v = match args.first() {
                    Some(t) => self.is_defined(t.text(src)) == (n == "ifdef"),
                    None => {
                        out.diag(depth, hash_loc, DiagKind::BadDirective);
                        false
                    }
                };
                self.open_cond(v, hash_loc, inp, out);
            }
            n @ ("elif" | "elifdef" | "elifndef") => {
                if self.cond.len() <= self.cond_base {
                    out.diag(depth, hash_loc, DiagKind::ElseWithoutIf);
                    return;
                }
                let top = self.cond.len() - 1;
                if self.cond[top].seen_else {
                    out.diag(depth, hash_loc, DiagKind::ElseAfterElse);
                    self.skip_group(inp, out, true);
                } else if self.cond[top].taken {
                    self.skip_group(inp, out, true);
                } else {
                    let v = match n {
                        "elif" => self.eval_cond(args, inp, out, hash_loc),
                        _ => match args.first() {
                            Some(t) => self.is_defined(t.text(src)) == (n == "elifdef"),
                            None => false,
                        },
                    };
                    if v {
                        self.cond[top].taken = true;
                    } else {
                        self.skip_group(inp, out, false);
                    }
                }
            }
            "else" => {
                if self.cond.len() <= self.cond_base {
                    out.diag(depth, hash_loc, DiagKind::ElseWithoutIf);
                    return;
                }
                let top = self.cond.len() - 1;
                if self.cond[top].seen_else {
                    out.diag(depth, hash_loc, DiagKind::ElseAfterElse);
                }
                let was_taken = self.cond[top].taken || self.cond[top].seen_else;
                self.cond[top].seen_else = true;
                if was_taken {
                    self.skip_group(inp, out, true);
                } else {
                    self.cond[top].taken = true;
                }
            }
            "endif" => {
                if self.cond.len() > self.cond_base {
                    self.cond.pop();
                } else {
                    out.diag(depth, hash_loc, DiagKind::EndifWithoutIf);
                }
            }
            "pragma" => self.do_pragma(args, src),
            "error" => out.diag(depth, hash_loc, DiagKind::UserError),
            "warning" | "line" | "ident" | "sccs" | "assert" | "unassert" => {}
            _ => out.diag(depth, hash_loc, DiagKind::UnknownDirective),
        }
    }

    fn open_cond(&mut self, v: bool, loc: u32, inp: &mut Input<'_>, out: &mut PpOutput) {
        self.cond.push(Cond { taken: v, seen_else: false, loc });
        if !v {
            self.skip_group(inp, out, false);
        }
    }

    /// Skips an inactive group. Stops *at* the `#` of the directive that ends
    /// it (`#else`/`#elif*` unless `to_endif`, or the matching `#endif`) so the
    /// normal directive path handles it. Only directive structure is parsed;
    /// everything else, however malformed, is skipped.
    fn skip_group(&mut self, inp: &mut Input<'_>, out: &mut PpOutput, to_endif: bool) {
        let toks = inp.toks;
        let src = inp.src;
        let a = if inp.pos > 0 { toks[inp.pos - 1].end() } else { 0 };
        let mut i = inp.pos;
        let mut nest = 0u32;
        let stop = loop {
            while i < toks.len() && !(toks[i].kind == K::Hash && bol(&toks[i])) {
                i += 1;
            }
            if i >= toks.len() {
                break toks.len();
            }
            if let Some(n) = toks.get(i + 1).filter(|n| !bol(n)) {
                match classify(n.text(src)) {
                    Class::If => nest += 1,
                    Class::Endif if nest == 0 => break i,
                    Class::Endif => nest -= 1,
                    Class::Else | Class::Elif if nest == 0 && !to_endif => break i,
                    _ => {}
                }
            }
            i += 1;
        };
        inp.pos = stop;
        if inp.depth == 0 {
            let b = toks.get(stop).map_or(src.len() as u32, |t| t.start);
            if b > a {
                out.skipped.push((a, b));
            }
        }
    }

    fn eval_cond(&mut self, args: &[Token], inp: &mut Input<'_>, out: &mut PpOutput, loc: u32) -> bool {
        if args.is_empty() {
            out.diag(inp.depth, loc, DiagKind::BadExpression);
            return false;
        }
        let mut list = self.take_vec();
        list.extend(args.iter().map(|&t| ExpTok::from_src(t)));
        let mut res = self.take_vec();
        let saved = std::mem::replace(&mut self.in_if, true);
        self.expand_list(&list, inp, out, &mut res);
        self.in_if = saved;
        let r = expr::eval(&res, Texts { arena: &self.t.arena, src: inp.src });
        self.give_vec(list);
        self.give_vec(res);
        match r {
            Ok(v) => v,
            Err(e) => {
                let kind = match e {
                    expr::EvalError::Syntax => DiagKind::BadExpression,
                    expr::EvalError::DivByZero => DiagKind::DivisionByZero,
                };
                out.diag(inp.depth, loc, kind);
                false
            }
        }
    }

    /// Parses `NAME[(params)] body` (tokens after `define`) into the table.
    /// Returns the name's `(offset, len, function_like)` on success.
    pub(super) fn do_define(&mut self, line: &[Token], src: &str) -> Option<(u32, u32, bool)> {
        let nt = *line.first()?;
        if !matches!(nt.kind, K::Ident | K::Keyword) {
            return None;
        }
        let name = nt.text(src);
        if name == "defined" {
            return None;
        }
        let rest = &line[1..];
        let mut func = false;
        let mut variadic = false;
        let mut body_from = 0;
        self.param_scratch.clear();
        if let Some(p) = rest.first().filter(|p| p.kind == K::LParen && !p.space_before()) {
            let _ = p;
            func = true;
            let mut i = 1;
            let mut closed = false;
            while let Some(t) = rest.get(i) {
                i += 1;
                match t.kind {
                    K::RParen => {
                        closed = true;
                        break;
                    }
                    K::Comma => {}
                    K::Ellipsis => {
                        variadic = true;
                        self.param_scratch.push((NONE, 0));
                    }
                    K::Ident | K::Keyword => {
                        if rest.get(i).is_some_and(|n| n.kind == K::Ellipsis) {
                            variadic = true;
                            i += 1;
                        }
                        self.param_scratch.push((t.start, t.len));
                    }
                    _ => return None,
                }
            }
            if !closed || self.param_scratch.len() >= NO_PARAM as usize {
                return None;
            }
            body_from = i;
        }

        let name_sp = self.t.intern(name);
        let body_start = self.t.body.len() as u32;
        for (k, t) in rest[body_from..].iter().enumerate() {
            let text = t.text(src);
            let sp = self.t.intern(text);
            let param = if func && matches!(t.kind, K::Ident | K::Keyword) {
                self.param_scratch
                    .iter()
                    .position(|&(s, l)| {
                        if s == NONE { text == "__VA_ARGS__" } else { &src[s as usize..(s + l) as usize] == text }
                    })
                    .map_or(NO_PARAM, |p| p as u16)
            } else {
                NO_PARAM
            };
            self.t.body.push(BodyTok {
                off: sp.off,
                len: sp.len,
                kind: t.kind,
                keyword: t.keyword,
                flags: if k > 0 && t.space_before() { F_SPACE } else { 0 },
                param,
            });
        }
        let body_len = self.t.body.len() as u32 - body_start;
        self.t.define(Macro {
            name: name_sp,
            kind: if func { MacroKind::Function } else { MacroKind::Object },
            variadic,
            nparams: self.param_scratch.len() as u16,
            body_start,
            body_len,
            disabled: false,
        });
        Some((nt.start, nt.len, func))
    }

    /// `define` from text such as `FOO 1` or `F(x) (x)` (command-line `-D`).
    pub(super) fn define_line(&mut self, text: &str) {
        let mut toks = std::mem::take(&mut self.tmp_toks);
        toks.clear();
        toks.extend(Lexer::new(text));
        let _ = self.do_define(&toks, text);
        self.tmp_toks = toks;
    }

    fn do_include(&mut self, args: &[Token], inp: &mut Input<'_>, out: &mut PpOutput, loc: u32, next: bool) {
        let depth = inp.depth;
        let mut list = self.take_vec();
        list.extend(args.iter().map(|&t| ExpTok::from_src(t)));
        let mut name = std::mem::take(&mut self.scratch2);
        let mut quote = self.header_name(&list, inp.src, &mut name);
        if quote.is_none() {
            let mut res = self.take_vec();
            self.expand_list(&list, inp, out, &mut res);
            quote = self.header_name(&res, inp.src, &mut name);
            self.give_vec(res);
        }
        self.give_vec(list);
        let Some(quote) = quote else {
            out.diag(depth, loc, DiagKind::BadInclude);
            self.scratch2 = name;
            return;
        };

        let resolved = self.resolve(&name, quote, next);
        if depth == 0 {
            let no = out.pool.len() as u32;
            out.pool.push_str(&name);
            let ro = out.pool.len() as u32;
            if resolved.is_some() {
                out.pool.push_str(&self.path_buf);
            }
            out.includes.push(IncludeRecord {
                loc,
                angled: !quote,
                name: (no, name.len() as u32),
                resolved: (ro, out.pool.len() as u32 - ro),
            });
        }
        match resolved {
            None => {
                out.stats.unresolved += 1;
                out.diag(depth, loc, DiagKind::IncludeNotFound);
            }
            Some(r) if self.cfg.follow_includes => {
                if depth as usize + 1 > self.cfg.max_include_depth as usize {
                    out.diag(depth, loc, DiagKind::IncludeTooDeep);
                } else {
                    self.enter_file(r.key, r.dir_idx, out);
                }
            }
            Some(_) => {}
        }
        self.scratch2 = name;
    }

    fn do_pragma(&mut self, args: &[Token], src: &str) {
        match args.first().map(|t| t.text(src)) {
            Some("once") => {
                let key = self.files[self.file_depth].key;
                self.once.insert(key);
            }
            Some(op @ ("push_macro" | "pop_macro")) => {
                let Some(s) = args.get(2).filter(|t| t.kind == K::String) else { return };
                let text = s.text(src);
                if text.len() < 2 {
                    return;
                }
                let name = &text[1..text.len() - 1];
                if op == "push_macro" {
                    let def = self.t.lookup(name);
                    self.pushed_macros.push((name.to_owned(), def));
                } else if let Some(i) = self.pushed_macros.iter().rposition(|(n, _)| n == name) {
                    let (_, def) = self.pushed_macros.remove(i);
                    match def {
                        Some(d) => self.t.restore(d),
                        None => {
                            self.t.undef(name);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// If the whole file is wrapped in `#ifndef X ... #endif` (or
/// `#if !defined(X) ... #endif`) with nothing outside, returns `X`. Skipping
/// the file whenever `X` is defined is then always correct, whether or not the
/// file itself defines `X`.
pub(super) fn detect_guard<'a>(src: &'a str, toks: &[Token]) -> Option<&'a str> {
    let t0 = toks.first()?;
    if !(t0.kind == K::Hash && bol(t0)) {
        return None;
    }
    let n1 = toks.get(1).filter(|t| !bol(t))?;
    fn ident(t: Option<&Token>) -> Option<&Token> {
        t.filter(|t| t.kind == K::Ident && !bol(t))
    }
    let guard = match n1.text(src) {
        "ifndef" => ident(toks.get(2))?.text(src),
        "if" => {
            toks.get(2).filter(|t| t.kind == K::Bang && !bol(t))?;
            toks.get(3).filter(|t| t.text(src) == "defined" && !bol(t))?;
            let mut i = 4;
            let paren = toks.get(i).is_some_and(|t| t.kind == K::LParen);
            if paren {
                i += 1;
            }
            let g = ident(toks.get(i))?;
            i += 1;
            if paren {
                toks.get(i).filter(|t| t.kind == K::RParen)?;
                i += 1;
            }
            if toks.get(i).is_some_and(|t| !bol(t)) {
                return None;
            }
            g.text(src)
        }
        _ => return None,
    };
    let mut nest = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if t.kind != K::Hash || !bol(t) {
            continue;
        }
        let Some(n) = toks.get(i + 1).filter(|n| !bol(n)) else { continue };
        match classify(n.text(src)) {
            Class::If => nest += 1,
            Class::Endif => {
                nest -= 1;
                if nest == 0 {
                    let rest = &toks[i + 1..];
                    return rest.iter().all(|t| !bol(t)).then_some(guard);
                }
            }
            _ => {}
        }
    }
    None
}
