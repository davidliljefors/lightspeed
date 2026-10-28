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
    /// Narrows several candidates for the identifier of file `fi` to one entity if possible.
    fn pick_unique(&self, fi: usize, r: &[SymbolId], type_position: bool) -> Pick {
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        // Records of one entity (forward declarations, definitions, overrides) are one symbol.
        let one_entity = |v: &[SymbolId]| {
            let mut group = Vec::new();
            self.equivalents(v[0], &mut group);
            v.iter().all(|c| group.contains(c))
        };
        if one_entity(r) {
            return Pick::One(hier.canonical(r[0]));
        }
        let seen = self.idx.reachable_files(fi);
        let mut v: Vec<SymbolId> = r.iter().copied().filter(|&c| self.location(c).is_some_and(|l| seen.contains(&l.file))).collect();
        if v.is_empty() {
            return Pick::External;
        }
        // `X::` and `X name` can only name a type or namespace.
        if type_position {
            let scopes: Vec<SymbolId> = v.iter().copied().filter(|&c| db.symbol(c).kind.is_type() || db.symbol(c).kind == SymbolKind::Namespace).collect();
            if !scopes.is_empty() {
                v = scopes;
            }
        }
        // Conditional `#define`s of one name are one macro as far as uses are concerned.
        if v.iter().all(|&c| db.symbol(c).kind == SymbolKind::Macro) {
            return Pick::One(hier.canonical(v[0]));
        }
        // `typedef struct Foo Foo;` names the struct twice.
        let qn = |c: SymbolId| db.qualified_name(c);
        let types: Vec<String> = v.iter().filter(|&&c| matches!(db.symbol(c).kind, SymbolKind::Struct | SymbolKind::Class | SymbolKind::Union | SymbolKind::Enum)).map(|&c| qn(c)).collect();
        v.retain(|&c| db.symbol(c).kind != SymbolKind::TypeAlias || !types.contains(&qn(c)));
        // A namespace reopened in several files is one namespace.
        let mut spaces: Vec<String> = Vec::new();
        v.retain(|&c| {
            if db.symbol(c).kind != SymbolKind::Namespace {
                return true;
            }
            let q = qn(c);
            let fresh = !spaces.contains(&q);
            spaces.push(q);
            fresh
        });
        if one_entity(&v) { Pick::One(hier.canonical(v[0])) } else { Pick::Ambiguous(v) }
    }

    /// Resolves every candidate identifier of every live file in parallel.
    pub fn build_occurrences(&self) -> (Vec<FileOccurrences>, OccurrenceStats) {
        let idx = self.idx;
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
                for (i, tok) in toks.iter().enumerate() {
                    if tok.kind != K::Ident || tok.keyword == Keyword::Reserved {
                        continue;
                    }
                    let name = tok.text(text);
                    if idx.db.lookup(name).is_empty() || in_include_directive(text, tok.start as usize) {
                        continue;
                    }
                    st.candidates += 1;
                    let scoped = i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon);
                    if i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow) {
                        st.members += 1;
                    }
                    if !scoped && self.local_in(&toks, path, text, i).is_some() {
                        st.locals += 1;
                        continue;
                    }
                    let r = self.definition_in(&toks, path, text, i);
                    match r.as_slice() {
                        [] => st.unresolved += 1,
                        [one] => out.sites.push((tok.start, *one)),
                        _ => match self.pick_unique(fi, &r, toks.get(i + 1).is_some_and(|t| t.kind == K::ColonColon || (t.kind == K::Ident && t.keyword != Keyword::Reserved))) {
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
                        },
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
