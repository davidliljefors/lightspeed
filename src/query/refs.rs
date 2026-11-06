//! Symbol-aware references (and the rename edit set built on them).
//!
//! Every lexical occurrence of the name is re-resolved with [`Query::definition_in`]
//! and kept only if it lands on the same symbol: virtual families, out-of-line
//! definitions and a class together with its constructors/destructor count as
//! one symbol. Occurrences that bind to a parameter/local never match.

use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// TEMP profiling: [read ns, lex ns, resolve ns, bytes, files read, files with name, candidates]
pub static PROF: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];

pub(crate) fn lap(slot: usize, t: &mut std::time::Instant) {
    PROF[slot].fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    *t = std::time::Instant::now();
}

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

/// Symbols of different classes never share references, whatever their spelling.
fn kind_class(k: SymbolKind) -> u8 {
    use SymbolKind::*;
    match k {
        Method | Constructor | Destructor => 1,
        Function => 2,
        Field | Variable => 3,
        Class | Struct | Union | Enum | TypeAlias => 4,
        Namespace => 5,
        EnumConstant => 6,
        Macro => 7,
    }
}

/// `goto name;` or a `name:` label at the start of a statement.
fn is_label(toks: &[crate::lexer::Token], text: &str, i: usize) -> bool {
    let prev = i.checked_sub(1).map(|p| &toks[p]);
    if prev.is_some_and(|p| p.kind == K::Ident && p.text(text) == "goto") {
        return true;
    }
    toks.get(i + 1).is_some_and(|n| n.kind == K::Colon) && prev.is_none_or(|p| matches!(p.kind, K::Semi | K::LBrace | K::RBrace))
}

impl Query<'_> {
    /// All symbols equivalent to `id` for renaming purposes.
    pub(super) fn equivalents(&self, id: SymbolId, out: &mut Vec<SymbolId>) {
        let (db, hier) = (&self.idx.db, &self.idx.hier);
        let mut seed = hier.canonical(id);
        if matches!(db.symbol(seed).kind, SymbolKind::Constructor | SymbolKind::Destructor)
            && let Some(p) = db.symbol(seed).parent.filter(|&p| db.symbol(p).kind.is_record())
        {
            seed = p;
        }
        let mut items = vec![seed, id];
        if db.symbol(seed).kind.is_record() {
            // Specializations share the template's name.
            let q = db.qualified_name(seed);
            let spec = |c: SymbolId| db.symbol(c).flags.contains(crate::symbols::SymbolFlags::SPECIALIZATION);
            let seed_spec = spec(seed);
            items.extend(db.lookup(db.name(seed)).iter().copied().filter(|&c| db.symbol(c).kind.is_record() && (seed_spec || spec(c)) && db.qualified_name(c) == q));
        }
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
        self.symbol_refs_streaming(path, text, offset, &|_, _| {})
    }

    /// [`Self::symbol_refs`] that also hands each file's `(certain, uncertain)` hits to `on_file` as soon
    /// as that file is scanned, from the worker thread that scanned it. Files with no hits are skipped.
    pub fn symbol_refs_streaming(&self, path: &str, text: &str, offset: u32, on_file: &(dyn Fn(&[Location], &[Location]) + Sync)) -> Option<SymbolRefs> {
        let toks = sig_tokens(text);
        let hit = ident_at(&toks, offset)?;
        if self.local_in(&toks, path, text, hit).is_some() {
            return None;
        }
        let name = toks[hit].text(text);
        let at_decl: Vec<SymbolId> = match self.idx.file_index(path) {
            // The name being declared here is the symbol; no need to guess from its spelling.
            Some(f) => self.idx.db.lookup(name).iter().copied().filter(|&c| self.location(c).is_some_and(|l| l.file as usize == f && l.start == toks[hit].start)).collect(),
            None => Vec::new(),
        };
        let seeds = if at_decl.is_empty() { self.definition_in(&toks, path, text, hit) } else { at_decl };
        if seeds.is_empty() {
            return None;
        }
        let class = kind_class(self.idx.db.symbol(seeds[0]).kind);
        let mut targets = Vec::new();
        for s in seeds {
            self.equivalents(s, &mut targets);
        }
        targets.sort_unstable();
        targets.dedup();
        let mut decl_sites: Vec<Location> = targets.iter().filter_map(|&t| self.location(t)).collect();
        decl_sites.sort_unstable();
        // Declarations of other symbols with this spelling are never occurrences of ours.
        let mut other_sites: Vec<Location> = self.idx.db.lookup(name).iter().filter(|c| targets.binary_search(c).is_err()).filter_map(|&c| self.location(c)).collect();
        other_sites.sort_unstable();

        let files: Vec<(usize, &str)> =
            self.idx.live_files().filter(|(_, f)| f.may_contain(name)).map(|(i, f)| (i, f.path.as_str())).collect();
        PROF[6].store(files.len() as u64, Ordering::Relaxed);
        let per_file: Vec<(Vec<Location>, Vec<Location>)> = files
            .par_iter()
            .map(|&(fi, path)| {
                let (mut certain, mut uncertain) = (Vec::new(), Vec::new());
                let t0 = std::time::Instant::now();
                let Some(t) = self.texts.text(path) else { return (certain, uncertain) };
                let text = t.as_str();
                PROF[0].fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                PROF[3].fetch_add(text.len() as u64, Ordering::Relaxed);
                PROF[4].fetch_add(1, Ordering::Relaxed);
                if memchr::memmem::find(text.as_bytes(), name.as_bytes()).is_none() {
                    return (certain, uncertain);
                }
                PROF[5].fetch_add(1, Ordering::Relaxed);
                let t1 = std::time::Instant::now();
                let toks = sig_tokens(text);
                PROF[1].fetch_add(t1.elapsed().as_nanos() as u64, Ordering::Relaxed);
                let t2 = std::time::Instant::now();
                for (i, tok) in toks.iter().enumerate() {
                    if tok.kind != K::Ident || tok.text(text) != name {
                        continue;
                    }
                    let loc = Location { file: fi as u32, start: tok.start, end: tok.end() };
                    if decl_sites.binary_search_by(|d| (d.file, d.start).cmp(&(loc.file, loc.start))).is_ok() {
                        certain.push(loc);
                        continue;
                    }
                    if other_sites.binary_search_by(|d| (d.file, d.start).cmp(&(loc.file, loc.start))).is_ok() {
                        continue;
                    }
                    let scoped = i > 0 && matches!(toks[i - 1].kind, K::Dot | K::Arrow | K::ColonColon);
                    let tl = std::time::Instant::now();
                    let is_local = !scoped && self.local_in(&toks, path, text, i).is_some();
                    PROF[13].fetch_add(tl.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    if is_local {
                        continue;
                    }
                    if is_label(&toks, text, i) {
                        continue;
                    }
                    let mut r = self.definition_in(&toks, path, text, i);
                    // A different kind of symbol with the same spelling is never this symbol.
                    let had = !r.is_empty();
                    r.retain(|c| targets.binary_search(c).is_ok() || kind_class(self.idx.db.symbol(*c).kind) == class);
                    if had && r.is_empty() {
                        continue;
                    }
                    let inside = r.iter().filter(|c| targets.binary_search(c).is_ok()).count();
                    if r.is_empty() || (inside > 0 && inside < r.len()) {
                        uncertain.push(loc);
                    } else if inside == r.len() {
                        certain.push(loc);
                    }
                }
                PROF[2].fetch_add(t2.elapsed().as_nanos() as u64, Ordering::Relaxed);
                if !certain.is_empty() || !uncertain.is_empty() {
                    on_file(&certain, &uncertain);
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
