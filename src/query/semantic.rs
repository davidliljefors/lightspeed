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

pub const TOKEN_TYPES: [&str; 12] = ["namespace", "class", "struct", "enum", "enumMember", "function", "method", "property", "macro", "type", "variable", "parameter"];
pub const MODIFIERS: [&str; 3] = ["static", "readonly", "abstract"];

const NAMESPACE: u8 = 0;
const CLASS: u8 = 1;
const STRUCT: u8 = 2;
const ENUM: u8 = 3;
const ENUM_MEMBER: u8 = 4;
const FUNCTION: u8 = 5;
const METHOD: u8 = 6;
const PROPERTY: u8 = 7;
const MACRO: u8 = 8;
const TYPE: u8 = 9;
const VARIABLE: u8 = 10;
const PARAMETER: u8 = 11;

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
        SymbolKind::Class => CLASS,
        SymbolKind::Struct | SymbolKind::Union => STRUCT,
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
        let first = toks.partition_point(|t| t.end() <= lo);
        let mut out = Vec::new();
        for i in first..toks.len() {
            let t = &toks[i];
            if t.start >= hi {
                break;
            }
            if t.kind != K::Ident || t.keyword == Keyword::Reserved {
                continue;
            }
            if let Some(k) = locals[i] {
                out.push(SemTok { start: t.start, len: t.len, ty: if k == LocalKind::Param { PARAMETER } else { VARIABLE }, mods: 0 });
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
            let fits = |k: SymbolKind| {
                if member {
                    matches!(k, SymbolKind::Field | SymbolKind::Method | SymbolKind::Variable)
                } else if scope {
                    k.is_scope() || k == SymbolKind::TypeAlias
                } else if call {
                    k.is_callable() || k.is_type() || k == SymbolKind::Macro
                } else {
                    true
                }
            };
            let Some(&c) = cands.iter().find(|&&c| fits(db.symbol(c).kind)) else { continue };
            let s = db.symbol(c);
            let mut mods = 0u8;
            for (bit, flag) in [(1, SymbolFlags::STATIC), (2, SymbolFlags::CONST), (4, SymbolFlags::PURE)] {
                if s.flags.contains(flag) {
                    mods |= bit;
                }
            }
            out.push(SemTok { start: t.start, len: t.len, ty: token_type(s.kind), mods });
        }
        out
    }
}
