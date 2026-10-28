//! Template parameters and specializations.
//!
//! Like function locals, template parameters are not indexed: the
//! `template<...>` header written before a declaration is re-read on demand.
//! Specializations (`struct X<int>`) are indexed as ordinary records with the
//! `SPECIALIZATION` flag; their written arguments are re-read the same way.

use super::locals::{Local, LocalKind};
use super::types::{matching_fwd, split_args};
use super::{Query, sig_tokens};
use crate::lexer::{Token, TokenKind as K};
use crate::symbols::{SymbolFlags, SymbolId};

pub(super) struct Param {
    /// Empty for unnamed parameters.
    pub name: String,
    /// Byte range of the whole parameter, e.g. `class A = allocator<T>`.
    pub decl: (u32, u32),
    /// Byte range of the name.
    pub name_at: (u32, u32),
}

pub(super) struct Header {
    /// Offset of the `<` that opens the parameter list.
    pub open: u32,
    pub params: Vec<Param>,
}

/// The `template<...>` header written just before a declaration starting at `decl_start`.
pub(super) fn template_header(src: &str, decl_start: usize) -> Option<Header> {
    let before = src.get(..decl_start)?;
    let mut b = before.trim_end().as_bytes();
    // Tolerate export macros between the header and the declaration.
    for _ in 0..3 {
        if b.last() == Some(&b'>') {
            break;
        }
        let n = b.iter().rev().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
        if n == 0 {
            return None;
        }
        b = before.get(..b.len() - n)?.trim_end().as_bytes();
    }
    let close = b.len().checked_sub(1).filter(|&c| b[c] == b'>')?;
    let mut depth = 0i32;
    let mut open = None;
    for k in (close.saturating_sub(4000)..=close).rev() {
        match b[k] {
            b'>' if k == 0 || b[k - 1] != b'-' => depth += 1,
            b'<' => {
                depth -= 1;
                if depth == 0 {
                    open = Some(k);
                    break;
                }
            }
            _ => {}
        }
    }
    let open = open?;
    if !before[..open].trim_end().ends_with("template") {
        return None;
    }
    let base = open + 1;
    let inner = &before[base..close];
    let toks = sig_tokens(inner);
    let mut params = Vec::new();
    let (mut depth, mut defaulted) = (0i32, false);
    let mut seg: Option<(u32, u32)> = None;
    let mut name: Option<(&str, u32, u32)> = None;
    let abs = |v: u32| v + base as u32;
    let mut finish = |seg: &mut Option<(u32, u32)>, name: &mut Option<(&str, u32, u32)>| {
        if let Some((a, b)) = seg.take() {
            let (n, s, e) = name.take().unwrap_or(("", a, a));
            params.push(Param { name: n.to_owned(), decl: (abs(a), abs(b)), name_at: (abs(s), abs(e)) });
        }
    };
    for t in &toks {
        match t.kind {
            K::Lt | K::LParen | K::LBracket | K::LBrace => depth += 1,
            K::Gt | K::RParen | K::RBracket | K::RBrace => depth -= 1,
            K::Comma if depth == 0 => {
                finish(&mut seg, &mut name);
                defaulted = false;
                continue;
            }
            K::Eq if depth == 0 => defaulted = true,
            K::Ident if depth == 0 && !defaulted => name = Some((t.text(inner), t.start, t.end())),
            _ => {}
        }
        seg = Some((seg.map_or(t.start, |s| s.0), t.end()));
    }
    finish(&mut seg, &mut name);
    Some(Header { open: open as u32, params })
}

impl Query<'_> {
    /// Marks every token bound to a template parameter of the templates overlapping
    /// `[lo, hi)`. Outer templates go first so an inner parameter of the same name wins.
    pub(super) fn template_kinds(&self, toks: &[Token], text: &str, path: &str, lo: u32, hi: u32, kinds: &mut [Option<LocalKind>]) {
        let (db, Some(fi)) = (&self.idx.db, self.idx.file_index(path)) else { return };
        let mut templates: Vec<(u32, u32, Header)> = self
            .idx
            .file_symbols(fi)
            .filter_map(|id| {
                let s = db.symbol(id);
                if !s.flags.contains(SymbolFlags::TEMPLATE) || s.extent_start >= hi || s.extent_end < lo {
                    return None;
                }
                template_header(text, s.extent_start as usize).map(|h| (h.open, s.extent_end, h))
            })
            .collect();
        templates.sort_by_key(|(open, end, _)| std::cmp::Reverse(end - open));
        for (open, end, header) in &templates {
            let a = toks.partition_point(|t| t.start < *open);
            for i in a..toks.len() {
                let t = &toks[i];
                if t.end() > *end {
                    break;
                }
                if t.kind == K::Ident
                    && header.params.iter().any(|p| !p.name.is_empty() && p.name == t.text(text))
                    && (i == 0 || !matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon))
                {
                    kinds[i] = Some(LocalKind::TemplateParam);
                }
            }
        }
    }

    /// The template parameter the identifier `toks[hit]` refers to, with all
    /// its occurrences: the innermost enclosing template that declares the name.
    pub(super) fn template_param_in(&self, toks: &[Token], path: &str, text: &str, hit: usize) -> Option<Local> {
        let name = toks[hit].text(text);
        let offset = toks[hit].start;
        if hit > 0 && matches!(toks[hit - 1].kind, K::Dot | K::Arrow | K::ColonColon) {
            return None;
        }
        let (db, fi) = (&self.idx.db, self.idx.file_index(path)?);
        let mut best: Option<(u32, SymbolId, Header, usize)> = None;
        for id in self.idx.file_symbols(fi) {
            let s = db.symbol(id);
            if !s.flags.contains(SymbolFlags::TEMPLATE) || s.extent_start.saturating_sub(4000) > offset || offset > s.extent_end {
                continue;
            }
            let Some(h) = template_header(text, s.extent_start as usize) else { continue };
            if offset < h.open {
                continue;
            }
            let Some(pi) = h.params.iter().position(|p| p.name == name) else { continue };
            let span = s.extent_end - h.open;
            if best.as_ref().is_none_or(|b| span < b.0) {
                best = Some((span, id, h, pi));
            }
        }
        let (_, function, header, pi) = best?;
        let end = db.symbol(function).extent_end;
        let p = &header.params[pi];
        let uses = toks
            .iter()
            .enumerate()
            .filter(|&(_, t)| t.kind == K::Ident && t.start >= header.open && t.end() <= end && t.text(text) == name)
            .filter(|&(i, _)| i == 0 || !matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon))
            .map(|(_, t)| (t.start, t.end()))
            .collect();
        let signature = text.get(p.decl.0 as usize..p.decl.1 as usize)?.split_whitespace().collect::<Vec<_>>().join(" ");
        Some(Local {
            kind: LocalKind::TemplateParam,
            name: name.to_owned(),
            signature,
            auto: false,
            function,
            decl: p.name_at,
            uses,
            hovered: (toks[hit].start, toks[hit].end()),
            decl_offset: p.name_at.0,
        })
    }

    /// The arguments written after the name of a specialization (`int` in
    /// `struct X<int>`), normalized like [`split_args`].
    pub(super) fn spec_args(&self, id: SymbolId) -> Vec<String> {
        let db = &self.idx.db;
        let s = db.symbol(id);
        let Some(text) = self.texts.text(db.files.path(s.file)) else { return Vec::new() };
        let Some(tail) = text.as_str().get(s.name_offset as usize..) else { return Vec::new() };
        let mut n = tail.len().min(600);
        while !tail.is_char_boundary(n) {
            n -= 1;
        }
        let tail = &tail[..n];
        let toks = sig_tokens(tail);
        if toks.get(1).is_none_or(|t| t.kind != K::Lt) {
            return Vec::new();
        }
        matching_fwd(&toks, 1).map(|c| split_args(&toks[2..c], tail)).unwrap_or_default()
    }

    /// Whether specialization `spec` applies to an instantiation with `args`.
    /// Arguments that are the specialization's own template parameters
    /// (`X<T*>`) match anything.
    pub(super) fn spec_matches(&self, spec: SymbolId, args: &[String]) -> bool {
        let sargs = self.spec_args(spec);
        if sargs.len() != args.len() {
            return false;
        }
        let db = &self.idx.db;
        let s = db.symbol(spec);
        let params: Vec<String> = self
            .texts
            .text(db.files.path(s.file))
            .and_then(|t| template_header(t.as_str(), s.extent_start as usize))
            .map(|h| h.params.into_iter().map(|p| p.name).collect())
            .unwrap_or_default();
        let stars = |s: &str| s.len() - s.trim_end_matches('*').len();
        sargs.iter().zip(args).all(|(s, a)| s == a || a.is_empty() || (params.iter().any(|p| p == s.trim_end_matches('*')) && stars(a) >= stars(s)))
    }

    /// Narrows `cands` (records of one name) to the specialization an
    /// instantiation with `args` selects, or to the primary template when
    /// none matches or no arguments are written.
    pub(super) fn choose_specialization(&self, cands: &mut Vec<SymbolId>, args: Option<&[String]>) {
        let db = &self.idx.db;
        let is_spec = |c: SymbolId| db.symbol(c).flags.contains(SymbolFlags::SPECIALIZATION);
        if !cands.iter().any(|&c| is_spec(c)) {
            return;
        }
        let (specs, rest): (Vec<SymbolId>, Vec<SymbolId>) = cands.iter().partition(|&&c| is_spec(c));
        if let Some(args) = args {
            let exact: Vec<SymbolId> = specs.iter().copied().filter(|&s| self.spec_args(s) == args).collect();
            let matching = if exact.is_empty() { specs.iter().copied().filter(|&s| self.spec_matches(s, args)).collect() } else { exact };
            if !matching.is_empty() {
                *cands = matching;
                return;
            }
        }
        if !rest.is_empty() {
            *cands = rest;
        }
    }
}
