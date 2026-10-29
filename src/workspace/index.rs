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

use crate::workspace::compile_commands::{CompileDb, FlagSet};
use crate::base::hash::hash_bytes;
use crate::hierarchy::Hierarchy;
use crate::lexer::{Token, TokenKind, lex_into};
use crate::parser::{ParseOutput, parse};
use crate::pp::vfs::{DiskFs, normalize_path, path_key};
use crate::pp::{PpConfig, PpOutput, Preprocessor};
use crate::base::source::SourceFile;
use crate::store::{self, CacheConfig, Probe, RecSrc, RecordParts, Snapshot};
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
    /// Reuse and refresh the on-disk cache (see [`crate::store`]).
    pub cache: Option<CacheConfig>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self { index_headers: true, compact_percent: 30, cache: None }
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

/// What the disk cache did during a build.
#[derive(Default, Clone, Debug)]
pub struct CacheStats {
    /// `valid`, or why the snapshot was not used (`Missing`, `Stale`, `Corrupt(..)`).
    pub status: String,
    pub open: store::OpenStats,
    /// Files whose record was reused.
    pub hits: u32,
    /// Files with no record (new, or analysed with other flags).
    pub absent: u32,
    /// Files whose record depended on something that changed.
    pub dirty: u32,
    pub bad: u32,
    pub decode_us: u64,
    pub encode_us: u64,
    pub saved_bytes: u64,
    pub save_ms: f64,
    /// Compressed bytes of the records written this build, and the largest one.
    pub fresh_bytes: u64,
    pub biggest_rec: (u64, String),
}

impl CacheStats {
    pub fn report(&self) -> String {
        let o = &self.open;
        let mut s = format!(
            "  cache {}: validated {} records ({} files hashed, {} dirs listed, {} dirty keys) in {:.0} ms | {} reused, {} changed deps, {} new, {} corrupt | decode {:.0} ms cpu, encode {:.0} ms cpu",
            self.status,
            o.records,
            o.files_checked,
            o.dirs_checked,
            o.dirty_keys,
            o.millis,
            self.hits,
            self.dirty,
            self.absent,
            self.bad,
            self.decode_us as f64 / 1e3,
            self.encode_us as f64 / 1e3,
        );
        if self.fresh_bytes > 0 {
            s.push_str(&format!(" | new records {} (largest {}: {})", crate::base::mem::mib(self.fresh_bytes as usize), crate::base::mem::mib(self.biggest_rec.0 as usize), self.biggest_rec.1));
        }
        if self.saved_bytes > 0 {
            s.push_str(&format!(" | saved {} in {:.0} ms", crate::base::mem::mib(self.saved_bytes as usize), self.save_ms));
        }
        s
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
    /// Largest sum of per-file results held at once between a wave and its absorb.
    pub peak_wave_bytes: usize,
    /// Largest single file result (bytes, path) seen in any wave.
    pub biggest_result: (usize, String),
    /// Resident bytes right after the parallel phase of the last wave.
    pub rss_after_analyze: usize,
    pub waves: u32,
    pub parallel_wall_ms: f64,
    pub absorb_wall_ms: f64,
    pub flush_wall_ms: f64,
    pub slowest: Vec<(u64, String)>,
    pub cache: Option<CacheStats>,
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
        if let Some(c) = &self.cache {
            s.push('\n');
            s.push_str(&c.report());
        }
        for (us, p) in self.slowest.iter().take(5) {
            s.push_str(&format!("\n  slow: {:.0} ms {p}", ms(*us)));
        }
        s
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Off,
    Hit,
    Absent,
    Dirty,
    Bad,
}

struct Analysis {
    shard: SymbolDb,
    idents: Vec<u32>,
    includes: Vec<String>,
    decls: crate::scope_decls::ScopeDecls,
    content_hash: u64,
    /// load, preprocess, parse, lex+idents, using/alias scan, record encode (µs).
    times: [u32; 6],
    decode_us: u32,
    outcome: Outcome,
    /// Cache record key and bytes, when caching.
    rec: Option<(u64, RecSrc)>,
}

impl Analysis {
    /// Approximate heap bytes held until this result is absorbed.
    fn heap_bytes(&self) -> usize {
        let rec = match &self.rec {
            Some((_, RecSrc::Fresh(b))) => b.capacity(),
            _ => 0,
        };
        self.shard.heap_bytes() + self.idents.capacity() * 4 + self.includes.iter().map(|s| s.capacity() + 24).sum::<usize>() + self.decls.heap_bytes() + rec
    }
}

/// Per-thread analysis state. Nothing here is shared between workers.
struct Worker {
    pp: Preprocessor<'static, DiskFs>,
    out: PpOutput,
    pres: ParseOutput,
    toks: Vec<Token>,
    last_flags: Option<u64>,
    /// What this worker last added to [`WORKER_MEM`].
    published: [usize; 5],
}

/// Retained heap bytes summed over all live workers: include-resolution memo,
/// macro tables, header/guard caches, preprocessor output buffers, scratch tokens.
static WORKER_MEM: [std::sync::atomic::AtomicUsize; 5] = [const { std::sync::atomic::AtomicUsize::new(0) }; 5];
const WORKER_MEM_LABELS: [&str; 5] = ["include-resolve memo", "macro tables", "header/guard caches", "pp output buffers", "token scratch"];

/// A worker whose preprocessor output buffers grew past this after a file frees them.
const OUTPUT_TRIM_BYTES: usize = 48 << 20;
/// Files analyzed per wave: enough to keep 64+ cores busy through the tail, small enough to cap held results.
const WAVE_JOBS: usize = 4096;
static OUTPUT_TRIMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for Worker {
    fn drop(&mut self) {
        for (m, p) in WORKER_MEM.iter().zip(self.published) {
            m.fetch_sub(p, std::sync::atomic::Ordering::Relaxed);
        }
    }
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
            published: [0; 5],
        }
    }

    fn publish_memory(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let (resolve, macros, caches) = self.pp.heap_breakdown();
        let now = [resolve, macros, caches, self.out.heap_bytes(), self.toks.capacity() * std::mem::size_of::<Token>()];
        for ((m, p), n) in WORKER_MEM.iter().zip(&mut self.published).zip(now) {
            if n >= *p {
                m.fetch_add(n - *p, Relaxed);
            } else {
                m.fetch_sub(*p - n, Relaxed);
            }
            *p = n;
        }
    }

    fn warm(&mut self, path: &str, text: &str, flags: &FlagSet, flags_hash: u64) {
        if self.last_flags != Some(flags_hash) {
            self.pp.set_flags(flags);
            self.last_flags = Some(flags_hash);
        }
        self.pp.run(path, text, &mut self.out);
    }

    fn analyze(&mut self, path: &str, text: &str, flags: &FlagSet, flags_hash: u64, rec_fp: Option<u64>) -> Analysis {
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

        let includes: Vec<String> = self
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
        let t = std::time::Instant::now();
        let rec = rec_fp.map(|fp| {
            let mut buf = Vec::new();
            store::encode_record(
                &RecordParts { path, flags_fp: fp, content_hash, deps: &self.out.deps, idents: &idents, includes: &includes, decls: &decls, shard: &shard },
                &mut buf,
            );
            buf.shrink_to_fit();
            (store::record_key(path_key(path), fp), RecSrc::Fresh(buf))
        });
        let t_enc = t.elapsed().as_micros() as u32;
        let outcome = if rec.is_some() { Outcome::Absent } else { Outcome::Off };
        let a = Analysis { shard, idents, includes, decls, content_hash, times: [0, t_pp, t_parse, t_lex, t_scope, t_enc], decode_us: 0, outcome, rec };
        if self.out.heap_bytes() > OUTPUT_TRIM_BYTES {
            OUTPUT_TRIMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.out.trim(OUTPUT_TRIM_BYTES);
        }
        if self.toks.capacity() > 1 << 22 {
            self.toks = Vec::new();
        }
        self.publish_memory();
        a
    }
}

struct Job {
    path: String,
    flag_entry: Option<u32>,
    is_tu: bool,
}

/// A resolved `#include` in an open buffer: the byte range of the header name (without its delimiters) and the file it names.
pub struct IncludeLink {
    pub start: u32,
    pub end: u32,
    pub path: String,
}

/// Byte range of the name in the `#include "x"` / `#include <x>` directive whose `#` is at `hash`.
pub fn include_name_span(text: &str, hash: u32) -> Option<(u32, u32)> {
    let rest = text.get(hash as usize..)?;
    let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
    let after_hash = line.strip_prefix('#')?.trim_start();
    let after_kw = after_hash.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_').trim_start();
    let close = match after_kw.chars().next()? {
        '"' => '"',
        '<' => '>',
        _ => return None,
    };
    let start = line.len() - after_kw.len() + 1;
    let len = line[start..].find(close)?;
    Some((hash + start as u32, hash + (start + len) as u32))
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
    reach_cache: std::sync::RwLock<std::collections::HashMap<usize, std::sync::Arc<hashbrown::HashSet<u32, FxBuildHasher>>>>,
    pub(crate) param_cache: crate::query::types::ParamCache,
    dirty: bool,
    dead_symbols: usize,
    /// Files that could not be read during the last build.
    pub unreadable: usize,
    pub stats: BuildStats,
    generation: u64,
    flag_fps: Vec<u64>,
    default_fp: u64,
    saver: Option<std::thread::JoinHandle<()>>,
    /// Include links of the open buffers, keyed by [`path_key`].
    links: HashMap<u64, Vec<IncludeLink>, FxBuildHasher>,
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
            reach_cache: Default::default(),
            param_cache: Default::default(),
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
            flag_fps: Vec::new(),
            default_fp: 0,
            saver: None,
            links: HashMap::with_hasher(FxBuildHasher),
        }
    }

    /// Records the resolved includes of the file the worker just preprocessed.
    fn capture_links(&mut self, path: &str, text: &str) {
        let out = &self.worker.out;
        let links = out
            .includes
            .iter()
            .filter_map(|r| {
                let target = out.include_path(r)?;
                let (start, end) = include_name_span(text, r.loc)?;
                let mut path = target.to_owned();
                normalize_path(&mut path);
                Some(IncludeLink { start, end, path })
            })
            .collect();
        self.links.insert(path_key(path), links);
    }

    /// Resolved includes of an open buffer, in source order.
    pub fn include_links(&self, path: &str) -> &[IncludeLink] {
        let mut p = path.to_owned();
        normalize_path(&mut p);
        self.links.get(&path_key(&p)).map_or(&[], Vec::as_slice)
    }

    pub fn forget_links(&mut self, path: &str) {
        let mut p = path.to_owned();
        normalize_path(&mut p);
        self.links.remove(&path_key(&p));
    }

    /// Blocks until a background cache write (if any) finished.
    pub fn wait_for_cache(&mut self) {
        if let Some(h) = self.saver.take() {
            let _ = h.join();
        }
    }

    fn fp_of(&self, entry: Option<u32>) -> u64 {
        match entry {
            Some(i) => self.flag_fps[self.cdb.entries[i as usize].flags.index()],
            None => self.default_fp,
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
        // Bulk build: skip per-file patching; one flush builds the indexes and hierarchy.
        idx.dirty = true;
        idx.flag_fps = idx.cdb.flag_sets.iter().map(FlagSet::hash).collect();
        idx.default_fp = FlagSet::default().hash();
        let cache_cfg = idx.cfg.cache.clone();
        let mut cstats = cache_cfg.as_ref().map(|_| CacheStats::default());
        let mut snapshot: Option<Snapshot> = None;
        if let (Some(c), Some(cs)) = (&cache_cfg, cstats.as_mut()) {
            match Snapshot::open(&c.file(), store::fingerprint()) {
                Ok(s) => {
                    cs.status = "valid".into();
                    cs.open = s.stats.clone();
                    snapshot = Some(s);
                }
                Err(e) => cs.status = format!("{e:?}"),
            }
        }
        let mut recs: Vec<(u64, RecSrc)> = Vec::new();
        // Fresh records stream straight into the next snapshot instead of piling up here.
        let mut spill = cache_cfg.as_ref().and_then(|c| store::Spill::create(&c.file(), store::fingerprint()).ok());
        let mut spill_failed = false;
        let mut jobs: Vec<Job> = Vec::with_capacity(idx.cdb.entries.len());
        let mut seen: hashbrown::HashSet<u64, FxBuildHasher> = Default::default();
        for (i, e) in idx.cdb.entries.iter().enumerate() {
            let mut p = e.file.clone();
            normalize_path(&mut p);
            if seen.insert(path_key(&p)) {
                jobs.push(Job { path: p, flag_entry: Some(i as u32), is_tu: true });
            }
        }
        // Same flag sets back to back: a worker then rolls its macro table back
        // between TUs instead of recopying the (possibly large) base each time.
        jobs.sort_by_key(|j| j.flag_entry.map_or(0, |i| idx.cdb.entries[i as usize].flags.index()));
        if let Some(p) = progress {
            p.total.store(jobs.len() as u32, Relaxed);
        }
        while !jobs.is_empty() {
            // Bound the results held between analyze and absorb.
            let mut rest = jobs.split_off(jobs.len().min(WAVE_JOBS));
            let t = std::time::Instant::now();
            let results = idx.analyze_jobs(&jobs, progress, snapshot.as_ref(), cache_cfg.is_some());
            idx.stats.parallel_wall_ms += t.elapsed().as_secs_f64() * 1e3;
            idx.stats.waves += 1;
            let wave_bytes: usize = results.iter().flatten().map(Analysis::heap_bytes).sum();
            idx.stats.peak_wave_bytes = idx.stats.peak_wave_bytes.max(wave_bytes);
            if let Some((b, j)) = results.iter().zip(&jobs).filter_map(|(r, j)| Some((r.as_ref()?.heap_bytes(), j))).max_by_key(|x| x.0)
                && b > idx.stats.biggest_result.0
            {
                idx.stats.biggest_result = (b, j.path.clone());
            }
            idx.stats.rss_after_analyze = crate::base::mem::process_bytes().0.max(idx.stats.rss_after_analyze);
            let t = std::time::Instant::now();
            let mut next: Vec<Job> = Vec::new();
            for (job, res) in jobs.iter().zip(results) {
                let Some(a) = res else {
                    idx.unreadable += 1;
                    continue;
                };
                let mut a = a;
                let [l, p, s, x, d, enc] = a.times.map(u64::from);
                if let Some(cs) = cstats.as_mut() {
                    match a.outcome {
                        Outcome::Hit => cs.hits += 1,
                        Outcome::Dirty => cs.dirty += 1,
                        Outcome::Bad => cs.bad += 1,
                        _ => cs.absent += 1,
                    }
                    cs.decode_us += u64::from(a.decode_us);
                    cs.encode_us += enc;
                }
                if let Some((k, src)) = a.rec.take() {
                    if let (RecSrc::Fresh(b), Some(cs)) = (&src, cstats.as_mut()) {
                        cs.fresh_bytes += b.len() as u64;
                        if b.len() as u64 > cs.biggest_rec.0 {
                            cs.biggest_rec = (b.len() as u64, job.path.clone());
                        }
                    }
                    match (src, spill.as_mut()) {
                        (RecSrc::Fresh(b), Some(s)) => match s.push(&b) {
                            Ok((off, len)) => recs.push((k, RecSrc::Written { off, len })),
                            Err(_) => spill_failed = true,
                        },
                        (src, _) => recs.push((k, src)),
                    }
                }
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
            rest.append(&mut next);
            jobs = rest;
        }
        let t = std::time::Instant::now();
        idx.flush();
        idx.stats.flush_wall_ms = t.elapsed().as_secs_f64() * 1e3;
        if let (Some(c), Some(cs)) = (cache_cfg, cstats) {
            idx.stats.cache = Some(cs);
            if spill_failed {
                crate::log!("lightspeed: cache not saved: cannot write {}", c.file().display());
            } else {
                idx.save_cache(&c, snapshot, recs, spill);
            }
        }
        idx.stats.slowest.sort_unstable_by(|a, b| b.cmp(a));
        idx.stats.slowest.truncate(8);
        idx
    }

    fn analyze_jobs(&self, jobs: &[Job], progress: Option<&Progress>, snap: Option<&Snapshot>, caching: bool) -> Vec<Option<Analysis>> {
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
                    let fp = self.fp_of(j.flag_entry);
                    let mut outcome = Outcome::Absent;
                    if let Some(s) = snap {
                        let t = std::time::Instant::now();
                        match s.probe(&j.path, path_key(&j.path), fp) {
                            Probe::Hit { data, off, len } => {
                                let d = *data;
                                return Some(Analysis {
                                    shard: d.shard,
                                    idents: d.idents,
                                    includes: d.includes,
                                    decls: d.decls,
                                    content_hash: d.content_hash,
                                    times: [0; 6],
                                    decode_us: t.elapsed().as_micros() as u32,
                                    outcome: Outcome::Hit,
                                    rec: Some((store::record_key(path_key(&j.path), fp), RecSrc::Old { off, len })),
                                });
                            }
                            Probe::Dirty => outcome = Outcome::Dirty,
                            Probe::Bad => outcome = Outcome::Bad,
                            Probe::Absent => {}
                        }
                    }
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
                        let mut a = w.analyze(&j.path, src.text(), flags, hash, caching.then_some(fp));
                        a.times[0] = load;
                        if caching {
                            a.outcome = outcome;
                        }
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

    /// Writes the next snapshot: records from this build, plus the content
    /// hash of every file any record depends on and the listing of every
    /// directory probed. Skipped when nothing changed.
    fn save_cache(&mut self, cfg: &CacheConfig, snapshot: Option<Snapshot>, recs: Vec<(u64, RecSrc)>, spill: Option<store::Spill>) {
        let unchanged = snapshot.as_ref().is_some_and(|s| s.record_count() == recs.len())
            && self.stats.cache.as_ref().is_some_and(|c| c.hits as usize == recs.len());
        if unchanged {
            return;
        }
        let mut files: Vec<(u64, String, u64)> = Vec::new();
        if let Some(s) = &snapshot {
            files.extend(s.carried_files().iter().cloned());
        }
        files.extend(self.shared.files());
        files.extend(self.files.iter().filter(|f| f.alive).map(|f| (path_key(&f.path), f.path.clone(), f.content_hash)));
        // The same file seen with two contents means it changed mid-build: never trust it.
        files.sort_unstable_by_key(|f| f.0);
        let mut out: Vec<(u64, String, u64)> = Vec::with_capacity(files.len());
        for f in files {
            match out.last_mut() {
                Some(l) if l.0 == f.0 => {
                    if l.2 != f.2 {
                        l.2 = store::UNTRUSTED;
                    }
                }
                _ => out.push(f),
            }
        }
        let dirs = crate::pp::vfs::dir_snapshot();
        let job = store::SaveJob {
            path: cfg.file(),
            fingerprint: store::fingerprint(),
            old: snapshot.map(Snapshot::into_map),
            spill,
            recs,
            files: out,
            dirs,
        };
        if cfg.background_save {
            self.saver = Some(std::thread::spawn(move || {
                let t = std::time::Instant::now();
                match store::save(job) {
                    Ok(b) => crate::log!("lightspeed: cache saved, {} in {:.0} ms", crate::base::mem::mib(b as usize), t.elapsed().as_secs_f64() * 1e3),
                    Err(e) => crate::log!("lightspeed: cache save failed: {e}"),
                }
            }));
        } else {
            let t = std::time::Instant::now();
            let bytes = store::save(job).unwrap_or(0);
            if let Some(c) = self.stats.cache.as_mut() {
                c.saved_bytes = bytes;
                c.save_ms = t.elapsed().as_secs_f64() * 1e3;
            }
        }
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
                let includes: Vec<_> = a.includes.iter().map(|p| path_key(p)).collect();
                // The include-graph caches are costly to refill, so keep them
                // when the file's includes and scope declarations are unchanged.
                let graph_changed = f.includes != includes || f.decls != a.decls;
                f.includes = includes;
                f.decls = a.decls;
                if graph_changed {
                    self.scope_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
                    self.reach_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
                }
                self.param_cache.clear();
                return a.includes.into_iter().filter(|p| !self.by_path.contains_key(&path_key(p))).collect();
            }
        }
        let was_clean = !self.dirty;
        let t0 = std::time::Instant::now();
        let mut old_range = 0..0;
        let slot = match self.by_path.get(&key) {
            Some(&i) => {
                let f = &self.files[i as usize];
                if f.alive {
                    old_range = f.syms.0 as usize..f.syms.1 as usize;
                }
                self.kill(i as usize);
                i as usize
            }
            None => {
                self.files.push(FileInfo::default());
                self.by_path.insert(key, (self.files.len() - 1) as u32);
                self.files.len() - 1
            }
        };
        let kill_ms = t0.elapsed().as_secs_f64() * 1e3;
        let t1 = std::time::Instant::now();
        let base = self.db.merge(&a.shard);
        let merge_ms = t1.elapsed().as_secs_f64() * 1e3;
        let n_new = a.shard.len();
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
        let t2 = std::time::Instant::now();
        let old_len = old_range.len();
        let patched = was_clean;
        if patched {
            self.patch_in_place(old_range, base..base + n_new);
        }
        self.dirty = !patched;
        crate::log!(
            "lightspeed: absorb {path}: kill {kill_ms:.1} ms, merge {merge_ms:.1} ms, patch {:.1} ms ({} old / {n_new} new symbols, {}; dead {} of {})",
            t2.elapsed().as_secs_f64() * 1e3,
            old_len,
            if patched { "patched in place" } else { "skipped: index already dirty" },
            self.dead_symbols,
            self.db.len(),
        );
        self.scope_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        self.reach_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        self.param_cache.clear();
        a.includes.into_iter().filter(|p| !self.by_path.contains_key(&path_key(p))).collect()
    }

    /// Updates the side indexes and hierarchy for one re-merged file without
    /// a global rebuild. Only valid when the indexes were fresh before the file was killed.
    fn patch_in_place(&mut self, old: std::ops::Range<usize>, new: std::ops::Range<usize>) {
        let t = std::time::Instant::now();
        self.db.patch_indexes(old.clone(), new.clone());
        let idx_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        self.hier.patch(&mut self.db, old, new);
        crate::log!("lightspeed: patch: indexes {idx_ms:.1} ms, hierarchy {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
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
        self.reach_cache.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        self.param_cache.clear();
    }

    /// Using-directives and namespace aliases that files included (directly
    /// or not) by `file` bring into scope. Cached until any file changes.
    /// Approximate memory held by each part of the index, plus process RSS.
    pub fn memory_report(&self) -> String {
        use crate::base::mem::{mib, process_bytes};
        let (hn, htext, htoks) = self.shared.stats();
        let files: usize = self
            .files
            .iter()
            .map(|f| f.path.capacity() + f.idents.capacity() * 4 + f.includes.capacity() * 8 + f.decls.heap_bytes() + std::mem::size_of::<FileInfo>())
            .sum();
        let idents: usize = self.files.iter().map(|f| f.idents.len()).sum();
        let (rss, peak) = process_bytes();
        let mut s = format!(
            "  memory: rss {} (peak {}, after analyze {}) | peak wave results {} (biggest file {}: {})\n  header cache: {hn} entries, text {}, tokens {}\n  symbol db {}:",
            mib(rss),
            mib(peak),
            mib(self.stats.rss_after_analyze),
            mib(self.stats.peak_wave_bytes),
            mib(self.stats.biggest_result.0),
            self.stats.biggest_result.1,
            mib(htext),
            mib(htoks),
            mib(self.db.heap_bytes()),
        );
        for (label, b) in self.db.heap_breakdown() {
            s.push_str(&format!(" {label} {},", mib(b)));
        }
        s.push_str(&format!("\n  hierarchy {} | per-file info {} ({} files, {idents} ident hashes)", mib(self.hier.heap_bytes()), mib(files), self.files.len()));
        s.push_str("\n  workers (all threads):");
        for (label, m) in WORKER_MEM_LABELS.iter().zip(&WORKER_MEM) {
            s.push_str(&format!(" {label} {},", mib(m.load(std::sync::atomic::Ordering::Relaxed))));
        }
        s.push_str(&format!(" output trims {}", OUTPUT_TRIMS.load(std::sync::atomic::Ordering::Relaxed)));
        s
    }

    /// Live files that `file` includes directly.
    pub fn included_files(&self, file: usize) -> impl Iterator<Item = usize> + '_ {
        self.files[file].includes.iter().filter_map(|k| self.by_path.get(k)).map(|&i| i as usize).filter(|&i| self.files[i].alive)
    }

    /// `file` and every file it includes, directly or not.
    pub fn reachable_files(&self, file: usize) -> std::sync::Arc<hashbrown::HashSet<u32, FxBuildHasher>> {
        if let Ok(c) = self.reach_cache.read()
            && let Some(r) = c.get(&file)
        {
            return r.clone();
        }
        let mut seen: hashbrown::HashSet<u32, FxBuildHasher> = Default::default();
        let mut stack = vec![file];
        while let Some(f) = stack.pop() {
            if !seen.insert(f as u32) {
                continue;
            }
            stack.extend(self.files[f].includes.iter().filter_map(|k| self.by_path.get(k)).map(|&i| i as usize).filter(|&i| self.files[i].alive));
        }
        let r = std::sync::Arc::new(seen);
        if let Ok(mut c) = self.reach_cache.write() {
            c.insert(file, r.clone());
        }
        r
    }

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
            let is_root = std::mem::replace(&mut first, false);
            let existing = self.by_path.get(&path_key(&p)).map(|&i| i as usize);
            let (flag_entry, is_tu) = match existing {
                Some(i) => (self.files[i].flag_entry, self.files[i].is_tu),
                None => {
                    let entry = inherited.or_else(|| self.cdb.index_of(&p).map(|i| i as u32)).or_else(|| self.nearest_entry(&p));
                    (entry, self.cdb.get(&p).is_some())
                }
            };
            self.worker.pp.invalidate_path(&p);
            let (flags, hash) = flags_of(&self.cdb, flag_entry);
            let t = std::time::Instant::now();
            let a = self.worker.analyze(&p, src, flags, hash, None);
            if is_root {
                self.capture_links(&p, src);
            }
            let analyze_ms = t.elapsed().as_secs_f64() * 1e3;
            let [_, pp_us, parse_us, lex_us, scope_us, _] = a.times;
            let t = std::time::Instant::now();
            let was_dirty = self.dirty;
            let inc = self.absorb(&p, flag_entry, is_tu, a);
            crate::log!(
                "lightspeed: reindex {p}: analyze {analyze_ms:.1} ms (preprocess {:.1}, parse {:.1}, lex {:.1}, scopes {:.1}), absorb {:.1} ms, {}",
                pp_us as f64 / 1e3,
                parse_us as f64 / 1e3,
                lex_us as f64 / 1e3,
                scope_us as f64 / 1e3,
                t.elapsed().as_secs_f64() * 1e3,
                if self.dirty && !was_dirty { "shape changed (full index rebuild pending)" } else { "same shape or already dirty" },
            );
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
        self.capture_links(&p, text);
    }

    pub fn remove(&mut self, path: &str) {
        if let Some(i) = self.file_index(path) {
            let was_clean = !self.dirty;
            let old = self.files[i].syms.0 as usize..self.files[i].syms.1 as usize;
            self.kill(i);
            if was_clean {
                let end = self.db.len();
                self.patch_in_place(old, end..end);
                self.dirty = false;
            }
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Housekeeping for idle moments: compacts when tombstones pile up (this renumbers
    /// symbols and rebuilds the indexes, O(symbols)). True if it did.
    pub fn maintain(&mut self) -> bool {
        if self.dead_symbols * 100 <= self.db.len() * self.cfg.compact_percent {
            return false;
        }
        self.rebuild_all();
        true
    }

    /// Discards incrementally patched indexes and hierarchy and rebuilds them
    /// from the symbol tables. Used as the reference when testing patching.
    pub fn rebuild_all(&mut self) {
        self.dirty = true;
        self.flush();
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
        let t = std::time::Instant::now();
        self.db.rebuild_indexes();
        let idx_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        self.hier = Hierarchy::build(&mut self.db);
        self.dirty = false;
        crate::log!("lightspeed: flush: indexes {idx_ms:.1} ms, hierarchy {:.1} ms ({} symbols)", t.elapsed().as_secs_f64() * 1e3, self.db.len());
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

use crate::base::ids::Idx;

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
