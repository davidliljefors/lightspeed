//! Queries over an [`Index`]: definition, references, completion, hover, signature
//! help, semantic tokens and symbol search. Everything here is position/offset
//! based and independent of the LSP wire format, so it can be tested directly.
//!
//! Layout: this file holds text access, scoping and shared helpers; [`definition`]
//! resolves names; [`types`] does on-demand receiver type resolution (the place to
//! grow type inference, with `signature` and `nav` built on it); `locals` and
//! `templates` bind parameters, locals and template parameters; `refs`,
//! `completion`, `hover`, `semantic`, `search` and `consteval` are the features.
//!
//! Resolution is deliberately approximate: names are matched lexically and
//! narrowed by scope, qualifier, member access and call arity.

mod completion;
mod consteval;
mod definition;
mod hover;
mod locals;
mod refs;
mod search;
mod semantic;
mod templates;
mod types;

pub use hover::Hover;
pub use locals::{Local, LocalKind};
pub use refs::SymbolRefs;
pub use semantic::{MODIFIERS as SEMANTIC_MODIFIERS, SemTok, TOKEN_TYPES as SEMANTIC_TYPES};
pub use types::signature::{SigHelp, SigInfo};

use std::sync::Arc;

use rayon::prelude::*;

use crate::index::{Index, find_occurrences};
use crate::lexer::{Token, TokenKind, lex_into};
use crate::source::SourceFile;
use crate::symbols::{SymbolFlags, SymbolId};

/// File contents, either mapped from disk or an editor buffer.
pub enum Text {
    File(SourceFile),
    Shared(Arc<str>),
}

impl Text {
    pub fn as_str(&self) -> &str {
        match self {
            Self::File(f) => f.text(),
            Self::Shared(s) => s,
        }
    }
}

/// Where query code gets file contents; the LSP overlays open buffers on disk.
pub trait TextSource: Sync {
    fn text(&self, path: &str) -> Option<Text>;
}

pub struct DiskText;

impl TextSource for DiskText {
    fn text(&self, path: &str) -> Option<Text> {
        SourceFile::load(path).ok().map(Text::File)
    }
}

/// A source range in an indexed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    pub file: u32,
    pub start: u32,
    pub end: u32,
}

fn sig_tokens(text: &str) -> Vec<Token> {
    let mut t = Vec::with_capacity(text.len() / 6 + 16);
    lex_into(text, &mut t);
    t.retain(|t| t.kind != TokenKind::Comment);
    t
}

/// Byte range of the identifier touching `offset`.
pub fn identifier_at(text: &str, offset: u32) -> Option<(u32, u32)> {
    let toks = sig_tokens(text);
    let t = &toks[ident_at(&toks, offset)?];
    Some((t.start, t.start + t.len))
}

/// Index of the identifier token touching `offset`.
fn ident_at(toks: &[Token], offset: u32) -> Option<usize> {
    let i = toks.partition_point(|t| t.start <= offset).checked_sub(1)?;
    let t = &toks[i];
    (t.kind == TokenKind::Ident && t.keyword != crate::lexer::Keyword::Reserved && offset <= t.start + t.len).then_some(i)
}

/// Using-directives and namespace aliases in effect at one point: those in
/// the file before it plus the namespace-level ones of everything it includes.
struct Decls {
    local: crate::scope_decls::ScopeDecls,
    included: Option<std::sync::Arc<crate::scope_decls::ScopeDecls>>,
}

impl Decls {
    fn names_contain(&self, name: &str) -> bool {
        self.local.names_contain(name) || self.included.as_ref().is_some_and(|d| d.names_contain(name))
    }

    fn alias(&self, alias: &str) -> Option<&str> {
        self.local.alias(alias).or_else(|| self.included.as_ref().and_then(|d| d.alias(alias)))
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Kinds that can be forward declared (`class X;`, `enum class E : int;`).
fn is_forward_decl_kind(db: &crate::symbols::SymbolDb, c: SymbolId) -> bool {
    let k = db.symbol(c).kind;
    k.is_record() || k == crate::symbols::SymbolKind::Enum
}

/// Removes forward declarations that have a definition with the same
/// qualified name among `cands`, so scoring can't prefer them (e.g. for
/// being in the same file as the use).
fn drop_forward_declarations(db: &crate::symbols::SymbolDb, cands: &mut Vec<SymbolId>) {
    let is_def = |c: SymbolId| db.symbol(c).flags.contains(SymbolFlags::DEFINITION);
    let fwd = |c: &SymbolId| is_forward_decl_kind(db, *c) && !is_def(*c);
    if !cands.iter().any(fwd) || !cands.iter().any(|&c| is_forward_decl_kind(db, c) && is_def(c)) {
        return;
    }
    let qname = |c: SymbolId| {
        let mut s = String::new();
        db.write_qualified_name(c, &mut s);
        s
    };
    let defined: Vec<String> = cands.iter().copied().filter(|&c| is_forward_decl_kind(db, c) && is_def(c)).map(qname).collect();
    cands.retain(|c| !fwd(c) || !defined.contains(&qname(*c)));
}

/// Number of top-level arguments in the call whose `(` is `toks[open]`.
fn call_arity(toks: &[Token], open: usize) -> u32 {
    let (mut depth, mut commas, mut empty) = (0i32, 0u32, true);
    for t in &toks[open..] {
        match t.kind {
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            TokenKind::Comma if depth == 1 => commas += 1,
            _ if depth >= 1 && !(t.kind == TokenKind::LParen && depth == 1) => empty = false,
            _ => {}
        }
        if depth == 1 && t.kind != TokenKind::LParen && t.kind != TokenKind::Comma {
            empty = false;
        }
    }
    if empty { 0 } else { commas + 1 }
}

pub struct Query<'a> {
    pub idx: &'a Index,
    pub texts: &'a dyn TextSource,
}

impl<'a> Query<'a> {
    pub fn new(idx: &'a Index, texts: &'a dyn TextSource) -> Self {
        Self { idx, texts }
    }

    /// Scope declarations enclosing `offset`, innermost first. Out-of-line
    /// definitions contribute their class through the in-class declaration.
    pub fn scopes_at(&self, fi: Option<usize>, offset: u32) -> Vec<SymbolId> {
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        let mut best: Option<(u32, SymbolId)> = None;
        if let Some(fi) = fi {
            for id in self.idx.file_symbols(fi) {
                let s = db.symbol(id);
                if s.extent_start <= offset && offset < s.extent_end {
                    let len = s.extent_end - s.extent_start;
                    if best.is_none_or(|(l, _)| len <= l) {
                        best = Some((len, id));
                    }
                }
            }
        }
        let mut out = Vec::new();
        let mut cur = best.map(|(_, id)| hier.canonical(id));
        while let Some(x) = cur {
            if db.symbol(x).kind.is_scope() {
                out.push(x);
            }
            cur = db.symbol(x).parent;
        }
        out
    }

    fn scope_set(&self, scopes: &[SymbolId]) -> (Vec<SymbolId>, Vec<SymbolId>) {
        let mut bases = Vec::new();
        for &s in scopes {
            if self.idx.db.symbol(s).kind.is_record() {
                self.idx.hier.all_bases(s, &mut bases);
            }
        }
        (scopes.to_vec(), bases)
    }

    /// Where `id`'s name is written.
    pub fn location(&self, id: SymbolId) -> Option<Location> {
        let s = self.idx.db.symbol(id);
        let file = self.idx.file_index(self.idx.db.files.path(s.file))?;
        let (start, end) = self.idx.db.name_range(id);
        Some(Location { file: file as u32, start, end })
    }

    /// The class (or struct/union) named at `offset`, at a declaration or a use;
    /// type aliases are followed to their record.
    pub fn class_at(&self, path: &str, text: &str, offset: u32) -> Option<SymbolId> {
        let db = &self.idx.db;
        let ids: Vec<SymbolId> = self.symbol_at(path, offset).into_iter().chain(self.definition(path, text, offset)).collect();
        ids.into_iter().find_map(|id| {
            let id = self.idx.hier.canonical(id);
            if db.symbol(id).kind.is_record() { Some(id) } else { self.resolve_records([id]).first().copied() }
        })
    }

    /// Symbol whose name is written at `offset` in `path` (a declaration site).
    pub fn symbol_at(&self, path: &str, offset: u32) -> Option<SymbolId> {
        let fi = self.idx.file_index(path)?;
        self.idx.file_symbols(fi).find(|&id| {
            let (a, b) = self.idx.db.name_range(id);
            a <= offset && offset <= b
        })
    }

    /// Every lexical occurrence of `name`, found by consulting the identifier
    /// index and scanning only candidate files, in parallel. Because virtual
    /// overrides share a name, references to a base method include its overriders.
    pub fn references(&self, name: &str) -> Vec<Location> {
        let files: Vec<(usize, &str)> =
            self.idx.live_files().filter(|(_, f)| f.may_contain(name)).map(|(i, f)| (i, f.path.as_str())).collect();
        let mut out: Vec<Location> = files
            .par_iter()
            .map_init(
                || (Vec::<Token>::new(), Vec::<(u32, u32)>::new()),
                |(toks, occ), &(fi, path)| {
                    occ.clear();
                    if let Some(t) = self.texts.text(path) {
                        find_occurrences(t.as_str(), name, toks, occ);
                    }
                    occ.iter().map(|&(start, end)| Location { file: fi as u32, start, end }).collect::<Vec<_>>()
                },
            )
            .flatten_iter()
            .collect();
        out.sort_unstable();
        out
    }
}
