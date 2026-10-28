//! Experiment: a resolved-occurrence index. Every identifier use that resolves to
//! exactly one symbol is stored as `(offset, symbol)` per file, so references could
//! become a lookup instead of per-site type inference. Built here only to measure
//! its time and memory; nothing reads it yet.

use rayon::prelude::*;

use super::{Query, sig_tokens};
use crate::lexer::{Keyword, TokenKind as K};
use crate::symbols::{SymbolId, SymbolKind};

/// Certain occurrences of one file, sorted by offset.
#[derive(Default)]
pub struct FileOccurrences {
    pub sites: Vec<(u32, SymbolId)>,
}

#[derive(Default, Debug)]
pub struct OccurrenceStats {
    pub files: usize,
    /// Identifiers that share a spelling with an indexed symbol.
    pub candidates: u64,
    pub members: u64,
    pub locals: u64,
    pub unresolved: u64,
    pub ambiguous: u64,
    pub external: u64,
    pub stored: u64,
    pub bytes: usize,
}

/// Whether `at` lies on a `#include`/`#import` line, where `foo.h` is a path, not a member access.
fn in_include_directive(text: &str, at: usize) -> bool {
    let line = text[..at].rsplit('\n').next().unwrap_or("").trim_start();
    line.strip_prefix('#').is_some_and(|d| {
        let d = d.trim_start();
        d.starts_with("include") || d.starts_with("import")
    })
}

enum Pick {
    One(SymbolId),
    /// Every same-named candidate is declared where this file cannot see it: the name is
    /// something outside the index (a system header), so it is none of the candidates.
    External,
    /// Still several distinct visible candidates.
    Ambiguous(Vec<SymbolId>),
}

impl Query<'_> {
    /// Every record of the entity `sym` names (declarations, definitions, overrides).
    pub fn entity_records(&self, sym: SymbolId) -> Vec<SymbolId> {
        let mut out = vec![sym];
        self.equivalents(sym, &mut out);
        out
    }

    /// For every file, the files whose declarations some translation unit including it can see:
    /// the union of the reachable sets of every root translation unit above it. A header that
    /// relies on what its includer pulled in first is judged by all of its includers.
    fn visibility_contexts(&self) -> Vec<Vec<u64>> {
        let idx = self.idx;
        let n = idx.files.len();
        let words = n.div_ceil(64);
        let mut included = vec![false; n];
        for (fi, _) in idx.live_files() {
            for t in idx.included_files(fi) {
                included[t] = true;
            }
        }
        let mut vis = vec![vec![0u64; words]; n];
        for (fi, _) in idx.live_files() {
            let reach = idx.reachable_files(fi);
            let mut own = vec![0u64; words];
            for &f in reach.iter() {
                own[f as usize / 64] |= 1 << (f % 64);
            }
            // Roots define the contexts; a file in an include cycle with no root keeps its own.
            let targets: Vec<usize> = if included[fi] { vec![fi] } else { reach.iter().map(|&f| f as usize).collect() };
            for f in targets {
                for (w, o) in vis[f].iter_mut().zip(&own) {
                    *w |= o;
                }
            }
        }
        vis
    }

    /// Narrows the candidates for the identifier of file `fi` to one entity if possible. A
    /// candidate nothing in the including translation units can see is not a candidate.
    fn pick_unique(&self, vis: &[u64], r: &[SymbolId], type_position: bool) -> Pick {
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        // Records of one entity (forward declarations, definitions, overrides) are one symbol.
        let one_entity = |v: &[SymbolId]| {
            let mut group = Vec::new();
            self.equivalents(v[0], &mut group);
            v.iter().all(|c| group.contains(c))
        };
        let seen = |c: SymbolId| {
            let mut group = vec![c];
            self.equivalents(c, &mut group);
            group.iter().any(|&s| self.location(s).is_some_and(|l| vis[l.file as usize / 64] >> (l.file % 64) & 1 == 1))
        };
        let mut v: Vec<SymbolId> = r.iter().copied().filter(|&c| seen(c)).collect();
        if v.is_empty() {
            return Pick::External;
        }
        // `X::` and `X name` can only name a type or namespace (or a macro that expands to one).
        if type_position {
            let scopes: Vec<SymbolId> = v.iter().copied().filter(|&c| db.symbol(c).kind.is_type() || matches!(db.symbol(c).kind, SymbolKind::Namespace | SymbolKind::Macro)).collect();
            if !scopes.is_empty() {
                v = scopes;
            }
        }
        // Conditional `#define`s of one name in one file are one macro as far as uses are concerned.
        if v.iter().all(|&c| db.symbol(c).kind == SymbolKind::Macro) {
            let file = db.symbol(v[0]).file;
            return if v.iter().all(|&c| db.symbol(c).file == file) { Pick::One(hier.canonical(*v.iter().min().unwrap())) } else { Pick::Ambiguous(v) };
        }
        // `typedef struct Foo Foo;` names the struct twice; an alias of something else does not.
        let aliases: Vec<SymbolId> = v
            .iter()
            .copied()
            .filter(|&c| {
                db.symbol(c).kind == SymbolKind::TypeAlias && {
                    let q = db.qualified_name(c);
                    self.resolve_records([c]).iter().any(|&t| db.qualified_name(t) == q && v.contains(&t))
                }
            })
            .collect();
        v.retain(|c| !aliases.contains(c));
        // A namespace reopened in several files is one namespace.
        let first_space = v.iter().copied().find(|&c| db.symbol(c).kind == SymbolKind::Namespace);
        if let Some(first) = first_space {
            let q = db.qualified_name(first);
            let spaces: Vec<SymbolId> = v.iter().copied().filter(|&c| db.symbol(c).kind == SymbolKind::Namespace && db.qualified_name(c) == q).collect();
            let canon = *spaces.iter().min().unwrap();
            v.retain(|c| !spaces.contains(c));
            v.push(canon);
        }
        if v.len() == 1 || one_entity(&v) { Pick::One(hier.canonical(v[0])) } else { Pick::Ambiguous(v) }
    }

    /// Resolves every candidate identifier of every live file in parallel.
    pub fn build_occurrences(&self) -> (Vec<FileOccurrences>, OccurrenceStats) {
        let idx = self.idx;
        let contexts = self.visibility_contexts();
        let files: Vec<(usize, &str)> = idx.live_files().map(|(i, f)| (i, f.path.as_str())).collect();
        let parts: Vec<(usize, FileOccurrences, OccurrenceStats)> = files
            .par_iter()
            .map(|&(fi, path)| {
                let mut st = OccurrenceStats::default();
                let mut out = FileOccurrences::default();
                let debug = std::env::var_os("LS_AMB_DEBUG").is_some();
                let Some(t) = self.texts.text(path) else { return (fi, out, st) };
                let text = t.as_str();
                let toks = sig_tokens(text);
                let vis = &contexts[fi];
                // Identifiers inside a macro invocation's arguments mean whatever the expansion makes them.
                let mut in_macro_args = vec![false; toks.len()];
                let mut parens: Vec<bool> = Vec::new();
                let mut open_macros = 0usize;
                // Inside a class declared in a function body: its members are not indexed, so a bare name may be one.
                let mut in_local_class = vec![false; toks.len()];
                // Per open brace: 0 other, 1 namespace/extern block, 2 class body.
                let mut braces: Vec<u8> = Vec::new();
                for (i, tok) in toks.iter().enumerate() {
                    in_macro_args[i] = open_macros > 0;
                    in_local_class[i] = braces.iter().enumerate().any(|(d, &b)| b == 2 && braces[..d].contains(&0));
                    match tok.kind {
                        K::LBrace => {
                            let (mut class, mut ns, mut paren) = (false, false, false);
                            for t in toks[..i].iter().rev().take(64) {
                                match (t.kind, t.keyword) {
                                    (K::Semi | K::LBrace | K::RBrace, _) => break,
                                    (K::LParen, _) => paren = true,
                                    (_, Keyword::Class | Keyword::Struct | Keyword::Union) => class = true,
                                    (_, Keyword::Namespace | Keyword::Extern) => ns = true,
                                    (_, Keyword::Enum) => paren = true,
                                    _ => {}
                                }
                            }
                            braces.push(if ns { 1 } else if class && !paren { 2 } else { 0 });
                        }
                        K::RBrace => {
                            braces.pop();
                        }
                        _ => {}
                    }
                    match tok.kind {
                        K::LParen => {
                            let is_macro = i > 0
                                && toks[i - 1].kind == K::Ident
                                && idx.db.lookup(toks[i - 1].text(text)).iter().any(|&c| idx.db.symbol(c).kind == SymbolKind::Macro);
                            open_macros += is_macro as usize;
                            parens.push(is_macro);
                        }
                        K::RParen => open_macros -= parens.pop().unwrap_or(false) as usize,
                        _ => {}
                    }
                }
                // Aliases a qualifier may be spelled through; spelled anywhere in the file or its includes.
                let file_aliases = std::cell::OnceCell::new();
                let qualifier_is_alias = |i: usize| {
                    let mut j = i;
                    while j >= 2 && toks[j - 1].kind == K::ColonColon && toks[j - 2].kind == K::Ident {
                        j -= 2;
                    }
                    if j == i {
                        return false;
                    }
                    let q = toks[j].text(text);
                    idx.db.lookup(q).iter().any(|&c| matches!(idx.db.symbol(c).kind, SymbolKind::TypeAlias | SymbolKind::Macro))
                        || idx.included_decls(fi).alias(q).is_some()
                        || file_aliases.get_or_init(|| crate::scope_decls::scan(&toks, text, toks.len(), false)).alias(q).is_some()
                };
                for (i, tok) in toks.iter().enumerate() {
                    if tok.kind != K::Ident || tok.keyword == Keyword::Reserved {
                        continue;
                    }
                    let name = tok.text(text);
                    if idx.db.lookup(name).is_empty() || in_include_directive(text, tok.start as usize) {
                        continue;
                    }
                    st.candidates += 1;
                    if in_macro_args[i] {
                        st.ambiguous += 1;
                        continue;
                    }
                    let scoped = i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon);
                    if i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow) {
                        st.members += 1;
                    }
                    if !scoped && self.local_in(&toks, path, text, i).is_some() {
                        st.locals += 1;
                        continue;
                    }
                    let (r, guessed) = self.definition_checked(&toks, path, text, i);
                    if r.is_empty() {
                        st.unresolved += 1;
                        continue;
                    }
                    let maybe_local_member = in_local_class[i] && !scoped && r.iter().any(|&c| idx.db.symbol(c).parent.is_some_and(|p| idx.db.symbol(p).kind.is_record()));
                    if guessed || maybe_local_member {
                        st.ambiguous += 1;
                        continue;
                    }
                    if qualifier_is_alias(i) {
                        st.ambiguous += 1;
                        continue;
                    }
                    match self.pick_unique(vis, &r, toks.get(i + 1).is_some_and(|t| t.kind == K::ColonColon || (t.kind == K::Ident && t.keyword != Keyword::Reserved))) {
                        Pick::One(s) => out.sites.push((tok.start, s)),
                        Pick::External => st.external += 1,
                        Pick::Ambiguous(visible) => {
                            st.ambiguous += 1;
                            if debug {
                                let db = &idx.db;
                                let ctx = if i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow) { "member" } else if i > 0 && toks[i - 1].kind == K::ColonColon { "qualified" } else { "bare" };
                                let call = toks.get(i + 1).is_some_and(|t| t.kind == K::LParen);
                                let kinds: std::collections::BTreeSet<String> = visible.iter().map(|&c| format!("{:?}", db.symbol(c).kind)).collect();
                                eprintln!("AMB\t{name}\t{ctx}\t{}\t{}/{}\t{}\t{}:{}", if call { "call" } else { "nocall" }, visible.len(), r.len(), kinds.into_iter().collect::<Vec<_>>().join("+"), path.rsplit('/').next().unwrap_or(path), text[..tok.start as usize].matches('\n').count() + 1);
                            }
                        }
                    }
                }
                st.files = 1;
                st.stored = out.sites.len() as u64;
                out.sites.shrink_to_fit();
                (fi, out, st)
            })
            .collect();

        let mut total = OccurrenceStats::default();
        let mut by_file: Vec<FileOccurrences> = (0..idx.files.len()).map(|_| FileOccurrences::default()).collect();
        for (fi, occ, st) in parts {
            total.files += st.files;
            total.candidates += st.candidates;
            total.members += st.members;
            total.locals += st.locals;
            total.unresolved += st.unresolved;
            total.ambiguous += st.ambiguous;
            total.external += st.external;
            total.stored += st.stored;
            total.bytes += occ.sites.capacity() * std::mem::size_of::<(u32, SymbolId)>();
            by_file[fi] = occ;
        }
        (by_file, total)
    }
}
