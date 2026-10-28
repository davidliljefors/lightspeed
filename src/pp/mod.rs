//! Tolerant, fast C/C++ preprocessor.
//!
//! One [`Preprocessor`] per worker thread; it owns every piece of mutable
//! state (macro table, context stacks, scratch pools, resolve caches), so
//! translation units run fully in parallel with nothing shared except the
//! read-only [`FileProvider`].
//!
//! What it produces is the token stream the parser sees: inactive `#if`
//! regions removed, directives consumed, macros expanded. Every output token
//! keeps a `loc` back into the *original* file, so symbols found in expanded
//! text still navigate to the right place (tokens that came out of a macro
//! body point at the invocation; tokens passed through as macro arguments keep
//! their own position).
//!
//! # Include policy
//! * [`PpConfig::follow_includes`] `= false`: `#include`s are recorded (and
//!   resolved) but never opened. Fastest; macros defined in headers are unknown.
//! * `= true`: included files are scanned for directives only (their tokens
//!   are never expanded or emitted), so `#define`s and conditionals inside
//!   headers shape the main file's macro state. Include guards and
//!   `#pragma once` short-circuit repeat visits without even loading the file.
//!
//! # Deliberate simplifications
//! * Unknown identifiers in `#if` are `0`; errors make a condition false rather
//!   than aborting.
//! * An invalid `##` paste leaves both tokens instead of failing.
//! * `_Pragma(...)` / `__pragma(...)` are dropped from the output.
//! * Cache keys for shards must include the preprocessed *macro state*, not
//!   just the file's bytes, once headers are followed (see `cache`).

mod directives;
mod expand;
mod expr;
pub(crate) mod macros;
pub mod vfs;

use std::sync::Arc;

use hashbrown::HashMap;
use rustc_hash::{FxBuildHasher, FxHashSet};

use crate::workspace::compile_commands::{FlagSet, IncludeDir};
use crate::base::hash::FieldHasher;
use crate::lexer::{Keyword, Token, TokenKind, lex_into};
use expand::{Ctx, ExpTok, F_ARENA, F_BOL, F_MACRO, F_SPACE, Texts};
use macros::{Macro, MacroKind, MacroTable, NONE};
pub use vfs::{DiskFs, FileProvider, MemFs, normalize_path, path_key};

/// Which toolchain's predefined macros to assume (compile_commands does not
/// tell us what the compiler predefines).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Profile {
    #[default]
    None,
    Msvc,
    ClangCl,
    Gcc,
    Clang,
}

impl Profile {
    /// The toolchain a compile command's `args[0]` implies (`None` for unknown wrappers).
    pub fn detect(compiler: Option<&str>) -> Self {
        let Some(c) = compiler else { return Profile::None };
        let name = c.rsplit(['/', '\\']).next().unwrap_or(c).to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        if name == "cl" {
            Profile::Msvc
        } else if name.contains("clang-cl") {
            Profile::ClangCl
        } else if name.contains("clang") {
            // clang targets the host ABI: on Windows that is MSVC's.
            if cfg!(windows) { Profile::ClangCl } else { Profile::Clang }
        } else if name.contains("gcc") || name.contains("g++") || name == "c++" || name == "cc" {
            Profile::Gcc
        } else {
            Profile::None
        }
    }

    fn defines(self) -> &'static [&'static str] {
        const MSVC: &[&str] =
            &["_MSC_VER 1938", "_MSC_FULL_VER 193833130", "_WIN32 1", "_WIN64 1", "_M_X64 100", "_M_AMD64 100"];
        const CLANG_CL: &[&str] = &[
            "_MSC_VER 1938", "_WIN32 1", "_WIN64 1", "_M_X64 100", "_M_AMD64 100", "__clang__ 1",
            "__clang_major__ 17", "__clang_minor__ 0", "__x86_64__ 1",
        ];
        const GCC: &[&str] = &["__GNUC__ 13", "__GNUC_MINOR__ 2", "__linux__ 1", "__unix__ 1", "__x86_64__ 1", "__LP64__ 1"];
        const CLANG: &[&str] = &[
            "__clang__ 1", "__clang_major__ 17", "__clang_minor__ 0", "__GNUC__ 4", "__GNUC_MINOR__ 2", "__linux__ 1", "__unix__ 1",
            "__x86_64__ 1", "__LP64__ 1",
        ];
        match self {
            Profile::None => &[],
            Profile::Msvc => MSVC,
            Profile::ClangCl => CLANG_CL,
            Profile::Gcc => GCC,
            Profile::Clang => CLANG,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PpConfig {
    /// Open included files to harvest their macro definitions.
    pub follow_includes: bool,
    pub max_include_depth: u32,
    pub profile: Profile,
    /// `__cplusplus` when no `-std=` is given.
    pub default_cplusplus: u32,
    /// Total tokens macro expansion may produce per run; once exhausted, macros
    /// stop expanding (guards against exponential "billion laughs" macros).
    pub max_expansion_tokens: u64,
    /// Per-worker budget for cached header directive streams (bytes); 0 disables.
    pub header_cache_bytes: usize,
}

impl Default for PpConfig {
    fn default() -> Self {
        Self {
            follow_includes: true,
            max_include_depth: 100,
            profile: Profile::None,
            default_cplusplus: 201703,
            max_expansion_tokens: 1 << 26,
            header_cache_bytes: 64 << 20,
        }
    }
}

pub const TOK_SPACE: u8 = F_SPACE;
pub const TOK_BOL: u8 = F_BOL;
/// Text is in [`PpOutput::pool`] instead of the source.
pub const TOK_POOLED: u8 = F_ARENA;
/// Produced by (or passed through) a macro expansion.
pub const TOK_MACRO: u8 = F_MACRO;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PpToken {
    /// Offset into the source, or into [`PpOutput::pool`] if `TOK_POOLED`.
    pub off: u32,
    pub len: u32,
    /// Position in the original file this token is attributed to.
    pub loc: u32,
    pub kind: TokenKind,
    pub keyword: Keyword,
    pub flags: u8,
}

const _: () = assert!(size_of::<PpToken>() == 16);

impl PpToken {
    pub fn pooled(&self) -> bool {
        self.flags & TOK_POOLED != 0
    }
    pub fn from_macro(&self) -> bool {
        self.flags & TOK_MACRO != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagKind {
    UnterminatedIf,
    ElseWithoutIf,
    EndifWithoutIf,
    ElseAfterElse,
    IncludeNotFound,
    IncludeTooDeep,
    BadInclude,
    BadDefine,
    BadExpression,
    DivisionByZero,
    UnterminatedMacroArgs,
    ExpansionLimit,
    UserError,
    UnknownDirective,
    BadDirective,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PpDiag {
    pub loc: u32,
    pub kind: DiagKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncludeRecord {
    /// Offset of the `#`.
    pub loc: u32,
    pub angled: bool,
    name: (u32, u32),
    resolved: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefineRecord {
    /// Offset of the macro name in the source.
    pub name_loc: u32,
    pub name_len: u32,
    pub function_like: bool,
    pub undef: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PpStats {
    pub expansions: u64,
    pub headers_entered: u32,
    pub skipped_guard: u32,
    pub skipped_once: u32,
    pub unresolved: u32,
    /// Diagnostics raised inside headers (not recorded individually).
    pub header_diags: u32,
    pub header_cache_hits: u32,
}

/// Result of preprocessing one file. Reuse it across runs: `run` clears it but
/// keeps the allocations.
#[derive(Default)]
pub struct PpOutput {
    pub tokens: Vec<PpToken>,
    /// Backing text for pooled tokens and include names.
    pub pool: String,
    pub includes: Vec<IncludeRecord>,
    pub defines: Vec<DefineRecord>,
    /// Byte ranges of the main file removed by conditionals.
    pub skipped: Vec<(u32, u32)>,
    pub diagnostics: Vec<PpDiag>,
    /// Sorted, deduplicated [`path_key`] of every path this run depended on:
    /// the main file plus every include candidate probed, whether or not it
    /// existed. A change to any of them (modify, delete, or create for a miss)
    /// can alter the result.
    pub deps: Vec<u64>,
    pub stats: PpStats,
}

impl PpOutput {
    /// Heap bytes retained by the buffers (kept between runs for reuse).
    pub fn heap_bytes(&self) -> usize {
        self.tokens.capacity() * std::mem::size_of::<PpToken>()
            + self.pool.capacity()
            + self.includes.capacity() * std::mem::size_of::<IncludeRecord>()
            + self.defines.capacity() * std::mem::size_of::<DefineRecord>()
            + self.skipped.capacity() * 8
            + self.diagnostics.capacity() * std::mem::size_of::<PpDiag>()
            + self.deps.capacity() * 8
    }

    /// Frees the buffers if a huge file inflated them, so one outlier doesn't
    /// pin hundreds of MiB per worker thread.
    pub fn trim(&mut self, limit_bytes: usize) {
        if self.heap_bytes() > limit_bytes {
            self.clear();
            self.tokens.shrink_to(1 << 16);
            self.pool.shrink_to(1 << 20);
            self.deps.shrink_to(1 << 12);
        }
    }
    pub fn clear(&mut self) {
        self.deps.clear();
        self.tokens.clear();
        self.pool.clear();
        self.includes.clear();
        self.defines.clear();
        self.skipped.clear();
        self.diagnostics.clear();
        self.stats = PpStats::default();
    }

    #[inline]
    pub fn text<'a>(&'a self, t: &PpToken, src: &'a str) -> &'a str {
        let s = if t.pooled() { self.pool.as_str() } else { src };
        &s[t.off as usize..(t.off + t.len) as usize]
    }

    pub fn include_name(&self, r: &IncludeRecord) -> &str {
        &self.pool[r.name.0 as usize..(r.name.0 + r.name.1) as usize]
    }

    /// Resolved path of the include, if it was found.
    pub fn include_path(&self, r: &IncludeRecord) -> Option<&str> {
        (r.resolved.1 > 0).then(|| &self.pool[r.resolved.0 as usize..(r.resolved.0 + r.resolved.1) as usize])
    }

    pub fn has_diag(&self, kind: DiagKind) -> bool {
        self.diagnostics.iter().any(|d| d.kind == kind)
    }

    /// Token texts joined by single spaces (handy in tests and debugging).
    pub fn render(&self, src: &str) -> String {
        let mut s = String::new();
        for (i, t) in self.tokens.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(self.text(t, src));
        }
        s
    }

    pub(super) fn diag(&mut self, depth: u32, loc: u32, kind: DiagKind) {
        if depth == 0 {
            self.diagnostics.push(PpDiag { loc, kind });
        } else {
            // Locations in headers are meaningless to the main file; count only.
            self.stats.header_diags += 1;
        }
    }
}

/// Cursor over one file's tokens.
pub(super) struct Input<'a> {
    pub src: &'a str,
    pub toks: &'a [Token],
    pub pos: usize,
    pub depth: u32,
    line_pos: usize,
    line: u32,
}

impl<'a> Input<'a> {
    pub fn new(src: &'a str, toks: &'a [Token], depth: u32) -> Self {
        Self { src, toks, pos: 0, depth, line_pos: 0, line: 1 }
    }

    /// 1-based line of `off`; amortized O(1) for monotonically increasing offsets.
    pub fn line_at(&mut self, off: u32) -> u32 {
        let off = (off as usize).min(self.src.len());
        if off < self.line_pos {
            self.line_pos = 0;
            self.line = 1;
        }
        let n = memchr::memchr_iter(b'\n', &self.src.as_bytes()[self.line_pos..off]).count();
        self.line += n as u32;
        self.line_pos = off;
        self.line
    }
}

/// A header reduced to its directive lines, owned so no file stays mapped.
/// Depends only on the file's content, so workers can share it.
pub(super) struct HeaderEntry {
    text: String,
    toks: Vec<Token>,
    guard: Option<String>,
    /// Where it was read from and the hash of the text that was read, so a
    /// disk cache can tell later whether this header changed.
    path: String,
    hash: u64,
}

impl HeaderEntry {
    fn size(&self) -> usize {
        self.text.len() + self.toks.len() * std::mem::size_of::<Token>() + self.path.len() + 64
    }
}

const SHARED_SHARDS: usize = 64;

/// Sharded map whose values are computed once: concurrent callers for a key
/// wait for whoever is already computing it instead of repeating the work.
struct OnceMap<V> {
    shards: Vec<std::sync::RwLock<HashMap<u64, Arc<std::sync::OnceLock<V>>, FxBuildHasher>>>,
}

impl<V: Clone> OnceMap<V> {
    fn new(shards: usize) -> Self {
        Self { shards: (0..shards).map(|_| std::sync::RwLock::new(HashMap::with_hasher(FxBuildHasher))).collect() }
    }

    fn shard(&self, key: u64) -> &std::sync::RwLock<HashMap<u64, Arc<std::sync::OnceLock<V>>, FxBuildHasher>> {
        &self.shards[(key >> 7) as usize % self.shards.len()]
    }

    /// The value for `key`, computed with `init` if nobody has yet. Also
    /// returns whether this call did the computing. `init` must not call back in.
    fn get_or_init(&self, key: u64, init: impl FnOnce() -> V) -> (V, bool) {
        let sh = self.shard(key);
        let existing = sh.read().ok().and_then(|m| m.get(&key).cloned());
        let slot = match existing {
            Some(s) => s,
            None => match sh.write() {
                Ok(mut m) => m.entry(key).or_default().clone(),
                Err(_) => return (init(), true),
            },
        };
        let mut computed = false;
        let v = slot
            .get_or_init(|| {
                computed = true;
                init()
            })
            .clone();
        (v, computed)
    }

    fn remove(&self, key: u64) {
        if let Ok(mut m) = self.shard(key).write() {
            m.remove(&key);
        }
    }

    /// Drops every finished value for which `keep` is false.
    fn retain(&self, keep: impl Fn(&V) -> bool) {
        for sh in &self.shards {
            if let Ok(mut m) = sh.write() {
                m.retain(|_, slot| slot.get().is_none_or(&keep));
            }
        }
    }

    /// Every finished value.
    fn ready(&self) -> Vec<(u64, V)> {
        let mut out = Vec::new();
        for sh in &self.shards {
            if let Ok(m) = sh.read() {
                out.extend(m.iter().filter_map(|(&k, slot)| Some((k, slot.get()?.clone()))));
            }
        }
        out
    }
}

/// Header directive streams shared by every worker, so each header is read and
/// lexed once per build instead of once per thread, plus the force-include
/// prologue of each flag set. The header value is `None` when the file could
/// not be read.
pub struct SharedHeaders {
    headers: OnceMap<Option<Arc<HeaderEntry>>>,
    prologues: OnceMap<Arc<Prologue>>,
}

impl Default for SharedHeaders {
    fn default() -> Self {
        Self { headers: OnceMap::new(SHARED_SHARDS), prologues: OnceMap::new(1) }
    }
}

impl SharedHeaders {
    /// The force-include prologue of flag set `key`, built by whichever
    /// worker asks first (the others wait for it).
    fn get_or_build_prologue(&self, key: u64, build: impl FnOnce() -> Prologue) -> Arc<Prologue> {
        self.prologues.get_or_init(key, || Arc::new(build())).0
    }

    /// `(entries, text bytes, token bytes)` currently cached.
    pub fn stats(&self) -> (usize, usize, usize) {
        let mut s = (0, 0, 0);
        for e in self.headers.ready().iter().filter_map(|(_, e)| e.as_ref()) {
            s.0 += 1;
            s.1 += e.text.capacity();
            s.2 += e.toks.capacity() * std::mem::size_of::<Token>();
        }
        s
    }

    /// `(key, path, content hash)` of every header read so far.
    pub fn files(&self) -> Vec<(u64, String, u64)> {
        self.headers.ready().into_iter().filter_map(|(k, e)| e.map(|e| (k, e.path.clone(), e.hash))).collect()
    }

    /// The entry for `key`, loading it with `load` if no worker has yet.
    /// Returns whether this call did the loading.
    fn get_or_load(&self, key: u64, load: impl FnOnce() -> Option<Arc<HeaderEntry>>) -> (Option<Arc<HeaderEntry>>, bool) {
        let (r, loaded) = self.headers.get_or_init(key, load);
        if r.is_none() && loaded {
            // Do not remember a failed read: the file may appear later.
            self.headers.remove(key);
        }
        (r, loaded)
    }

    /// Forgets one file (it changed on disk).
    pub fn invalidate(&self, key: u64) {
        self.prologues.retain(|p| !p.deps.contains(&key));
        self.headers.remove(key);
    }
}

pub(super) struct FileInfo {
    pub path: String,
    pub key: u64,
    /// Include-dir index this file was found in ([`NONE`] if not via a dir).
    pub dir_idx: u32,
}

#[derive(Clone)]
pub(super) struct Cond {
    pub taken: bool,
    pub seen_else: bool,
    pub loc: u32,
}

#[derive(Clone, Copy)]
pub(super) struct Resolved {
    pub key: u64,
    pub dir_idx: u32,
}

/// Resolution (path offset, len, dir index, key) plus its range in `probes`.
type ResolveEntry = (Option<(u32, u32, u32, u64)>, u32, u32);

/// Preprocessor state at the first real token of a main file, i.e. after its
/// whole include/`#define` prologue. While the prologue bytes stay the same,
/// a re-run resumes here instead of replaying every header directive, which is
/// what makes editing a file body cheap.
struct Snapshot {
    base_key: u64,
    prefix_len: u32,
    prefix_hash: u64,
    t: MacroTable,
    cond: Vec<Cond>,
    once: FxHashSet<u64>,
    counter: u32,
    call_line: u32,
    budget: u64,
    pushed_macros: Vec<(String, Option<u32>)>,
    deps: Vec<u64>,
    includes: Vec<IncludeRecord>,
    defines: Vec<DefineRecord>,
    skipped: Vec<(u32, u32)>,
    diagnostics: Vec<PpDiag>,
    pool: String,
    stats: PpStats,
}

/// What a flag set's force-includes leave behind: the macro table every TU
/// starts from, plus the records and state replayed at the start of each TU.
/// Computed once per flag set and shared by all workers.
struct Prologue {
    base: MacroTable,
    once: FxHashSet<u64>,
    counter: u32,
    deps: Vec<u64>,
    includes: Vec<IncludeRecord>,
    defines: Vec<DefineRecord>,
    skipped: Vec<(u32, u32)>,
    diagnostics: Vec<PpDiag>,
    pool: String,
    stats: PpStats,
}

impl Default for Prologue {
    fn default() -> Self {
        Self {
            base: MacroTable::new(),
            once: FxHashSet::default(),
            counter: 0,
            deps: Vec::new(),
            includes: Vec::new(),
            defines: Vec::new(),
            skipped: Vec::new(),
            diagnostics: Vec::new(),
            pool: String::new(),
            stats: PpStats::default(),
        }
    }
}

const MAX_SNAPSHOTS: usize = 16;
const RESOLVE_CACHE_ENTRIES: usize = 50_000;

/// Per-worker preprocessor. See the module docs.
pub struct Preprocessor<'fs, P: FileProvider> {
    pub(super) fs: &'fs P,
    pub(super) cfg: PpConfig,

    snapshots: HashMap<u64, Snapshot, FxBuildHasher>,
    snapshotting: bool,
    /// Predefined macros of the flag set; `base` is this plus the force-includes.
    flags: FlagSet,
    prologue: Arc<Prologue>,
    prologue_stale: bool,
    base_key: Option<u64>,
    pub(super) dirs: Vec<IncludeDir>,
    force_includes: Vec<String>,

    /// Working macro table for the current TU.
    pub(super) t: MacroTable,
    pub(super) ctx: Vec<Ctx>,
    pub(super) vec_pool: Vec<Vec<ExpTok>>,
    pub(super) range_pool: Vec<Vec<(u32, u32)>>,
    tok_pool: Vec<Vec<Token>>,
    pub(super) pushback: Option<ExpTok>,
    pub(super) in_if: bool,
    pub(super) budget: u64,
    pub(super) expand_depth: u32,

    pub(super) cond: Vec<Cond>,
    pub(super) cond_base: usize,
    pub(super) files: Vec<FileInfo>,
    pub(super) file_depth: usize,
    pub(super) once: FxHashSet<u64>,
    pub(super) counter: u32,
    pub(super) call_line: u32,
    pub(super) pushed_macros: Vec<(String, Option<u32>)>,

    /// File key -> include-guard macro name (span in `guard_text`), if any.
    /// Depends only on file content, so it survives across TUs.
    guards: HashMap<u64, Option<(u32, u32)>, FxBuildHasher>,
    header_cache: HashMap<u64, Arc<HeaderEntry>, FxBuildHasher>,
    header_cache_bytes: usize,
    shared: Option<Arc<SharedHeaders>>,
    guard_text: String,
    resolve_cache: HashMap<u64, ResolveEntry, FxBuildHasher>,
    resolved_paths: String,
    probes: Vec<u64>,
    deps: Vec<u64>,

    pub(super) scratch: String,
    pub(super) scratch2: String,
    pub(super) path_buf: String,
    pub(super) tmp_toks: Vec<Token>,
    pub(super) param_scratch: Vec<(u32, u32)>,
}

fn cplusplus_for(std: &str) -> Option<u32> {
    let s = std.to_ascii_lowercase();
    let v = s.strip_prefix("gnu++").or_else(|| s.strip_prefix("c++"))?;
    Some(match v {
        "98" | "03" => 199711,
        "11" | "0x" => 201103,
        "14" | "1y" => 201402,
        "17" | "1z" => 201703,
        "20" | "2a" => 202002,
        "23" | "2b" | "latest" => 202302,
        "26" | "2c" => 202400,
        _ => return None,
    })
}

impl<'fs, P: FileProvider> Preprocessor<'fs, P> {
    pub fn new(fs: &'fs P, cfg: PpConfig) -> Self {
        Self {
            fs,
            cfg,
            snapshots: HashMap::with_hasher(FxBuildHasher),
            snapshotting: false,
            flags: FlagSet::default(),
            prologue: Arc::new(Prologue::default()),
            prologue_stale: false,
            base_key: None,
            dirs: Vec::new(),
            force_includes: Vec::new(),
            t: MacroTable::new(),
            ctx: Vec::new(),
            vec_pool: Vec::new(),
            range_pool: Vec::new(),
            tok_pool: Vec::new(),
            pushback: None,
            in_if: false,
            budget: 0,
            expand_depth: 0,
            cond: Vec::new(),
            cond_base: 0,
            files: Vec::new(),
            file_depth: 0,
            once: FxHashSet::default(),
            counter: 0,
            call_line: 0,
            pushed_macros: Vec::new(),
            guards: HashMap::with_hasher(FxBuildHasher),
            header_cache: HashMap::with_hasher(FxBuildHasher),
            header_cache_bytes: 0,
            shared: None,
            guard_text: String::new(),
            resolve_cache: HashMap::with_hasher(FxBuildHasher),
            resolved_paths: String::new(),
            probes: Vec::new(),
            deps: Vec::new(),
            scratch: String::new(),
            scratch2: String::new(),
            path_buf: String::new(),
            tmp_toks: Vec::new(),
            param_scratch: Vec::new(),
        }
    }

    /// Shares lexed header directive streams with other workers.
    pub fn set_shared(&mut self, shared: Arc<SharedHeaders>) {
        self.shared = Some(shared);
    }

    /// Selects the flag set for the following [`run`](Self::run)s. Cheap when
    /// unchanged; otherwise rebuilds the predefined-macro snapshot.
    pub fn set_flags(&mut self, flags: &FlagSet) {
        let key = flags.hash();
        if self.base_key == Some(key) {
            return;
        }
        self.base_key = Some(key);
        self.snapshots.clear();
        self.dirs.clone_from(&flags.include_dirs);
        self.force_includes.clone_from(&flags.force_includes);
        // The predefined-macro table is only built if no worker has this
        // flag set's prologue yet, so switching between known flag sets is cheap.
        self.flags.clone_from(flags);
        self.prologue_stale = true;
    }

    /// Leaves the flag set's predefined macros in `self.t`.
    fn build_predefined(&mut self) {
        let flags = std::mem::take(&mut self.flags);
        self.t = MacroTable::new();
        for (name, kind) in [
            ("__LINE__", MacroKind::Line),
            ("__FILE__", MacroKind::File),
            ("__BASE_FILE__", MacroKind::File),
            ("__COUNTER__", MacroKind::Counter),
            ("__INCLUDE_LEVEL__", MacroKind::IncludeLevel),
        ] {
            let sp = self.t.intern(name);
            self.t.define(Macro { name: sp, kind, variadic: false, nparams: 0, body_start: 0, body_len: 0, disabled: false });
        }
        let cpp = flags.std.as_deref().and_then(cplusplus_for).unwrap_or(self.cfg.default_cplusplus);
        let line = format!("__cplusplus {cpp}L");
        self.define_line(&line);
        for l in ["__STDC_HOSTED__ 1", "__DATE__ \"Jan  1 2025\"", "__TIME__ \"00:00:00\""] {
            self.define_line(l);
        }
        let profile = if self.cfg.profile == Profile::None { Profile::detect(flags.compiler.as_deref()) } else { self.cfg.profile };
        for l in profile.defines() {
            self.define_line(l);
        }
        for m in &flags.macros {
            if m.undef {
                self.t.undef(&m.name);
            } else {
                self.define_line(&format!("{} {}", m.name, m.value.as_deref().unwrap_or("1")));
            }
        }
        self.flags = flags;
    }

    /// Forget cached include resolutions (call when files appear/disappear).
    pub fn clear_resolve_cache(&mut self) {
        self.resolve_cache.clear();
        self.resolved_paths.clear();
        self.probes.clear();
    }

    /// Frees the include-resolution memo once it grows past `RESOLVE_CACHE_ENTRIES`
    /// (it is keyed by flag set and includer directory, so across many modules it
    /// otherwise only grows).
    fn bound_resolve_cache(&mut self) {
        if self.resolve_cache.len() > RESOLVE_CACHE_ENTRIES || self.probes.len() > RESOLVE_CACHE_ENTRIES * 8 {
            self.clear_resolve_cache();
            self.resolve_cache.shrink_to(1 << 12);
            self.resolved_paths.shrink_to(1 << 16);
            self.probes.shrink_to(1 << 14);
        }
    }

    /// Approximate retained heap bytes: `(resolve cache, macro tables, header/guard caches)`.
    pub fn heap_breakdown(&self) -> (usize, usize, usize) {
        let resolve = self.resolve_cache.capacity() * (8 + std::mem::size_of::<ResolveEntry>() + 1) + self.resolved_paths.capacity() + self.probes.capacity() * 8;
        let macros = self.t.heap_bytes() + self.prologue.base.heap_bytes() + self.snapshots.values().map(|s| s.t.heap_bytes() + s.pool.capacity()).sum::<usize>();
        let caches = self.header_cache.capacity() * 17 + self.guards.capacity() * 21 + self.guard_text.capacity() + self.tok_pool.iter().map(|v| v.capacity() * std::mem::size_of::<Token>()).sum::<usize>();
        (resolve, macros, caches)
    }

    /// Number of live macros in the last finished TU (diagnostics/tests).
    pub fn macro_count(&self) -> usize {
        self.t.len()
    }

    /// Whether `name` is defined after the last [`run`](Self::run). Like real
    /// compilers, the `__has_*` feature-test operators count as defined.
    pub fn is_defined(&self, name: &str) -> bool {
        self.t.lookup(name).is_some() || name.starts_with("__has_")
    }

    /// Preprocesses `src` (the text of `path`) into `out`.
    pub fn run(&mut self, path: &str, src: &str, out: &mut PpOutput) {
        self.bound_resolve_cache();
        if self.prologue_stale {
            self.build_prologue();
        }
        out.clear();
        // An aborted expansion can leave macros disabled; the table is about
        // to be rolled back rather than recopied, so clear that here.
        for c in &self.ctx {
            if let Some(m) = self.t.defs.get_mut(c.mac as usize) {
                m.disabled = false;
            }
        }
        self.t.reset_from(&self.prologue.base);
        self.reset_tu_state();

        let mut main_path = path.to_owned();
        normalize_path(&mut main_path);
        let key = path_key(&main_path);
        self.set_file(0, &main_path, key, NONE);

        let mut toks = self.tok_pool.pop().unwrap_or_default();
        lex_into(src, &mut toks);
        let mut inp = Input::new(src, &toks, 0);
        let mut first = !self.restore_snapshot(key, src, &mut inp, out);
        if first {
            self.deps.clear();
            self.deps.push(key);
            let p = &*self.prologue;
            self.deps.extend_from_slice(&p.deps);
            self.once.clone_from(&p.once);
            self.counter = p.counter;
            out.includes.extend_from_slice(&p.includes);
            out.defines.extend_from_slice(&p.defines);
            out.skipped.extend_from_slice(&p.skipped);
            out.diagnostics.extend_from_slice(&p.diagnostics);
            out.pool.push_str(&p.pool);
            out.stats = p.stats;
        }

        while let Some(t) = self.expand_next(&mut inp, out, 0, true) {
            if first {
                first = false;
                if self.snapshotting {
                    self.take_snapshot(key, &t, inp.toks, src, out);
                }
            }
            if t.kind == TokenKind::Ident && t.len >= 7 && self.is_pragma_op(&t, src)
                && let Some(n) = self.expand_next(&mut inp, out, 0, true) {
                    if n.kind == TokenKind::LParen {
                        self.drop_call(&mut inp, out);
                        continue;
                    }
                    self.pushback = Some(n);
                }
            self.emit(&t, src, out);
        }
        for c in &self.cond {
            out.diagnostics.push(PpDiag { loc: c.loc, kind: DiagKind::UnterminatedIf });
        }
        self.cond.clear();
        self.tok_pool.push(toks);
        self.deps.sort_unstable();
        self.deps.dedup();
        out.deps.extend_from_slice(&self.deps);
    }

    fn reset_tu_state(&mut self) {
        self.ctx.clear();
        self.pushback = None;
        self.in_if = false;
        self.budget = self.cfg.max_expansion_tokens;
        self.expand_depth = 0;
        self.cond.clear();
        self.cond_base = 0;
        self.once.clear();
        self.counter = 0;
        self.call_line = 0;
        self.pushed_macros.clear();
        self.file_depth = 0;
    }

    /// Selects this flag set's prologue, computing it if no worker has yet.
    fn build_prologue(&mut self) {
        self.prologue_stale = false;
        let key = self.base_key.unwrap_or(0);
        self.prologue = match self.shared.clone() {
            Some(s) => s.get_or_build_prologue(key, || self.compute_prologue()),
            None => Arc::new(self.compute_prologue()),
        };
    }

    /// Runs the flag set's force-included files once, so every TU starts from
    /// the macro table they leave (like a precompiled header) instead of
    /// re-executing them.
    fn compute_prologue(&mut self) -> Prologue {
        self.build_predefined();
        let mut p = Prologue { base: self.t.clone(), ..Prologue::default() };
        if self.force_includes.is_empty() || !self.cfg.follow_includes {
            p.base.seal();
            return p;
        }
        let mut out = PpOutput::default();
        self.reset_tu_state();
        self.set_file(0, "<command-line>", 0, NONE);
        self.deps.clear();
        for k in 0..self.force_includes.len() {
            self.path_buf.clear();
            self.path_buf.push_str(&self.force_includes[k]);
            normalize_path(&mut self.path_buf);
            let key = path_key(&self.path_buf);
            self.deps.push(key);
            if self.fs.exists_keyed(&self.path_buf, key) {
                self.enter_file(key, NONE, &mut out);
            }
        }
        self.file_depth = 0;
        for c in &self.cond {
            out.diagnostics.push(PpDiag { loc: c.loc, kind: DiagKind::UnterminatedIf });
        }
        self.cond.clear();
        self.deps.sort_unstable();
        self.deps.dedup();
        p.once = std::mem::take(&mut self.once);
        p.counter = self.counter;
        p.deps = std::mem::take(&mut self.deps);
        p.includes = out.includes;
        p.defines = out.defines;
        p.skipped = out.skipped;
        p.diagnostics = out.diagnostics;
        p.pool = out.pool;
        p.stats = out.stats;
        p.base.clone_from(&self.t);
        p.base.seal();
        p
    }

    fn take_snapshot(&mut self, key: u64, t: &ExpTok, toks: &[Token], src: &str, out: &PpOutput) {
        if !self.ctx.is_empty() || self.pushback.is_some() || t.flags & (F_MACRO | F_ARENA) != 0 || self.cond_base != 0 || self.file_depth != 0 {
            return;
        }
        // Cut at the end of the last directive's line, so typing between it and
        // the first real token (a new first declaration) still hits the snapshot.
        let first = toks.partition_point(|x| x.start < t.loc);
        let off = match first.checked_sub(1).map(|i| toks[i].end() as usize) {
            None => 0,
            Some(end) => match memchr::memchr(b'\n', &src.as_bytes()[end.min(src.len())..]) {
                Some(n) => end + n + 1,
                None => return,
            },
        };
        if off > t.loc as usize {
            return;
        }
        let Some(prefix) = src.as_bytes().get(..off) else { return };
        if self.snapshots.len() >= MAX_SNAPSHOTS && !self.snapshots.contains_key(&key) {
            let victim = self.snapshots.keys().next().copied();
            if let Some(v) = victim {
                self.snapshots.remove(&v);
            }
        }
        let snap = Snapshot {
            base_key: self.base_key.unwrap_or(0),
            prefix_len: off as u32,
            prefix_hash: crate::base::hash::hash_bytes(prefix),
            t: self.t.clone(),
            cond: self.cond.clone(),
            once: self.once.clone(),
            counter: self.counter,
            call_line: self.call_line,
            budget: self.budget,
            pushed_macros: self.pushed_macros.clone(),
            deps: self.deps.clone(),
            includes: out.includes.clone(),
            defines: out.defines.clone(),
            skipped: out.skipped.clone(),
            diagnostics: out.diagnostics.clone(),
            pool: out.pool.clone(),
            stats: out.stats,
        };
        self.snapshots.insert(key, snap);
    }

    /// Resumes from the snapshot of `key` if the file's prologue is unchanged.
    fn restore_snapshot(&mut self, key: u64, src: &str, inp: &mut Input<'_>, out: &mut PpOutput) -> bool {
        let Some(s) = self.snapshots.get(&key) else { return false };
        let len = s.prefix_len as usize;
        let valid = s.base_key == self.base_key.unwrap_or(0)
            && src.as_bytes().get(..len).is_some_and(|p| crate::base::hash::hash_bytes(p) == s.prefix_hash);
        let pos = inp.toks.partition_point(|t| (t.start as usize) < len);
        // A token straddling the cut means the edit extended the prologue's last token.
        if !valid || pos.checked_sub(1).is_some_and(|i| inp.toks[i].end() as usize > len) {
            self.snapshots.remove(&key);
            return false;
        }
        let s = &self.snapshots[&key];
        self.t.clone_from(&s.t);
        self.cond.clone_from(&s.cond);
        self.once.clone_from(&s.once);
        self.counter = s.counter;
        self.call_line = s.call_line;
        self.budget = s.budget;
        self.pushed_macros.clone_from(&s.pushed_macros);
        self.deps.clone_from(&s.deps);
        out.includes.extend_from_slice(&s.includes);
        out.defines.extend_from_slice(&s.defines);
        out.skipped.extend_from_slice(&s.skipped);
        out.diagnostics.extend_from_slice(&s.diagnostics);
        out.pool.push_str(&s.pool);
        out.stats = s.stats;
        self.file_depth = 0;
        self.cond_base = 0;
        inp.pos = pos;
        true
    }

    /// Keep per-file snapshots so that re-running an edited file skips its
    /// unchanged include prologue. Meant for the long-lived editing worker;
    /// bulk indexing leaves it off since each file runs once.
    pub fn set_snapshotting(&mut self, on: bool) {
        self.snapshotting = on;
        if !on {
            self.snapshots.clear();
        }
    }

    fn is_pragma_op(&self, t: &ExpTok, src: &str) -> bool {
        matches!(Texts { arena: &self.t.arena, src }.get(t), "_Pragma" | "__pragma")
    }

    /// After `_Pragma (`: swallow through the matching `)`.
    fn drop_call(&mut self, inp: &mut Input<'_>, out: &mut PpOutput) {
        let mut depth = 1;
        while depth > 0 {
            match self.expand_next(inp, out, 0, true).map(|t| t.kind) {
                Some(TokenKind::LParen) => depth += 1,
                Some(TokenKind::RParen) => depth -= 1,
                Some(_) => {}
                None => break,
            }
        }
    }

    #[inline]
    fn emit(&mut self, t: &ExpTok, src: &str, out: &mut PpOutput) {
        let (off, pooled) = if t.flags & F_ARENA != 0 {
            let text = &self.t.arena[t.off as usize..(t.off + t.len) as usize];
            let o = out.pool.len() as u32;
            out.pool.push_str(text);
            (o, F_ARENA)
        } else {
            debug_assert!((t.off + t.len) as usize <= src.len());
            (t.off, 0)
        };
        out.tokens.push(PpToken {
            off,
            len: t.len,
            loc: t.loc,
            kind: t.kind,
            keyword: t.keyword,
            flags: (t.flags & (F_SPACE | F_BOL | F_MACRO)) | pooled,
        });
    }

    pub(super) fn set_file(&mut self, depth: usize, path: &str, key: u64, dir_idx: u32) {
        if self.files.len() <= depth {
            self.files.resize_with(depth + 1, || FileInfo { path: String::new(), key: 0, dir_idx: NONE });
        }
        let f = &mut self.files[depth];
        self.call_line = 0;
        f.path.clear();
        f.path.push_str(path);
        f.key = key;
        f.dir_idx = dir_idx;
    }

    /// Reads and lexes the header whose normalized path is in `path_buf`,
    /// keeping only its directive lines.
    fn load_header(&mut self) -> Option<Arc<HeaderEntry>> {
        let sf = self.fs.load(&self.path_buf)?;
        let src = sf.text();
        let mut toks = self.tok_pool.pop().unwrap_or_default();
        lex_into(src, &mut toks);
        let guard = directives::detect_guard(src, &toks).map(str::to_owned);
        let mut e = HeaderEntry { text: String::new(), toks: Vec::new(), guard, path: self.path_buf.clone(), hash: crate::base::hash::hash_bytes(src.as_bytes()) };
        directives::compact_directives(src, &toks, &mut e.text, &mut e.toks);
        self.tok_pool.push(toks);
        Some(Arc::new(e))
    }

    /// Loads the file whose normalized path is in `path_buf` and scans it for
    /// directives (tokens are not emitted).
    pub(super) fn enter_file(&mut self, key: u64, dir_idx: u32, out: &mut PpOutput) {
        if self.once.contains(&key) {
            out.stats.skipped_once += 1;
            return;
        }
        if let Some(&Some((o, l))) = self.guards.get(&key)
            && self.t.lookup(&self.guard_text[o as usize..(o + l) as usize]).is_some() {
                out.stats.skipped_guard += 1;
                return;
            }
        let depth = self.file_depth + 1;
        if depth as u32 > self.cfg.max_include_depth {
            return;
        }
        let entry = match self.header_cache.remove(&key) {
            Some(e) => {
                self.header_cache_bytes -= e.size();
                out.stats.header_cache_hits += 1;
                e
            }
            None => {
                let shared = self.shared.clone();
                let loaded = match shared {
                    Some(s) => {
                        let (e, did_load) = s.get_or_load(key, || self.load_header());
                        if !did_load && e.is_some() {
                            out.stats.header_cache_hits += 1;
                        }
                        e
                    }
                    None => self.load_header(),
                };
                let Some(e) = loaded else { return };
                if !self.guards.contains_key(&key) {
                    let g = e.guard.as_deref().map(|name| {
                        let o = self.guard_text.len() as u32;
                        self.guard_text.push_str(name);
                        (o, name.len() as u32)
                    });
                    self.guards.insert(key, g);
                }
                e
            }
        };
        out.stats.headers_entered += 1;
        let path = std::mem::take(&mut self.path_buf);
        self.set_file(depth, &path, key, dir_idx);
        self.path_buf = path;
        self.file_depth = depth;
        let saved_base = std::mem::replace(&mut self.cond_base, self.cond.len());
        let mut inp = Input::new(&entry.text, &entry.toks, depth as u32);
        self.scan_directives(&mut inp, out);
        if self.cond.len() > self.cond_base {
            out.diag(depth as u32, 0, DiagKind::UnterminatedIf);
        }
        self.cond.truncate(self.cond_base);
        self.cond_base = saved_base;
        self.file_depth = depth - 1;
        if self.header_cache_bytes + entry.size() <= self.cfg.header_cache_bytes {
            self.header_cache_bytes += entry.size();
            self.header_cache.insert(key, entry);
        }
    }

    /// [`Self::invalidate_file`] for a path, also dropping state that only
    /// depended on a file of that name being absent.
    pub fn invalidate_path(&mut self, path: &str) {
        let name = vfs::name_key(path);
        self.snapshots.retain(|_, s| !s.deps.contains(&name));
        self.invalidate_file(path_key(path));
    }

    /// Forgets everything cached about the file with `key` (after it changed
    /// on disk). Resolutions are dropped wholesale because a created or
    /// deleted file can change any include search.
    pub fn invalidate_file(&mut self, key: u64) {
        if let Some(e) = self.header_cache.remove(&key) {
            self.header_cache_bytes -= e.size();
        }
        if let Some(s) = &self.shared {
            s.invalidate(key);
        }
        self.guards.remove(&key);
        if self.prologue.deps.contains(&key) {
            self.prologue_stale = true;
        }
        self.snapshots.retain(|&k, s| k == key || !s.deps.contains(&key));
        self.clear_resolve_cache();
    }

    /// Drops all cached header and resolution state.
    pub fn clear_caches(&mut self) {
        self.prologue_stale = true;
        self.snapshots.clear();
        self.header_cache.clear();
        self.header_cache_bytes = 0;
        self.guards.clear();
        self.guard_text.clear();
        self.clear_resolve_cache();
    }

    /// Include search. On success the normalized path is left in `path_buf`.
    /// Every candidate probed is recorded as a dependency.
    fn probe(&mut self) -> bool {
        let key = path_key(&self.path_buf);
        let hit = self.fs.exists_keyed(&self.path_buf, key);
        // A hit depends on that exact file; a miss only on a file of that name appearing.
        self.probes.push(if hit { key } else { vfs::name_key(&self.path_buf) });
        hit
    }
    pub(super) fn resolve(&mut self, name: &str, quote: bool, next: bool) -> Option<Resolved> {
        let d = self.file_depth;
        let start = if next {
            match self.files[d].dir_idx {
                NONE => 0,
                i => i as usize + 1,
            }
        } else {
            0
        };
        let includer = self.files[d].path.as_str();
        let includer_dir = if quote && !next { includer.rfind('/').map_or("", |s| &includer[..s]) } else { "" };
        let mut h = FieldHasher::new();
        h.u64(self.base_key.unwrap_or(0)).str(includer_dir).str(name).u64(quote as u64 | (start as u64) << 1);
        let ck = h.finish();

        if let Some(&(found, po, pl)) = self.resolve_cache.get(&ck) {
            self.deps.extend_from_slice(&self.probes[po as usize..(po + pl) as usize]);
            return match found {
                Some((o, l, dir, key)) => {
                    self.path_buf.clear();
                    self.path_buf.push_str(&self.resolved_paths[o as usize..(o + l) as usize]);
                    Some(Resolved { key, dir_idx: dir })
                }
                None => None,
            };
        }

        let probe_start = self.probes.len();
        let mut found: Option<u32> = None;
        let b = name.as_bytes();
        let absolute = b.first().is_some_and(|&c| c == b'/' || c == b'\\') || b.get(1) == Some(&b':');
        if absolute {
            self.path_buf.clear();
            self.path_buf.push_str(name);
            normalize_path(&mut self.path_buf);
            if self.probe() {
                found = Some(NONE);
            }
        } else {
            if quote && !next {
                self.path_buf.clear();
                if !includer_dir.is_empty() {
                    self.path_buf.push_str(includer_dir);
                    self.path_buf.push('/');
                }
                self.path_buf.push_str(name);
                normalize_path(&mut self.path_buf);
                if self.probe() {
                    found = Some(NONE);
                }
            }
            if found.is_none() {
                for i in start..self.dirs.len() {
                    self.path_buf.clear();
                    self.path_buf.push_str(&self.dirs[i].path);
                    self.path_buf.push('/');
                    self.path_buf.push_str(name);
                    normalize_path(&mut self.path_buf);
                    if self.probe() {
                        found = Some(i as u32);
                        break;
                    }
                }
            }
        }
        let (po, pl) = (probe_start as u32, (self.probes.len() - probe_start) as u32);
        self.deps.extend_from_slice(&self.probes[probe_start..]);
        match found {
            Some(dir_idx) => {
                let key = path_key(&self.path_buf);
                let o = self.resolved_paths.len() as u32;
                self.resolved_paths.push_str(&self.path_buf);
                self.resolve_cache.insert(ck, (Some((o, self.path_buf.len() as u32, dir_idx, key)), po, pl));
                Some(Resolved { key, dir_idx })
            }
            None => {
                self.resolve_cache.insert(ck, (None, po, pl));
                None
            }
        }
    }
}
