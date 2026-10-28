//! Persistent index cache: relaunch without re-preprocessing or re-parsing
//! anything that did not change.
//!
//! One file holds a *record* per analysed file (its symbol shard, identifier
//! hashes, direct includes, using/alias declarations) plus the world those
//! records were computed in. A record is reused only if all of this holds:
//!
//! * the running executable is byte-identical (the parser, preprocessor and
//!   this format are all "code"; any rebuild invalidates everything),
//! * the record was made for the same path and the same *effective* flag set
//!   (include dirs including discovered system dirs, macros, std, ...),
//! * none of the record's dependency keys is *dirty*.
//!
//! The dependency keys are exactly [`PpOutput::deps`](crate::pp::PpOutput::deps):
//! the path key of the file itself and of every `#include` candidate the
//! preprocessor probed, whether or not it existed. Dirty keys come from two
//! checks that run once per launch, in parallel, before any file is analysed:
//!
//! 1. **Directory listings.** Every directory the preprocessor listed is listed
//!    again and compared with the stored listing. Any file that appeared,
//!    disappeared or was renamed there becomes a dirty key. This catches new
//!    files that *shadow* an earlier search result and deleted files, and it
//!    needs no per-probe bookkeeping.
//! 2. **Contents.** Every file the preprocessor read (and every analysed file)
//!    is read again and hashed. A different hash, or a file that can no longer
//!    be read, makes its key dirty. Modification times are never trusted.
//!
//! A record with any dirty dependency is simply recomputed, so editing one
//! header re-analyses exactly the files that (transitively, through the
//! preprocessor's actual behaviour) depend on it.
//!
//! Anything doubtful degrades to "recompute": truncated or corrupt files,
//! a different executable, an unreadable directory. Records carry their own
//! checksum and the tables are checksummed, so a damaged cache is never
//! trusted. A file whose content was seen with two different hashes while the
//! cache was being built is stored with hash 0, which never validates.
//!
//! Layout (little-endian): `"LSIX" version u32 fingerprint u64 | records |
//! manifest | files | dirs | trailer`. The manifest is sorted by record key so
//! lookups are a binary search over the mapped file; nothing is parsed or
//! allocated for records that are not used.

#![allow(clippy::chunks_exact_to_as_chunks)]

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use hashbrown::HashSet;
use memmap2::Mmap;
use rayon::prelude::*;
use rustc_hash::FxBuildHasher;

pub mod codec;

use crate::base::hash::{FieldHasher, hash_bytes};
use crate::base::source::SourceFile;
use crate::pp::vfs::{list_dir_entries, seed_dir};
use crate::scope_decls::ScopeDecls;
use crate::store::codec::{DecodeError, ShardKey, decode_shard, encode_shard};
use crate::symbols::SymbolDb;

const FILE_MAGIC: &[u8; 4] = b"LSIX";
const RECORD_MAGIC: &[u8; 4] = b"LSRC";
/// Bump when the layout of this file changes (code changes are covered by [`fingerprint`]).
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const TRAILER_LEN: usize = 64;
const MANIFEST_ENTRY: usize = 20;
/// Stored in place of a hash that cannot be trusted; never validates.
pub const UNTRUSTED: u64 = 0;

/// Identifies the code that produced a cache: a hash of the running executable.
/// If the executable cannot be read nothing can be proven, so the result
/// is unique to this process and no cache ever matches it.
pub fn fingerprint() -> u64 {
    static FP: OnceLock<u64> = OnceLock::new();
    *FP.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| File::open(p).ok())
            // SAFETY: read-only map of our own image; the OS keeps a running executable from being modified.
            .and_then(|f| unsafe { Mmap::map(&f) }.ok())
            .map(|m| hash_bytes(&m));
        let h = exe.unwrap_or_else(|| {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
            t ^ (u64::from(std::process::id()) << 40)
        });
        FieldHasher::new().u64(u64::from(FORMAT_VERSION)).u64(h).finish()
    })
}

/// Where the cache lives and how it is written.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub dir: PathBuf,
    /// Write the new snapshot on a background thread (the index is usable meanwhile).
    pub background_save: bool,
}

impl CacheConfig {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), background_save: false }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("index.lsc")
    }

    /// The per-user cache location for the workspace described by `cdb`.
    /// `LIGHTSPEED_NO_CACHE` disables it; `LIGHTSPEED_CACHE_DIR` relocates it.
    pub fn for_project(cdb: &Path) -> Option<Self> {
        if std::env::var_os("LIGHTSPEED_NO_CACHE").is_some() {
            return None;
        }
        let base = std::env::var_os("LIGHTSPEED_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("lightspeed")))
            .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("lightspeed")))
            .or_else(|| std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".cache").join("lightspeed")))
            .unwrap_or_else(|| std::env::temp_dir().join("lightspeed"));
        let canon = std::fs::canonicalize(cdb).unwrap_or_else(|_| cdb.to_path_buf());
        let id = hash_bytes(canon.to_string_lossy().to_lowercase().as_bytes());
        Some(Self { dir: base.join(format!("{id:016x}")), background_save: true })
    }
}

/// Lookup key of a record: the file and the effective flags it was analysed with.
pub fn record_key(path_key: u64, flags_fp: u64) -> u64 {
    FieldHasher::new().u64(path_key).u64(flags_fp).finish()
}

// ---------------------------------------------------------------- records

/// Everything a record stores about one analysed file.
pub struct RecordParts<'a> {
    pub path: &'a str,
    pub flags_fp: u64,
    pub content_hash: u64,
    /// [`PpOutput::deps`](crate::pp::PpOutput::deps) of the run that produced the rest.
    pub deps: &'a [u64],
    pub idents: &'a [u32],
    pub includes: &'a [String],
    pub decls: &'a ScopeDecls,
    pub shard: &'a SymbolDb,
}

/// A record read back.
pub struct Decoded {
    pub content_hash: u64,
    pub shard: SymbolDb,
    pub idents: Vec<u32>,
    pub includes: Vec<String>,
    pub decls: ScopeDecls,
}

fn put_u32(o: &mut Vec<u8>, v: usize) {
    o.extend_from_slice(&u32::try_from(v).expect("record section exceeds 4 GiB").to_le_bytes());
}

fn put_str(o: &mut Vec<u8>, s: &str) {
    put_u32(o, s.len());
    o.extend_from_slice(s.as_bytes());
}

/// Serializes a record into `out` (cleared first). Everything after the
/// dependency keys is zstd-compressed; the keys stay readable so a snapshot
/// can be validated without decompressing anything.
pub fn encode_record(p: &RecordParts, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(RECORD_MAGIC);
    out.extend_from_slice(&p.content_hash.to_le_bytes());
    out.extend_from_slice(&p.flags_fp.to_le_bytes());
    put_str(out, p.path);
    put_u32(out, p.deps.len());
    for d in p.deps {
        out.extend_from_slice(&d.to_le_bytes());
    }
    let mut pay: Vec<u8> = Vec::new();
    put_u32(&mut pay, p.idents.len());
    for i in p.idents {
        pay.extend_from_slice(&i.to_le_bytes());
    }
    put_u32(&mut pay, p.includes.len());
    for s in p.includes {
        put_str(&mut pay, s);
    }
    put_u32(&mut pay, p.decls.names.len());
    for s in &p.decls.names {
        put_str(&mut pay, s);
    }
    put_u32(&mut pay, p.decls.aliases.len());
    for (a, b) in &p.decls.aliases {
        put_str(&mut pay, a);
        put_str(&mut pay, b);
    }
    let mut shard = Vec::new();
    encode_shard(&ShardKey { path: p.path, content_hash: p.content_hash, flags_hash: p.flags_fp }, p.shard, &mut shard);
    put_u32(&mut pay, shard.len());
    pay.extend_from_slice(&shard);
    put_u32(out, pay.len());
    let at = out.len();
    out.resize(at + zstd::zstd_safe::compress_bound(pay.len()), 0);
    let n = COMPRESSOR
        .with(|c| c.borrow_mut().compress_to_buffer(&pay, &mut out[at..]))
        .expect("zstd compression into a bound-sized buffer cannot fail");
    out.truncate(at + n);
    let sum = hash_bytes(out);
    out.extend_from_slice(&sum.to_le_bytes());
}

thread_local! {
    static DECOMPRESSOR: std::cell::RefCell<zstd::bulk::Decompressor<'static>> =
        std::cell::RefCell::new(zstd::bulk::Decompressor::new().expect("zstd decompressor"));
    static COMPRESSOR: std::cell::RefCell<zstd::bulk::Compressor<'static>> =
        std::cell::RefCell::new(zstd::bulk::Compressor::new(ZSTD_LEVEL).expect("zstd level"));
}

/// Level 1 compresses at hundreds of MB/s per thread and still shrinks records about 3x.
const ZSTD_LEVEL: i32 = 1;
/// No record legitimately expands past this; guards the decompression buffer.
const MAX_RAW_RECORD: usize = 1 << 30;

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

type R<T> = Result<T, DecodeError>;

fn corrupt<T>(why: &'static str) -> R<T> {
    Err(DecodeError::Corrupt(why))
}

impl<'a> Rd<'a> {
    fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        match self.pos.checked_add(n).filter(|&e| e <= self.b.len()) {
            Some(e) => {
                let s = &self.b[self.pos..e];
                self.pos = e;
                Ok(s)
            }
            None => corrupt("truncated"),
        }
    }
    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> R<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.bytes(n)?).or(corrupt("invalid utf-8"))
    }
    /// A count that the remaining bytes can satisfy (keeps hostile counts from allocating).
    fn count(&mut self, min_each: usize) -> R<usize> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_each) > self.b.len() - self.pos { corrupt("count exceeds data") } else { Ok(n) }
    }
}

/// Reads a record. `Stale` if it is for a different path or flag set.
pub fn decode_record(bytes: &[u8], path: &str, flags_fp: u64) -> R<Decoded> {
    if bytes.len() < 8 + RECORD_MAGIC.len() || &bytes[..4] != RECORD_MAGIC {
        return corrupt("bad record header");
    }
    let (body, trailer) = bytes.split_at(bytes.len() - 8);
    if hash_bytes(body) != u64::from_le_bytes(trailer.try_into().unwrap()) {
        return corrupt("record checksum mismatch");
    }
    let mut r = Rd { b: body, pos: 4 };
    let content_hash = r.u64()?;
    let fp = r.u64()?;
    if fp != flags_fp || r.str()? != path {
        return Err(DecodeError::Stale);
    }
    let n = r.count(8)?;
    r.bytes(n * 8)?;
    let raw = r.u32()? as usize;
    if raw > MAX_RAW_RECORD {
        return corrupt("record too large");
    }
    let pay = DECOMPRESSOR
        .with(|d| d.borrow_mut().decompress(&body[r.pos..], raw))
        .or(corrupt("bad compressed payload"))?;
    if pay.len() != raw {
        return corrupt("payload length");
    }
    let mut r = Rd { b: &pay, pos: 0 };
    let n = r.count(4)?;
    let idents = r.bytes(n * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let n = r.count(4)?;
    let mut includes = Vec::with_capacity(n);
    for _ in 0..n {
        includes.push(r.str()?.to_owned());
    }
    let n = r.count(4)?;
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        names.push(r.str()?.to_owned());
    }
    let n = r.count(8)?;
    let mut aliases = Vec::with_capacity(n);
    for _ in 0..n {
        aliases.push((r.str()?.to_owned(), r.str()?.to_owned()));
    }
    let n = r.u32()? as usize;
    let shard = decode_shard(r.bytes(n)?, &ShardKey { path, content_hash, flags_hash: flags_fp })?;
    if r.pos != pay.len() {
        return corrupt("trailing bytes");
    }
    Ok(Decoded { content_hash, shard, idents, includes, decls: ScopeDecls { names, aliases } })
}

/// The dependency keys of a record as raw little-endian bytes, without
/// decoding or checksumming it (callers decode, and so verify, afterwards).
fn record_deps(bytes: &[u8]) -> Option<&[u8]> {
    let mut r = Rd { b: bytes, pos: 4 + 8 + 8 };
    let _ = r.str().ok()?;
    let n = r.u32().ok()? as usize;
    r.bytes(n.checked_mul(8)?).ok()
}

// --------------------------------------------------------------- snapshot

/// Why a snapshot file could not be used at all.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    Missing,
    /// Written by different code (or an older layout).
    Stale,
    Corrupt(&'static str),
}

/// What opening and validating a snapshot found.
#[derive(Clone, Debug, Default)]
pub struct OpenStats {
    pub records: usize,
    pub files_checked: usize,
    pub dirs_checked: usize,
    pub dirty_keys: usize,
    pub millis: f64,
}

/// Outcome of asking for one file's record.
pub enum Probe {
    Hit { data: Box<Decoded>, off: u64, len: u32 },
    /// No record for this path and flags.
    Absent,
    /// A record exists but something it depends on changed.
    Dirty,
    Bad,
}

struct FileEnt<'a> {
    key: u64,
    hash: u64,
    path: &'a str,
}

struct DirEnt<'a> {
    path: &'a str,
    keys: &'a [u8],
}

/// A validated, memory-mapped snapshot.
pub struct Snapshot {
    map: Arc<Mmap>,
    manifest: (usize, usize),
    dirty: HashSet<u64, FxBuildHasher>,
    /// `(key, path, hash now)` of every stored file that could still be read.
    carry: Vec<(u64, String, u64)>,
    pub stats: OpenStats,
}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

impl Snapshot {
    /// Maps `path`, checks it was written by `fingerprint`'s code, and finds
    /// out what changed on disk since (lists directories, hashes files).
    pub fn open(path: &Path, fingerprint: u64) -> Result<Self, OpenError> {
        let t = std::time::Instant::now();
        let file = File::open(path).map_err(|_| OpenError::Missing)?;
        // SAFETY: the cache file is only ever replaced by renaming a new file over it, never modified in place.
        let map = unsafe { Mmap::map(&file) }.map_err(|_| OpenError::Corrupt("cannot map"))?;
        let b = &map[..];
        if b.len() < HEADER_LEN + TRAILER_LEN || &b[..4] != FILE_MAGIC {
            return Err(OpenError::Corrupt("bad header"));
        }
        if u32::from_le_bytes(b[4..8].try_into().unwrap()) != FORMAT_VERSION || le64(&b[8..]) != fingerprint {
            return Err(OpenError::Stale);
        }
        let tr = &b[b.len() - TRAILER_LEN..];
        if hash_bytes(&tr[..56]) != le64(&tr[56..]) {
            return Err(OpenError::Corrupt("trailer checksum"));
        }
        let f = |i: usize| le64(&tr[i * 8..]) as usize;
        let (mo, mn, fo, fnn, dof, dn) = (f(0), f(1), f(2), f(3), f(4), f(5));
        let tstart = b.len() - TRAILER_LEN;
        let ordered = HEADER_LEN <= mo && mo <= fo && fo <= dof && dof <= tstart;
        if !ordered || mn.checked_mul(MANIFEST_ENTRY).is_none_or(|n| mo + n != fo) {
            return Err(OpenError::Corrupt("bad section offsets"));
        }
        if hash_bytes(&b[mo..tstart]) != le64(&tr[48..]) {
            return Err(OpenError::Corrupt("section checksum"));
        }
        for e in b[mo..fo].chunks_exact(MANIFEST_ENTRY) {
            let (off, len) = (le64(&e[8..]) as usize, u32::from_le_bytes(e[16..20].try_into().unwrap()) as usize);
            if off < HEADER_LEN || off.checked_add(len).is_none_or(|end| end > mo) {
                return Err(OpenError::Corrupt("record out of range"));
            }
        }
        let files = parse_files(&b[fo..dof], fnn)?;
        let dirs = parse_dirs(&b[dof..tstart], dn)?;

        // Directory listings: any name that appeared or vanished is a dirty key.
        let dir_dirty: Vec<Vec<u64>> = dirs
            .par_iter()
            .map(|d| {
                let ents = list_dir_entries(d.path);
                let now: Vec<u64> = ents.iter().map(|e| e.0).collect();
                seed_dir(d.path, &now);
                let mut changed = sym_diff(d.keys, &now);
                // A file that appeared also dirties every record that missed an include of that name.
                let names: Vec<u64> = changed.iter().filter_map(|k| ents.binary_search_by_key(k, |e| e.0).ok().map(|i| ents[i].1)).collect();
                changed.extend(names);
                changed
            })
            .collect();
        // Contents: hash everything that was read.
        let checked: Vec<(u64, &str, Option<u64>, u64)> = files
            .par_iter()
            .map(|e| {
                let now = SourceFile::load(e.path).ok().map(|s| hash_bytes(s.text().as_bytes()));
                (e.key, e.path, now, e.hash)
            })
            .collect();
        let mut dirty: HashSet<u64, FxBuildHasher> = HashSet::default();
        dirty.extend(dir_dirty.into_iter().flatten());
        let mut carry = Vec::with_capacity(checked.len());
        for (key, path, now, stored) in checked {
            match now {
                Some(h) if h == stored && stored != UNTRUSTED => carry.push((key, path.to_owned(), h)),
                Some(h) => {
                    dirty.insert(key);
                    carry.push((key, path.to_owned(), h));
                }
                None => {
                    dirty.insert(key);
                }
            }
        }
        let stats = OpenStats {
            records: mn,
            files_checked: files.len(),
            dirs_checked: dirs.len(),
            dirty_keys: dirty.len(),
            millis: t.elapsed().as_secs_f64() * 1e3,
        };
        Ok(Self { map: Arc::new(map), manifest: (mo, mn), dirty, carry, stats })
    }

    pub fn record_count(&self) -> usize {
        self.manifest.1
    }

    /// Whether some stored dependency changed (test hook).
    pub fn is_dirty(&self, key: u64) -> bool {
        self.dirty.contains(&key)
    }

    fn find(&self, rec_key: u64) -> Option<(u64, u32)> {
        let (mo, n) = self.manifest;
        let m = &self.map[mo..mo + n * MANIFEST_ENTRY];
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = &m[mid * MANIFEST_ENTRY..];
            match le64(e).cmp(&rec_key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some((le64(&e[8..]), u32::from_le_bytes(e[16..20].try_into().unwrap()))),
            }
        }
        None
    }

    /// Looks up and, if nothing it depends on changed, decodes the record of `path` analysed with `flags_fp`.
    pub fn probe(&self, path: &str, path_key: u64, flags_fp: u64) -> Probe {
        let Some((off, len)) = self.find(record_key(path_key, flags_fp)) else { return Probe::Absent };
        let bytes = &self.map[off as usize..off as usize + len as usize];
        if !self.dirty.is_empty() {
            let Some(deps) = record_deps(bytes) else { return Probe::Bad };
            if deps.chunks_exact(8).any(|c| self.dirty.contains(&le64(c))) {
                return Probe::Dirty;
            }
        }
        match decode_record(bytes, path, flags_fp) {
            Ok(d) => Probe::Hit { data: Box::new(d), off, len },
            Err(DecodeError::Stale) => Probe::Absent,
            Err(DecodeError::Corrupt(_)) => Probe::Bad,
        }
    }

    /// Files whose content was verified (or re-read) at open, for carrying into the next snapshot.
    pub fn carried_files(&self) -> &[(u64, String, u64)] {
        &self.carry
    }
}

/// Keys in exactly one of two sorted lists (`old` as raw little-endian bytes).
fn sym_diff(old: &[u8], now: &[u64]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut a = old.chunks_exact(8).map(le64).peekable();
    let mut b = now.iter().copied().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (Some(x), Some(y)) if x == y => {
                a.next();
                b.next();
            }
            (Some(x), Some(y)) if x < y => {
                out.push(x);
                a.next();
            }
            (Some(_), Some(y)) => {
                out.push(y);
                b.next();
            }
            (Some(x), None) => {
                out.push(x);
                a.next();
            }
            (None, Some(y)) => {
                out.push(y);
                b.next();
            }
            (None, None) => return out,
        }
    }
}

fn parse_files(b: &[u8], n: usize) -> Result<Vec<FileEnt<'_>>, OpenError> {
    let bad = |_| OpenError::Corrupt("bad file table");
    let mut r = Rd { b, pos: 0 };
    let mut out = Vec::with_capacity(n.min(b.len() / 20));
    for _ in 0..n {
        let key = r.u64().map_err(bad)?;
        let hash = r.u64().map_err(bad)?;
        let path = r.str().map_err(bad)?;
        out.push(FileEnt { key, hash, path });
    }
    if r.pos != b.len() {
        return Err(bad(DecodeError::Stale));
    }
    Ok(out)
}

fn parse_dirs(b: &[u8], n: usize) -> Result<Vec<DirEnt<'_>>, OpenError> {
    let bad = |_| OpenError::Corrupt("bad dir table");
    let mut r = Rd { b, pos: 0 };
    let mut out = Vec::with_capacity(n.min(b.len() / 8));
    for _ in 0..n {
        let path = r.str().map_err(bad)?;
        let k = r.count(8).map_err(bad)?;
        let keys = r.bytes(k * 8).map_err(bad)?;
        out.push(DirEnt { path, keys });
    }
    if r.pos != b.len() {
        return Err(bad(DecodeError::Stale));
    }
    Ok(out)
}

// ------------------------------------------------------------------- save

/// Where a record's bytes come from when a snapshot is written.
pub enum RecSrc {
    Fresh(Vec<u8>),
    /// Already appended to the build's [`Spill`] at this offset.
    Written { off: u64, len: u32 },
    /// Copied from the snapshot the index was started from.
    Old { off: u64, len: u32 },
}

static TMP_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn tmp_path(path: &Path) -> PathBuf {
    let n = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("{}.{n}.tmp", std::process::id()))
}

fn open_new(path: &Path, fingerprint: u64) -> io::Result<(Counting<BufWriter<File>>, PathBuf)> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = tmp_path(path);
    let mut w = Counting { inner: BufWriter::with_capacity(1 << 20, File::create(&tmp)?), pos: 0 };
    w.write_all(FILE_MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&fingerprint.to_le_bytes())?;
    Ok((w, tmp))
}

/// The next snapshot, being written while the index builds so fresh records
/// never pile up in memory. [`save`] finishes it.
pub struct Spill {
    w: Option<Counting<BufWriter<File>>>,
    tmp: PathBuf,
}

impl Spill {
    pub fn create(path: &Path, fingerprint: u64) -> io::Result<Self> {
        let (w, tmp) = open_new(path, fingerprint)?;
        Ok(Self { w: Some(w), tmp })
    }

    /// Appends one record; returns where it landed.
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<(u64, u32)> {
        let w = self.w.as_mut().ok_or_else(|| io::Error::other("spill already finished"))?;
        let off = w.pos;
        w.write_all(bytes)?;
        Ok((off, bytes.len() as u32))
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        // Still owning the writer means the snapshot was never finished.
        if self.w.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Everything needed to write the next snapshot, owned so it can run on another thread.
pub struct SaveJob {
    pub path: PathBuf,
    pub fingerprint: u64,
    pub old: Option<Arc<Mmap>>,
    pub spill: Option<Spill>,
    pub recs: Vec<(u64, RecSrc)>,
    /// `(key, path, content hash)`; hash [`UNTRUSTED`] for files seen with two different contents.
    pub files: Vec<(u64, String, u64)>,
    pub dirs: Vec<(String, Vec<u64>)>,
}

impl Snapshot {
    /// Hands over the mapping so the file can be replaced while records are still copied from it.
    pub fn into_map(self) -> Arc<Mmap> {
        self.map
    }
}

/// Writes the snapshot atomically (temp file + rename). Returns its size.
pub fn save(job: SaveJob) -> io::Result<u64> {
    let SaveJob { path, fingerprint, old, spill, mut recs, mut files, mut dirs } = job;
    let (w, tmp) = match spill {
        Some(mut s) => match s.w.take() {
            Some(w) => (w, s.tmp.clone()),
            None => return Err(io::Error::other("spill already finished")),
        },
        None => open_new(&path, fingerprint)?,
    };
    let result = (|| {
        let mut w = w;
        let mut manifest: Vec<(u64, u64, u32)> = Vec::with_capacity(recs.len());
        for (key, src) in recs.drain(..) {
            let off = w.pos;
            match src {
                RecSrc::Fresh(b) => {
                    w.write_all(&b)?;
                    manifest.push((key, off, b.len() as u32));
                }
                RecSrc::Written { off, len } => manifest.push((key, off, len)),
                RecSrc::Old { off: o, len } => {
                    let old = old.as_ref().ok_or_else(|| io::Error::other("record from a missing snapshot"))?;
                    let s = old.get(o as usize..o as usize + len as usize).ok_or_else(|| io::Error::other("record out of range"))?;
                    w.write_all(s)?;
                    manifest.push((key, off, len));
                }
            }
        }
        manifest.sort_unstable_by_key(|m| m.0);
        manifest.dedup_by_key(|m| m.0);
        files.sort_unstable_by_key(|f| f.0);
        files.dedup_by_key(|f| f.0);
        dirs.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut sections = Vec::new();
        let mo = w.pos;
        for (k, o, l) in &manifest {
            sections.extend_from_slice(&k.to_le_bytes());
            sections.extend_from_slice(&o.to_le_bytes());
            sections.extend_from_slice(&l.to_le_bytes());
        }
        let fo = mo + sections.len() as u64;
        for (k, p, h) in &files {
            sections.extend_from_slice(&k.to_le_bytes());
            sections.extend_from_slice(&h.to_le_bytes());
            put_str(&mut sections, p);
        }
        let dof = mo + sections.len() as u64;
        for (p, keys) in &dirs {
            put_str(&mut sections, p);
            put_u32(&mut sections, keys.len());
            for k in keys {
                sections.extend_from_slice(&k.to_le_bytes());
            }
        }
        w.write_all(&sections)?;
        let mut tr = Vec::with_capacity(TRAILER_LEN);
        for v in [mo, manifest.len() as u64, fo, files.len() as u64, dof, dirs.len() as u64, hash_bytes(&sections)] {
            tr.extend_from_slice(&v.to_le_bytes());
        }
        let sum = hash_bytes(&tr);
        tr.extend_from_slice(&sum.to_le_bytes());
        w.write_all(&tr)?;
        w.inner.flush()?;
        Ok(w.pos)
    })();
    // The old file must be unmapped before it can be replaced on Windows.
    drop(old);
    match result {
        Ok(size) => {
            std::fs::rename(&tmp, &path).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })?;
            Ok(size)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

struct Counting<W: Write> {
    inner: W,
    pos: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{Symbol, SymbolKind};

    fn shard() -> SymbolDb {
        let mut db = SymbolDb::new();
        let f = db.files.intern("/a.h");
        db.files.set_content_hash(f, 7);
        let n = db.intern("Thing");
        db.add_symbol(Symbol::new(n, SymbolKind::Class, f, 3));
        db
    }

    fn parts<'a>(db: &'a SymbolDb, decls: &'a ScopeDecls, deps: &'a [u64]) -> RecordParts<'a> {
        RecordParts { path: "/a.h", flags_fp: 9, content_hash: 7, deps, idents: &[1, 2, 3], includes: &[], decls, shard: db }
    }

    #[test]
    fn records_round_trip_and_reject_damage() {
        let db = shard();
        let decls = ScopeDecls { names: vec!["std".into()], aliases: vec![("fs".into(), "std::filesystem".into())] };
        let inc = vec!["/b.h".to_owned()];
        let mut p = parts(&db, &decls, &[5, 6]);
        p.includes = &inc;
        let mut bytes = Vec::new();
        encode_record(&p, &mut bytes);

        let d = decode_record(&bytes, "/a.h", 9).unwrap();
        assert_eq!((d.content_hash, d.idents.as_slice(), d.includes.as_slice()), (7, &[1, 2, 3][..], &["/b.h".to_owned()][..]));
        assert_eq!(d.decls, decls);
        assert_eq!(d.shard.len(), 1);
        assert_eq!(record_deps(&bytes).unwrap().chunks_exact(8).map(le64).collect::<Vec<_>>(), [5, 6]);

        assert!(matches!(decode_record(&bytes, "/other.h", 9), Err(DecodeError::Stale)));
        assert!(matches!(decode_record(&bytes, "/a.h", 10), Err(DecodeError::Stale)));
        for n in 0..bytes.len() {
            assert!(decode_record(&bytes[..n], "/a.h", 9).is_err(), "truncated to {n}");
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x40;
            assert!(decode_record(&b, "/a.h", 9).is_err(), "flip at {i}");
        }
    }

    #[test]
    fn symmetric_difference_of_sorted_listings() {
        let old: Vec<u8> = [1u64, 3, 5, 9].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(sym_diff(&old, &[1, 4, 5, 10]), [3, 4, 9, 10]);
        assert!(sym_diff(&old, &[1, 3, 5, 9]).is_empty());
        assert_eq!(sym_diff(&[], &[2]), [2]);
        assert_eq!(sym_diff(&old, &[]), [1, 3, 5, 9]);
    }
}
