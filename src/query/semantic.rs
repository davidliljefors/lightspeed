//! Semantic tokens: one classification per identifier, from the name's
//! candidates in the symbol database.
//!
//! No per-token resolution happens here. When every symbol with that name has
//! the same kind the answer is a hash lookup; otherwise cheap syntactic hints
//! (`name(`, `.name`, `name::`) narrow the candidates. Parameters and locals
//! come first, from one binding pass per function (see `Query::local_kinds`).

use super::{LocalKind, Query, sig_tokens};
use crate::lexer::{Keyword, TokenKind as K};
use crate::symbols::{SymbolFlags, SymbolId, SymbolKind};

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
fn include_names(toks: &[crate::lexer::Token], text: &str) -> Vec<(u32, u32)> {
    toks.windows(2)
        .filter(|w| w[0].kind == K::Hash && w[0].at_line_start() && !w[1].at_line_start() && matches!(w[1].text(text), "include" | "include_next" | "import"))
        .filter_map(|w| crate::workspace::index::include_name_span(text, w[0].start))
        .collect()
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
    /// The symbol that colours `toks[i]` among the same-named `cands` that `fits`.
    fn pick_candidate(&self, toks: &[crate::lexer::Token], path: &str, text: &str, i: usize, cands: &[SymbolId], fits: &dyn Fn(SymbolKind) -> bool) -> Option<SymbolId> {
        let db = &self.idx.db;
        let mut fit = cands.iter().copied().filter(|&c| fits(db.symbol(c).kind));
        let first = fit.next()?;
        let first_ty = token_type(db.symbol(first).kind);
        // Same-named symbols of different kinds: resolve the name by scope instead of guessing.
        if fit.any(|c| token_type(db.symbol(c).kind) != first_ty)
            && let Some(c) = self.definition_of_kind(toks, path, text, i, fits).into_iter().find(|&c| fits(db.symbol(c).kind))
        {
            return Some(c);
        }
        // Prefer the definition: a forward declaration may use another class-key (`class A;` vs `struct A {}`), which would colour the same type differently.
        let definition = cands.iter().copied().find(|&c| fits(db.symbol(c).kind) && db.symbol(c).flags.contains(SymbolFlags::DEFINITION));
        definition.or(Some(first))
    }

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
        let includes = include_names(&toks, text);
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
            let Some(c) = self.pick_candidate(&toks, path, text, i, cands, &fits) else { continue };
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
