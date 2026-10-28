//! Navigation queries over an [`Index`]: go-to-definition, find-references and
//! completion. Everything here is position/offset based and independent of
//! the LSP wire format, so it can be tested directly.
//!
//! Layout: this file holds text access, scoping, definition and references;
//! [`types`] does on-demand receiver type resolution (the place to grow type
//! inference); [`completion`] builds completion lists on top of both.
//!
//! Resolution is deliberately approximate: names are matched lexically and
//! narrowed by scope, qualifier, member access and call arity.

mod completion;
mod consteval;
mod hover;
mod locals;
mod refs;
mod search;
mod templates;
mod types;

pub use hover::Hover;
pub use locals::{Local, LocalKind};
pub use refs::SymbolRefs;
pub use types::signature::{SigHelp, SigInfo};

use std::sync::Arc;

use rayon::prelude::*;

use crate::ids::Idx;
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

    /// Declarations the identifier at `offset` may refer to, best first
    /// (all top-scoring candidates). Out-of-line members resolve to their
    /// definitions when one exists, otherwise to the declaration.
    pub fn definition(&self, path: &str, text: &str, offset: u32) -> Vec<SymbolId> {
        let toks = sig_tokens(text);
        let Some(i) = ident_at(&toks, offset) else { return Vec::new() };
        self.definition_in(&toks, path, text, i)
    }

    /// [`Self::definition`] for the identifier token `toks[i]` of an already lexed file.
    pub(super) fn definition_in(&self, toks: &[Token], path: &str, text: &str, i: usize) -> Vec<SymbolId> {
        let offset = toks[i].start;
        let name = toks[i].text(text);
        let (db, hier) = (&self.idx.db, &self.idx.hier);

        let member = i > 0 && matches!(toks[i - 1].kind, TokenKind::Dot | TokenKind::Arrow);
        let mut qual: Vec<&str> = Vec::new();
        let mut j = i;
        while j >= 2 && toks[j - 1].kind == TokenKind::ColonColon && toks[j - 2].kind == TokenKind::Ident {
            qual.push(toks[j - 2].text(text));
            j -= 2;
        }
        qual.reverse();
        let arity = (toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::LParen)).then(|| call_arity(toks, i + 1));

        let fi = self.idx.file_index(path);
        let scopes = self.scopes_at(fi, offset);
        let (in_scope, in_bases) = self.scope_set(&scopes);

        let mut cands: Vec<SymbolId> = db.lookup(name).iter().map(|&c| hier.canonical(c)).collect();
        if member {
            let recv = toks[i - 1].start;
            let types = self.receiver_types(path, text, recv, &scopes);
            if !types.is_empty() {
                let mut owners = Vec::new();
                for &t in &types {
                    owners.push(t);
                    hier.all_bases(t, &mut owners);
                }
                let typed: Vec<SymbolId> = cands.iter().copied().filter(|c| db.symbol(*c).parent.is_some_and(|p| owners.contains(&p))).collect();
                if !typed.is_empty() {
                    cands = typed;
                }
            }
        }
        cands.sort_unstable();
        cands.dedup();

        // Computed on first use: most lookups never need them.
        let decls = std::cell::OnceCell::new();
        let decls_at = || -> &Decls {
            decls.get_or_init(|| Decls {
                local: crate::scope_decls::scan(toks, text, i, false),
                included: fi.map(|f| self.idx.included_decls(f)),
            })
        };

        if !qual.is_empty() {
            let mut buf = String::new();
            let mut by_qualifier = |want: &str, cands: &[SymbolId]| -> Vec<SymbolId> {
                cands
                    .iter()
                    .copied()
                    .filter(|&c| {
                        let Some(p) = db.symbol(c).parent else { return false };
                        buf.clear();
                        db.write_qualified_name(p, &mut buf);
                        buf == want || buf.strip_suffix(want).is_some_and(|r| r.ends_with("::"))
                    })
                    .collect()
            };
            let mut filtered = by_qualifier(&qual.join("::"), &cands);
            if filtered.is_empty()
                && let Some(target) = decls_at().alias(qual[0])
            {
                // `namespace fs = std::filesystem;` makes `fs::` mean `std::filesystem::`.
                let mut want = target.to_owned();
                for q in &qual[1..] {
                    want.push_str("::");
                    want.push_str(q);
                }
                filtered = by_qualifier(&want, &cands);
            }
            if !filtered.is_empty() {
                cands = filtered;
            }
        }

        drop_forward_declarations(db, &mut cands);
        if cands.iter().any(|&c| db.symbol(c).flags.contains(crate::symbols::SymbolFlags::SPECIALIZATION)) {
            let args = (toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Lt))
                .then(|| types::matching_fwd(toks, i + 1))
                .flatten()
                .map(|c| types::split_args(&toks[i + 2..c], text));
            self.choose_specialization(&mut cands, args.as_deref());
        }
        if !member && qual.is_empty() && !(i > 0 && toks[i - 1].kind == TokenKind::ColonColon) {
            cands.retain(|&c| db.symbol(c).kind.is_callable() || self.visible_unqualified(c, &in_scope, &in_bases, || decls_at()));
        }

        let mut best: (i32, Vec<SymbolId>) = (i32::MIN, Vec::new());
        for &c in &cands {
            let s = db.symbol(c);
            let mut score = 0i32;
            let parent_kind = s.parent.map(|p| db.symbol(p).kind);
            let is_member = parent_kind.is_some_and(|k| k.is_record());
            if member {
                score += if is_member { 4 } else { -6 };
            } else if qual.is_empty() {
                if let Some(p) = s.parent {
                    if let Some(pos) = in_scope.iter().position(|&x| self.same_scope(x, p)) {
                        score += 8 - pos.min(4) as i32;
                    } else if in_bases.contains(&p) {
                        score += 3;
                    }
                }
                if s.kind == crate::symbols::SymbolKind::EnumConstant
                    && let Some(p) = s.parent.filter(|&p| db.symbol(p).flags.contains(SymbolFlags::SCOPED))
                    && !in_scope.contains(&p)
                {
                    // `enum class` constants need their qualifier.
                    score -= 10;
                }
                if fi.is_some_and(|f| self.idx.files[f].syms.0 <= c.index() as u32 && (c.index() as u32) < self.idx.files[f].syms.1) {
                    score += 1;
                }
            }
            if let Some(n) = arity {
                match db.signature(c) {
                    Some(sig) if u32::from(sig.arity) == n => score += 3,
                    Some(sig) if u32::from(sig.arity) > n => score += 1,
                    Some(_) => score -= 5,
                    None if s.kind.is_record() => score += 1,
                    None => score -= 2,
                }
                if s.kind.is_callable() || s.kind.is_record() {
                    score += 1;
                }
            }
            if score > best.0 {
                best = (score, vec![c]);
            } else if score == best.0 {
                best.1.push(c);
            }
        }

        let mut picked = best.1;
        if picked.iter().any(|&c| is_forward_decl_kind(db, c) && db.symbol(c).flags.contains(SymbolFlags::DEFINITION)) {
            picked.retain(|&c| !is_forward_decl_kind(db, c) || db.symbol(c).flags.contains(SymbolFlags::DEFINITION));
        }
        let mut out = Vec::new();
        for c in picked {
            let defs = hier.out_of_line_defs(c);
            if defs.is_empty() || db.symbol(c).flags.contains(SymbolFlags::DEFINITION) {
                out.push(c);
            } else {
                out.extend_from_slice(defs);
            }
        }
        out
    }

    /// Whether `a` and `b` are the same scope; reopened namespaces are separate
    /// symbols, so those compare by their chain of names.
    fn same_scope(&self, mut a: SymbolId, mut b: SymbolId) -> bool {
        use crate::symbols::SymbolKind as K;
        let db = &self.idx.db;
        loop {
            if a == b {
                return true;
            }
            let (sa, sb) = (db.symbol(a), db.symbol(b));
            if sa.kind != K::Namespace || sb.kind != K::Namespace || sa.name != sb.name {
                return false;
            }
            match (sa.parent, sb.parent) {
                (None, None) => return true,
                (Some(pa), Some(pb)) => (a, b) = (pa, pb),
                _ => return false,
            }
        }
    }

    /// Whether `c` can be named without qualification from a scope whose
    /// enclosing scopes are `in_scope` (and inherited ones `in_bases`).
    /// Members of other namespaces and classes need a qualifier or a `using`.
    fn visible_unqualified<'t>(&self, c: SymbolId, in_scope: &[SymbolId], in_bases: &[SymbolId], decls: impl FnOnce() -> &'t Decls) -> bool {
        use crate::symbols::SymbolKind as K;
        let db = &self.idx.db;
        // Unscoped enumerators and anonymous namespaces belong to the enclosing scope.
        let mut parent = db.symbol(c).parent;
        while let Some(p) = parent {
            let s = db.symbol(p);
            let transparent = (s.kind == K::Enum && !s.flags.contains(SymbolFlags::SCOPED)) || (s.kind == K::Namespace && db.names.get(s.name).is_empty());
            if !transparent {
                break;
            }
            parent = s.parent;
        }
        let Some(p) = parent else { return true };
        let kind = db.symbol(p).kind;
        if !matches!(kind, K::Namespace | K::Enum) && !kind.is_record() {
            return true;
        }
        if in_scope.iter().any(|&s| self.same_scope(s, p)) || in_bases.contains(&p) {
            return true;
        }
        let name = db.names.get(db.symbol(p).name);
        decls().names_contain(name)
    }

    /// Where `id`'s name is written.
    pub fn location(&self, id: SymbolId) -> Option<Location> {
        let s = self.idx.db.symbol(id);
        let file = self.idx.file_index(self.idx.db.files.path(s.file))?;
        let (start, end) = self.idx.db.name_range(id);
        Some(Location { file: file as u32, start, end })
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
