//! Semantic tokens: one classification per identifier, from the name's
//! candidates in the symbol database.
//!
//! No per-token resolution happens here. When every symbol with that name has
//! the same kind the answer is a hash lookup; otherwise cheap syntactic hints
//! (`name(`, `.name`, `name::`) narrow the candidates. Parameters and locals
//! come first, from one binding pass per function (see `Query::local_kinds`).

use super::{LocalKind, Query, sig_tokens};
use crate::lexer::{Keyword, TokenKind as K};
use crate::symbols::{SymbolFlags, SymbolKind};

pub const TOKEN_TYPES: [&str; 14] = ["namespace", "class", "struct", "enum", "enumMember", "function", "method", "property", "macro", "type", "variable", "parameter", "typeParameter", "string"];
pub const MODIFIERS: [&str; 4] = ["static", "readonly", "abstract", "declaration"];
const DECLARATION: u8 = 8;

const NAMESPACE: u8 = 0;
const CLASS: u8 = 1;
const ENUM: u8 = 3;
const ENUM_MEMBER: u8 = 4;
const FUNCTION: u8 = 5;
const METHOD: u8 = 6;
const PROPERTY: u8 = 7;
const MACRO: u8 = 8;
const TYPE: u8 = 9;
const VARIABLE: u8 = 10;
const PARAMETER: u8 = 11;
const TYPE_PARAMETER: u8 = 12;
const STRING: u8 = 13;

/// Byte ranges of the header names in `#include`/`#include_next`/`#import` lines, in source order.
fn include_names(text: &str) -> Vec<(u32, u32)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line_start = true;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line_start = true;
            i += 1;
        } else if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
        } else if line_start && c == b'#' {
            line_start = false;
            let d = text[i + 1..].trim_start_matches([' ', '\t']);
            if d.starts_with("include") || d.starts_with("import") {
                out.extend(crate::workspace::index::include_name_span(text, i as u32));
            }
            i += 1;
        } else {
            line_start = false;
            i = skip_literal(b, i);
        }
    }
    out
}

/// Past the comment or string literal starting at `i`, or one byte on if there is none.
fn skip_literal(b: &[u8], i: usize) -> usize {
    let find = |from: usize, pat: &[u8]| b[from..].windows(pat.len()).position(|w| w == pat).map_or(b.len(), |p| from + p + pat.len());
    let line_end = |mut j: usize| {
        while j < b.len() && (b[j] != b'\n' || b[j - 1] == b'\\') {
            j += 1;
        }
        j
    };
    let ident = |j: usize| b[j].is_ascii_alphanumeric() || b[j] == b'_';
    match b[i] {
        b'/' if b.get(i + 1) == Some(&b'*') => find(i + 2, b"*/"),
        b'/' if b.get(i + 1) == Some(&b'/') => line_end(i + 2),
        b'R' if b.get(i + 1) == Some(&b'"') && (i == 0 || !ident(i - 1) || matches!(b[i - 1], b'L' | b'u' | b'U' | b'8')) => {
            let open = i + 2;
            match b[open..].iter().position(|&x| x == b'(' || x == b'\n') {
                Some(p) if b[open + p] == b'(' => {
                    let mut close = vec![b')'];
                    close.extend_from_slice(&b[open..open + p]);
                    close.push(b'"');
                    find(open + p + 1, &close)
                }
                _ => i + 1,
            }
        }
        q @ (b'"' | b'\'') if q == b'"' || i == 0 || !ident(i - 1) => {
            let mut j = i + 1;
            while j < b.len() && b[j] != q && b[j] != b'\n' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            if b.get(j) == Some(&q) { j + 1 } else { j.min(b.len()) }
        }
        _ => i + 1,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemTok {
    pub start: u32,
    pub len: u32,
    pub ty: u8,
    /// Bit set over [`MODIFIERS`].
    pub mods: u8,
}

fn token_type(k: SymbolKind) -> u8 {
    match k {
        SymbolKind::Namespace => NAMESPACE,
        // Themes colour `struct` like its keyword; the class-key says nothing about the type.
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union => CLASS,
        SymbolKind::Enum => ENUM,
        SymbolKind::EnumConstant => ENUM_MEMBER,
        SymbolKind::Function => FUNCTION,
        SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Destructor => METHOD,
        SymbolKind::Field => PROPERTY,
        SymbolKind::Variable => VARIABLE,
        SymbolKind::TypeAlias => TYPE,
        SymbolKind::Macro => MACRO,
    }
}

impl Query<'_> {
    /// Classified identifiers of `text` that start inside `range` (all if `None`), in source order.
    pub fn semantic_tokens(&self, path: &str, text: &str, range: Option<(u32, u32)>) -> Vec<SemTok> {
        let db = &self.idx.db;
        let toks = sig_tokens(text);
        let (lo, hi) = range.unwrap_or((0, u32::MAX));
        let locals = self.local_kinds(&toks, text, path, lo, hi);
        // Name offsets of the symbols this file declares, sorted.
        let mut declared: Vec<u32> = self.idx.file_index(path).map(|fi| self.idx.file_symbols(fi).map(|id| db.name_range(id).0).collect()).unwrap_or_default();
        declared.sort_unstable();
        let first = toks.partition_point(|t| t.end() <= lo);
        let mut out = Vec::new();
        // A header name is one string, not a run of identifiers.
        let includes = include_names(text);
        out.extend(includes.iter().filter(|&&(s, e)| s < hi && e > lo).map(|&(s, e)| SemTok { start: s, len: e - s, ty: STRING, mods: 0 }));
        for i in first..toks.len() {
            let t = &toks[i];
            if t.start >= hi {
                break;
            }
            if t.kind != K::Ident || t.keyword == Keyword::Reserved {
                continue;
            }
            let inc = includes.partition_point(|&(_, e)| e <= t.start);
            if includes.get(inc).is_some_and(|&(s, _)| s <= t.start) {
                continue;
            }
            if let Some((k, is_decl)) = locals[i] {
                let ty = match k {
                    LocalKind::Param => PARAMETER,
                    LocalKind::Variable => VARIABLE,
                    LocalKind::TemplateParam => TYPE_PARAMETER,
                };
                out.push(SemTok { start: t.start, len: t.len, ty, mods: if is_decl { DECLARATION } else { 0 } });
                continue;
            }
            let cands = db.lookup(t.text(text));
            if cands.is_empty() {
                continue;
            }
            let prev = i.checked_sub(1).map(|p| toks[p].kind);
            let next = toks.get(i + 1).map(|n| n.kind);
            let member = matches!(prev, Some(K::Dot | K::Arrow));
            let call = next == Some(K::LParen);
            let scope = next == Some(K::ColonColon);
            // `class X;` / `struct X` name a type, whatever else shares the name.
            let elaborated = i > 0 && matches!(toks[i - 1].keyword, Keyword::Class | Keyword::Struct | Keyword::Union | Keyword::Enum);
            let fits = |k: SymbolKind| {
                if elaborated {
                    k.is_type()
                } else if member {
                    matches!(k, SymbolKind::Field | SymbolKind::Method | SymbolKind::Variable)
                } else if scope {
                    k.is_scope() || k == SymbolKind::TypeAlias
                } else if call {
                    k.is_callable() || k.is_type() || k == SymbolKind::Macro
                } else {
                    // A bare name is not a constructor, which shares its class's name.
                    !matches!(k, SymbolKind::Constructor | SymbolKind::Destructor)
                }
            };
            let fitting = |&&c: &&crate::symbols::SymbolId| fits(db.symbol(c).kind);
            // Prefer the definition: a forward declaration may use another class-key (`class A;` vs `struct A {}`), which would colour the same type differently.
            let mut fit = cands.iter().filter(fitting);
            let Some(&first) = fit.next() else { continue };
            let first_ty = token_type(db.symbol(first).kind);
            let ambiguous = fit.any(|&c| token_type(db.symbol(c).kind) != first_ty);
            // Same-named symbols of different kinds: resolve the name by scope instead of guessing.
            let scoped = if ambiguous { self.definition_of_kind(&toks, path, text, i, &fits).into_iter().find(|&c| fits(db.symbol(c).kind)) } else { None };
            let Some(c) = scoped.or_else(|| cands.iter().filter(fitting).find(|&&c| db.symbol(c).flags.contains(SymbolFlags::DEFINITION)).or_else(|| cands.iter().find(fitting)).copied()) else { continue };
            let s = db.symbol(c);
            let mut mods = if declared.binary_search(&t.start).is_ok() { DECLARATION } else { 0 };
            for (bit, flag) in [(1, SymbolFlags::STATIC), (2, SymbolFlags::CONST), (4, SymbolFlags::PURE)] {
                if s.flags.contains(flag) {
                    mods |= bit;
                }
            }
            out.push(SemTok { start: t.start, len: t.len, ty: token_type(s.kind), mods });
        }
        out.sort_unstable_by_key(|t| t.start);
        out
    }
}
