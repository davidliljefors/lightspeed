//! Experiment: a resolved-occurrence index. Every identifier use that resolves to
//! exactly one symbol is stored as `(offset, symbol)` per file, so references could
//! become a lookup instead of per-site type inference. Built here only to measure
//! its time and memory; nothing reads it yet.

use rayon::prelude::*;

use super::{Query, sig_tokens};
use crate::lexer::{Keyword, TokenKind as K};
use crate::symbols::SymbolId;

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

impl Query<'_> {
    /// Resolves every candidate identifier of every live file in parallel.
    pub fn build_occurrences(&self) -> (Vec<FileOccurrences>, OccurrenceStats) {
        let idx = self.idx;
        let files: Vec<(usize, &str)> = idx.live_files().map(|(i, f)| (i, f.path.as_str())).collect();
        let parts: Vec<(usize, FileOccurrences, OccurrenceStats)> = files
            .par_iter()
            .map(|&(fi, path)| {
                let mut st = OccurrenceStats::default();
                let mut out = FileOccurrences::default();
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
                        _ => {
                            // Several records of one entity (forward declarations, definitions, overrides) are still one symbol.
                            let mut group = Vec::new();
                            self.equivalents(r[0], &mut group);
                            if r.iter().all(|c| group.contains(c)) {
                                out.sites.push((tok.start, idx.hier.canonical(r[0])));
                            } else {
                                // Of same-named symbols, only those declared in files this one can see.
                                let seen = idx.reachable_files(fi);
                                let visible: Vec<SymbolId> = r.iter().copied().filter(|&c| self.location(c).is_some_and(|l| seen.contains(&l.file))).collect();
                                let mut group = Vec::new();
                                if let Some(&first) = visible.first() {
                                    self.equivalents(first, &mut group);
                                }
                                if !visible.is_empty() && visible.iter().all(|c| group.contains(c)) {
                                    out.sites.push((tok.start, idx.hier.canonical(visible[0])));
                                } else {
                                    st.ambiguous += 1;
                                }
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
            total.stored += st.stored;
            total.bytes += occ.sites.capacity() * std::mem::size_of::<(u32, SymbolId)>();
            by_file[fi] = occ;
        }
        (by_file, total)
    }
}
