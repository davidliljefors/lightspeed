//! Symbol-aware references (and the rename edit set built on them).
//!
//! Every lexical occurrence of the name is re-resolved with [`Query::definition_in`]
//! and kept only if it lands on the same symbol: virtual families, out-of-line
//! definitions and a class together with its constructors/destructor count as
//! one symbol. Occurrences that bind to a parameter/local never match.

use rayon::prelude::*;

use super::{Location, Query, ident_at, sig_tokens};
use crate::lexer::TokenKind as K;
use crate::symbols::{SymbolId, SymbolKind};

pub struct SymbolRefs {
    pub name: String,
    /// Every symbol that is "the same thing" as the one asked about, sorted.
    pub targets: Vec<SymbolId>,
    /// Occurrences that resolve to the targets only (safe to rename).
    pub certain: Vec<Location>,
    /// Same spelling, but resolution was ambiguous or failed (unknown receiver type, ...).
    pub uncertain: Vec<Location>,
}

impl Query<'_> {
    /// All symbols equivalent to `id` for renaming purposes.
    fn equivalents(&self, id: SymbolId, out: &mut Vec<SymbolId>) {
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        let mut seed = hier.canonical(id);
        if matches!(db.symbol(seed).kind, SymbolKind::Constructor | SymbolKind::Destructor)
            && let Some(p) = db.symbol(seed).parent.filter(|&p| db.symbol(p).kind.is_record())
        {
            seed = p;
        }
        let mut items = vec![seed, id];
        if db.symbol(seed).kind.is_record() {
            items.extend(db.children(seed).iter().copied().filter(|&c| matches!(db.symbol(c).kind, SymbolKind::Constructor | SymbolKind::Destructor)));
        }
        if let Some(f) = db.symbol(seed).family {
            items.extend_from_slice(hier.family(f));
        }
        let defs: Vec<SymbolId> = items.iter().flat_map(|&s| hier.out_of_line_defs(s).iter().copied()).collect();
        out.extend(items);
        out.extend(defs);
    }

    /// Occurrences of the symbol named at `offset`; `None` for parameters/locals
    /// (see [`Query::local_at`]) or when the name resolves to nothing.
    pub fn symbol_refs(&self, path: &str, text: &str, offset: u32) -> Option<SymbolRefs> {
        let toks = sig_tokens(text);
        let hit = ident_at(&toks, offset)?;
        if self.local_in(&toks, path, text, hit).is_some() {
            return None;
        }
        let name = toks[hit].text(text);
        let seeds = self.definition_in(&toks, path, text, hit);
        if seeds.is_empty() {
            return None;
        }
        let mut targets = Vec::new();
        for s in seeds {
            self.equivalents(s, &mut targets);
        }
        targets.sort_unstable();
        targets.dedup();
        let mut decl_sites: Vec<Location> = targets.iter().filter_map(|&t| self.location(t)).collect();
        decl_sites.sort_unstable();

        let files: Vec<(usize, &str)> =
            self.idx.live_files().filter(|(_, f)| f.may_contain(name)).map(|(i, f)| (i, f.path.as_str())).collect();
        let per_file: Vec<(Vec<Location>, Vec<Location>)> = files
            .par_iter()
            .map(|&(fi, path)| {
                let (mut certain, mut uncertain) = (Vec::new(), Vec::new());
                let Some(t) = self.texts.text(path) else { return (certain, uncertain) };
                let text = t.as_str();
                if memchr::memmem::find(text.as_bytes(), name.as_bytes()).is_none() {
                    return (certain, uncertain);
                }
                let toks = sig_tokens(text);
                for (i, tok) in toks.iter().enumerate() {
                    if tok.kind != K::Ident || tok.text(text) != name {
                        continue;
                    }
                    let loc = Location { file: fi as u32, start: tok.start, end: tok.end() };
                    if decl_sites.binary_search_by(|d| (d.file, d.start).cmp(&(loc.file, loc.start))).is_ok() {
                        certain.push(loc);
                        continue;
                    }
                    let scoped = i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon);
                    if !scoped && self.local_in(&toks, path, text, i).is_some() {
                        continue;
                    }
                    let r = self.definition_in(&toks, path, text, i);
                    let inside = r.iter().filter(|c| targets.binary_search(c).is_ok()).count();
                    if r.is_empty() || (inside > 0 && inside < r.len()) {
                        uncertain.push(loc);
                    } else if inside == r.len() {
                        certain.push(loc);
                    }
                }
                (certain, uncertain)
            })
            .collect();

        let (mut certain, mut uncertain) = (Vec::new(), Vec::new());
        for (c, u) in per_file {
            certain.extend(c);
            uncertain.extend(u);
        }
        certain.sort_unstable();
        uncertain.sort_unstable();
        Some(SymbolRefs { name: name.to_owned(), targets, certain, uncertain })
    }
}
