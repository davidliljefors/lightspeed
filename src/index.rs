//! The workspace index: every translation unit and every header it reaches,
//! preprocessed, parsed and merged into one flat [`SymbolDb`] plus the
//! [`Hierarchy`], with per-file bookkeeping for incremental updates.
//!
//! Build is embarrassingly parallel: each worker owns its preprocessor and
//! buffers and returns a private shard; shards are merged on one thread in a
//! deterministic order. Updates tombstone a file's symbol range and append a
//! fresh shard, then [`Index::flush`] rebuilds the O(N) lookup structures.

use std::hash::Hasher;

use hashbrown::HashMap;
use rayon::prelude::*;
use rustc_hash::{FxBuildHasher, FxHasher};

use crate::compile_commands::{CompileDb, FlagSet};
use crate::hash::hash_bytes;
use crate::hierarchy::Hierarchy;
use crate::lexer::{Token, TokenKind, lex_into};
use crate::parser::{ParseOutput, parse};
use crate::pp::vfs::{DiskFs, normalize_path, path_key};
use crate::pp::{PpConfig, PpOutput, Preprocessor};
use crate::source::SourceFile;
use crate::symbols::{SymbolDb, SymbolId};

static DISK: DiskFs = DiskFs;

/// Live indexing progress, readable from another thread while [`Index::build_with`] runs.
/// `total` grows as `#include`d headers are discovered.
#[derive(Debug, Default)]
pub struct Progress {
    pub done: std::sync::atomic::AtomicU32,
    pub total: std::sync::atomic::AtomicU32,
}

impl Progress {
    pub fn snapshot(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.done.load(Relaxed), self.total.load(Relaxed))
    }
}

#[derive(Clone, Debug)]
pub struct IndexConfig {
    /// Parse headers reached through `#include` (standalone, with the flags
    /// of the first file that includes them).
    pub index_headers: bool,
    /// Compact once tombstones exceed this fraction (numerator over 100) of all symbols.
    pub compact_percent: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self { index_headers: true, compact_percent: 30 }
    }
}

/// Everything the index remembers about one file.
#[derive(Debug, Default)]
pub struct FileInfo {
    pub path: String,
    /// Compile entry whose flags this file is analysed with.
    pub flag_entry: Option<u32>,
    pub is_tu: bool,
    pub alive: bool,
    /// Half-open range of this file's symbols in the database.
    pub syms: (u32, u32),
    /// Sorted, deduplicated [`ident_hash`]es of every identifier in the file.
    pub idents: Vec<u32>,
    pub content_hash: u64,
    /// [`path_key`]s of the files this one includes directly.
    pub includes: Vec<u64>,
    /// Using-directives and namespace aliases at namespace level.
    pub decls: crate::scope_decls::ScopeDecls,
}

impl FileInfo {
    pub fn may_contain(&self, name: &str) -> bool {
        self.idents.binary_search(&ident_hash(name)).is_ok()
    }
}

#[inline]
pub fn ident_hash(s: &str) -> u32 {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    (h.finish() >> 32) as u32
}

/// Offsets `(start, end)` of every identifier token spelled exactly `name`.
/// `toks` is scratch space.
pub fn find_occurrences(text: &str, name: &str, toks: &mut Vec<Token>, out: &mut Vec<(u32, u32)>) {
    if name.is_empty() || memchr::memmem::find(text.as_bytes(), name.as_bytes()).is_none() {
        return;
    }
    toks.clear();
    lex_into(text, toks);
    for t in toks.iter() {
        if t.kind == TokenKind::Ident && t.text(text) == name {
            out.push((t.start, t.start + t.len));
        }
    }
}

/// Where an index build spent its time. Per-file phase times are CPU-µs summed
/// over all workers (divide by threads for the wall-clock share); the rest is wall time.
#[derive(Default, Clone, Debug)]
pub struct BuildStats {
    pub load_us: u64,
    pub preprocess_us: u64,
    pub parse_us: u64,
    pub lex_idents_us: u64,
    pub scope_us: u64,
    pub waves: u32,
    pub parallel_wall_ms: f64,
    pub absorb_wall_ms: f64,
    pub flush_wall_ms: f64,
    pub slowest: Vec<(u64, String)>,
}

impl BuildStats {
    /// Multi-line human-readable breakdown for `threads` workers.
    pub fn report(&self, threads: usize) -> String {
        let ms = |us: u64| us as f64 / 1e3;
        let mut s = format!(
            "  parallel analyze {:.0} ms wall ({} waves) | serial absorb {:.0} ms | flush (indexes+hierarchy) {:.0} ms\n  cpu total: load {:.0} ms, preprocess {:.0} ms, parse {:.0} ms, lex+idents {:.0} ms, using/alias scan {:.0} ms (÷{threads} threads for wall share)\n  (template params, specializations, bindings and type inference are lazy: they cost nothing at index time)",
            self.parallel_wall_ms,
            self.waves,
            self.absorb_wall_ms,
            self.flush_wall_ms,
            ms(self.load_us),
            ms(self.preprocess_us),
            ms(self.parse_us),
            ms(self.lex_idents_us),
            ms(self.scope_us),
        );
        for (us, p) in self.slowest.iter().take(5) {
            s.push_str(&format!("\n  slow: {:.0} ms {p}", ms(*us)));
        }
        s
    }
}

struct Analysis {
    shard: SymbolDb,
    idents: Vec<u32>,
    includes: Vec<String>,
    decls: crate::scope_decls::ScopeDecls,
    content_hash: u64,
    times: [u32; 5],
}

/// Per-thread analysis state. Nothing here is shared between workers.
struct Worker {
    pp: Preprocessor<'static, DiskFs>,
    out: PpOutput,
    pres: ParseOutput,
    toks: Vec<Token>,
    last_flags: Option<u64>,
}

impl Worker {
    fn new(shared: &std::sync::Arc<crate::pp::SharedHeaders>) -> Self {
        let mut pp = Preprocessor::new(&DISK, PpConfig::default());
        pp.set_shared(shared.clone());
        Self {
            pp,
            out: PpOutput::default(),
            pres: ParseOutput::default(),
            toks: Vec::new(),
            last_flags: None,
        }
    }

    fn warm(&mut self, path: &str, text: &str, flags: &FlagSet, flags_hash: u64) {
        if self.last_flags != Some(flags_hash) {
            self.pp.set_flags(flags);
            self.last_flags = Some(flags_hash);
        }
        self.pp.run(path, text, &mut self.out);
    }

    fn analyze(&mut self, path: &str, text: &str, flags: &FlagSet, flags_hash: u64) -> Analysis {
        if self.last_flags != Some(flags_hash) {
            self.pp.set_flags(flags);
            self.last_flags = Some(flags_hash);
        }
        let t = std::time::Instant::now();
        self.pp.run(path, text, &mut self.out);
        let t_pp = t.elapsed().as_micros() as u32;
        let t = std::time::Instant::now();
        let content_hash = hash_bytes(text.as_bytes());
        let mut shard = SymbolDb::new();
        let file = shard.files.intern(path);
        shard.files.set_content_hash(file, content_hash);
        parse(&self.out, text, &mut shard, file, &mut self.pres);
        let t_parse = t.elapsed().as_micros() as u32;

        let t = std::time::Instant::now();
        self.toks.clear();
        lex_into(text, &mut self.toks);
        let mut idents: Vec<u32> =
            self.toks.iter().filter(|t| t.kind == TokenKind::Ident && t.keyword != crate::lexer::Keyword::Reserved).map(|t| ident_hash(t.text(text))).collect();
        idents.sort_unstable();
        idents.dedup();

        let includes = self
            .out
            .includes
            .iter()
            .filter_map(|r| self.out.include_path(r))
            .map(|p| {
                let mut p = p.to_owned();
                normalize_path(&mut p);
                p
            })
            .collect();
        let t_lex = t.elapsed().as_micros() as u32;
        let t = std::time::Instant::now();
        let mut decls = crate::scope_decls::scan(&self.toks, text, self.toks.len(), true);
        decls.normalize();
        let t_scope = t.elapsed().as_micros() as u32;
        Analysis { shard, idents, includes, decls, content_hash, times: [0, t_pp, t_parse, t_lex, t_scope] }
    }
}

struct Job {
    path: String,
    flag_entry: Option<u32>,
    is_tu: bool,
}

pub struct Index {
    pub db: SymbolDb,
    pub hier: Hierarchy,
    pub files: Vec<FileInfo>,
    pub cdb: CompileDb,
    cfg: IndexConfig,
    by_path: HashMap<u64, u32, FxBuildHasher>,
    worker: Worker,
    shared: std::sync::Arc<crate::pp::SharedHeaders>,
    scope_cache: std::sync::RwLock<std::collections::HashMap<usize, std::sync::Arc<crate::scope_decls::ScopeDecls>>>,
    dirty: bool,
    dead_symbols: usize,
    /// Files that could not be read during the last build.
    pub unreadable: usize,
    pub stats: BuildStats,
    generation: u64,
}

impl Index {
    pub fn empty(cdb: CompileDb, cfg: IndexConfig) -> Self {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut db = SymbolDb::new();
        let hier = Hierarchy::build(&mut db);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let shared = std::sync::Arc::new(crate::pp::SharedHeaders::default());
        let mut worker = Worker::new(&shared);
        worker.pp.set_snapshotting(true);
        Self {
            shared,
            scope_cache: Default::default(),
            db,
            hier,
            files: Vec::new(),
            cdb,
            cfg,
            by_path: HashMap::with_hasher(FxBuildHasher),
            worker,
            dirty: false,
            dead_symbols: 0,
            unreadable: 0,
            stats: BuildStats::default(),
            generation,
        }
    }

    /// Indexes every compile entry and the headers they include, in parallel.
    pub fn build(cdb: CompileDb, cfg: IndexConfig) -> Self {
        Self::build_with(cdb, cfg, None)
    }

    /// [`Self::build`] that publishes how many files are done out of those known so far.
    pub fn build_with(cdb: CompileDb, cfg: IndexConfig, progress: Option<&Progress>) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let mut idx = Self::empty(cdb, cfg);
        let mut jobs: Vec<Job> = Vec::with_capacity(idx.cdb.entries.len());
        let mut seen: hashbrown::HashSet<u64, FxBuildHasher> = Default::default();
        for (i, e) in idx.cdb.entries.iter().enumerate() {
            let mut p = e.file.clone();
            normalize_path(&mut p);
            if seen.insert(path_key(&p)) {
                jobs.push(Job { path: p, flag_entry: Some(i as u32), is_tu: true });
            }
        }
        if let Some(p) = progress {
            p.total.store(jobs.len() as u32, Relaxed);
        }
        while !jobs.is_empty() {
            let t = std::time::Instant::now();
            let results = idx.analyze_jobs(&jobs, progress);
            idx.stats.parallel_wall_ms += t.elapsed().as_secs_f64() * 1e3;
            idx.stats.waves += 1;
            let t = std::time::Instant::now();
            let mut next: Vec<Job> = Vec::new();
            for (job, res) in jobs.iter().zip(results) {
                let Some(a) = res else {
                    idx.unreadable += 1;
                    continue;
                };
                let [l, p, s, x, d] = a.times.map(u64::from);
                let st = &mut idx.stats;
                (st.load_us, st.preprocess_us, st.parse_us, st.lex_idents_us) = (st.load_us + l, st.preprocess_us + p, st.parse_us + s, st.lex_idents_us + x);
                st.scope_us += d;
                st.slowest.push((l + p + s + x + d, job.path.clone()));
                if st.slowest.len() > 64 {
                    st.slowest.sort_unstable_by(|a, b| b.cmp(a));
                    st.slowest.truncate(8);
                }
                let inc = idx.absorb(&job.path, job.flag_entry, job.is_tu, a);
                if idx.cfg.index_headers {
                    for h in inc {
                        if seen.insert(path_key(&h)) {
                            next.push(Job { path: h, flag_entry: job.flag_entry, is_tu: false });
                            if let Some(p) = progress {
                                p.total.fetch_add(1, Relaxed);
                            }
                        }
                    }
                }
            }
            idx.stats.absorb_wall_ms += t.elapsed().as_secs_f64() * 1e3;
            jobs = next;
        }
        let t = std::time::Instant::now();
        idx.flush();
        idx.stats.flush_wall_ms = t.elapsed().as_secs_f64() * 1e3;
        idx.stats.slowest.sort_unstable_by(|a, b| b.cmp(a));
        idx.stats.slowest.truncate(8);
        idx
    }

    fn analyze_jobs(&self, jobs: &[Job], progress: Option<&Progress>) -> Vec<Option<Analysis>> {
        use std::cell::RefCell;
        // One long-lived worker per pool thread (rayon's `map_init` would create
        // one per work split and throw the header caches away). A new build
        // generation drops stale caches in case files changed in between.
        thread_local! {
            static WORKER: RefCell<Option<(u64, Worker)>> = const { RefCell::new(None) };
        }
        let generation = self.generation;
        let shared = &self.shared;
        let cdb = &self.cdb;
        jobs.par_iter()
            .map(|j| {
                let r = (|| {
                    let t = std::time::Instant::now();
                    let src = SourceFile::load(&j.path).ok()?;
                    let load = t.elapsed().as_micros() as u32;
                    let (flags, hash) = flags_of(cdb, j.flag_entry);
                    WORKER.with(|cell| {
                        let mut slot = cell.borrow_mut();
                        let (g, w) = slot.get_or_insert_with(|| (generation, Worker::new(shared)));
                        if *g != generation {
                            *w = Worker::new(shared);
                            *g = generation;
                        }
                        let mut a = w.analyze(&j.path, src.text(), flags, hash);
                        a.times[0] = load;
                        Some(a)
                    })
                })();
                if let Some(p) = progress {
                    p.done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                r
            })
            .collect()
    }

    /// Registers an analysed file, tombstoning any previous version. Returns
    /// the included paths that are not indexed yet.
    fn absorb(&mut self, path: &str, flag_entry: Option<u32>, is_tu: bool, a: Analysis) -> Vec<String> {
        let key = path_key(path);
        if let Some(&i) = self.by_path.get(&key)
            && self.files[i as usize].alive
        {
            let (s, e) = self.files[i as usize].syms;
            // Edits that only move declarations leave every index valid.
            if self.db.patch_same_shape(s as usize, e as usize, &a.shard) {
                if let Some(f) = self.db.files.lookup(path) {
                    self.db.files.set_content_hash(f, a.content_hash);
                }
                let f = &mut self.files[i as usize];
                f.path = path.to_owned();
                f.flag_entry = flag_entry;
                f.is_tu |= is_tu;
                f.idents = a.idents;
                f.content_hash = a.content_hash;
                f.includes = a.includes.iter().map(|p| path_key(p)).collect();
                f.decls = a.decls;
                self.scope_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
                return a.includes.into_iter().filter(|p| !self.by_path.contains_key(&path_key(p))).collect();
            }
        }
        let slot = match self.by_path.get(&key) {
            Some(&i) => {
                self.kill(i as usize);
                i as usize
            }
            None => {
                self.files.push(FileInfo::default());
                self.by_path.insert(key, (self.files.len() - 1) as u32);
                self.files.len() - 1
            }
        };
        let base = self.db.merge(&a.shard);
        let f = &mut self.files[slot];
        f.path = path.to_owned();
        f.flag_entry = flag_entry;
        f.is_tu |= is_tu;
        f.alive = true;
        f.syms = (base as u32, (base + a.shard.len()) as u32);
        f.idents = a.idents;
        f.content_hash = a.content_hash;
        f.includes = a.includes.iter().map(|p| path_key(p)).collect();
        f.decls = a.decls;
        self.dirty = true;
        self.scope_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        a.includes.into_iter().filter(|p| !self.by_path.contains_key(&path_key(p))).collect()
    }

    fn kill(&mut self, slot: usize) {
        let f = &mut self.files[slot];
        if f.alive && f.syms.1 > f.syms.0 {
            self.db.kill_range(f.syms.0 as usize, f.syms.1 as usize);
            self.dead_symbols += (f.syms.1 - f.syms.0) as usize;
        }
        f.syms = (0, 0);
        f.idents = Vec::new();
        f.includes = Vec::new();
        f.decls = Default::default();
        f.alive = false;
        self.dirty = true;
        self.scope_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Using-directives and namespace aliases that files included (directly
    /// or not) by `file` bring into scope. Cached until any file changes.
    pub fn included_decls(&self, file: usize) -> std::sync::Arc<crate::scope_decls::ScopeDecls> {
        if let Ok(c) = self.scope_cache.read()
            && let Some(d) = c.get(&file)
        {
            return d.clone();
        }
        let mut out = crate::scope_decls::ScopeDecls::default();
        let mut seen: hashbrown::HashSet<usize, FxBuildHasher> = Default::default();
        let mut stack = vec![file];
        while let Some(f) = stack.pop() {
            if !seen.insert(f) {
                continue;
            }
            let info = &self.files[f];
            if f != file {
                out.merge(&info.decls);
            }
            stack.extend(info.includes.iter().filter_map(|k| self.by_path.get(k)).map(|&i| i as usize).filter(|&i| self.files[i].alive));
        }
        out.normalize();
        let d = std::sync::Arc::new(out);
        if let Ok(mut c) = self.scope_cache.write() {
            c.insert(file, d.clone());
        }
        d
    }

    pub fn file_index(&self, path: &str) -> Option<usize> {
        let mut p = path.to_owned();
        normalize_path(&mut p);
        self.by_path.get(&path_key(&p)).map(|&i| i as usize).filter(|&i| self.files[i].alive)
    }

    /// Entry whose directory shares the longest prefix with `path`: the best
    /// guess at flags for a file the compile database does not list.
    fn nearest_entry(&self, path: &str) -> Option<u32> {
        let common = |a: &str, b: &str| a.bytes().zip(b.bytes()).take_while(|(x, y)| x.eq_ignore_ascii_case(y)).count();
        self.cdb.entries.iter().enumerate().max_by_key(|(_, e)| common(&e.file, path)).map(|(i, _)| i as u32)
    }

    /// Re-indexes `path`. `text` overrides the disk contents (unsaved
    /// buffer); `None` reads the file, and removes it from the index if it is gone.
    /// Newly reachable headers are indexed too. Call [`Self::flush`] before querying.
    pub fn update(&mut self, path: &str, text: Option<&str>) {
        let mut root = path.to_owned();
        normalize_path(&mut root);
        let mut pending: Vec<(String, Option<u32>)> = vec![(root, None)];
        let mut first = true;
        while let Some((p, inherited)) = pending.pop() {
            crate::pp::vfs::invalidate_dir_of(&p);
            let loaded;
            let src = match text.filter(|_| first) {
                Some(t) => t,
                None => match SourceFile::load(&p) {
                    Ok(s) => {
                        loaded = s;
                        loaded.text()
                    }
                    Err(_) => {
                        self.remove(&p);
                        first = false;
                        continue;
                    }
                },
            };
            first = false;
            let existing = self.by_path.get(&path_key(&p)).map(|&i| i as usize);
            let (flag_entry, is_tu) = match existing {
                Some(i) => (self.files[i].flag_entry, self.files[i].is_tu),
                None => {
                    let entry = inherited.or_else(|| self.cdb.index_of(&p).map(|i| i as u32)).or_else(|| self.nearest_entry(&p));
                    (entry, self.cdb.get(&p).is_some())
                }
            };
            self.worker.pp.invalidate_file(path_key(&p));
            let (flags, hash) = flags_of(&self.cdb, flag_entry);
            let a = self.worker.analyze(&p, src, flags, hash);
            let inc = self.absorb(&p, flag_entry, is_tu, a);
            if self.cfg.index_headers {
                pending.extend(inc.into_iter().map(|h| (h, flag_entry)));
            }
        }
    }

    /// Pre-processes `path` so the first edit after opening it is as cheap as
    /// the later ones (header directives are cached, the prologue snapshotted).
    /// Does not touch the symbol database.
    pub fn warm(&mut self, path: &str, text: &str) {
        let mut p = path.to_owned();
        normalize_path(&mut p);
        let flag_entry = match self.by_path.get(&path_key(&p)) {
            Some(&i) => self.files[i as usize].flag_entry,
            None => self.cdb.index_of(&p).map(|i| i as u32).or_else(|| self.nearest_entry(&p)),
        };
        let (flags, hash) = flags_of(&self.cdb, flag_entry);
        self.worker.warm(&p, text, flags, hash);
    }

    pub fn remove(&mut self, path: &str) {
        if let Some(i) = self.file_index(path) {
            self.kill(i);
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Rebuilds name/children indexes and the hierarchy if anything changed,
    /// compacting first when tombstones pile up. O(symbols).
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        if self.dead_symbols * 100 > self.db.len() * self.cfg.compact_percent {
            let map = self.db.compact();
            for f in self.files.iter_mut().filter(|f| f.alive) {
                f.syms = (map[f.syms.0 as usize], map[f.syms.1 as usize]);
            }
            self.dead_symbols = 0;
        }
        self.db.rebuild_indexes();
        self.hier = Hierarchy::build(&mut self.db);
        self.dirty = false;
    }

    /// Live symbols declared in file `fi`.
    pub fn file_symbols(&self, fi: usize) -> impl Iterator<Item = SymbolId> {
        let (a, b) = self.files[fi].syms;
        (a..b).map(|i| SymbolId::from_usize(i as usize))
    }

    pub fn live_files(&self) -> impl Iterator<Item = (usize, &FileInfo)> {
        self.files.iter().enumerate().filter(|(_, f)| f.alive)
    }

    pub fn dead_symbols(&self) -> usize {
        self.dead_symbols
    }
}

use crate::ids::Idx;

fn flags_of(cdb: &CompileDb, entry: Option<u32>) -> (&FlagSet, u64) {
    static DEFAULT: std::sync::OnceLock<FlagSet> = std::sync::OnceLock::new();
    match entry {
        Some(i) => {
            let e = &cdb.entries[i as usize];
            (cdb.flags(e), e.flags_hash)
        }
        None => (DEFAULT.get_or_init(FlagSet::default), 0),
    }
}
