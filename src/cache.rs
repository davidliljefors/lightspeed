//! On-disk symbol shard cache (Phase 1).
//!
//! One file per `(source path, flags)` pair, fanned out over 256 directories.
//! A shard is valid only if both the XXH3 of the file's contents and the
//! XXH3 of its [`FlagSet`](crate::compile_commands::FlagSet) match, so parsing
//! is skipped for unchanged files.
//!
//! Contention: workers never share state here. Each shard is its own file,
//! written to a process-unique temp name and atomically renamed, and each
//! shard path is touched by exactly one worker per run.
//!
//! Known limit (by design for now): a shard is keyed by its own file's
//! contents, not those of the headers it includes. Once the preprocessor
//! exists, include-dependent state must be folded into `content_hash`.
//!
//! Format (little-endian): `magic "LSSD" | version u32 | content_hash u64 |
//! flags_hash u64 | path | names | files | symbols | bases | out_of_line | signatures | xxh3 u64` where
//! the trailer covers every preceding byte. Decoding validates every index,
//! so corrupt or hostile files yield an error, never a panic.

use crate::hash::hash_bytes;
use crate::ids::Idx;
use crate::symbols::{
    Access, BaseEdge, FamilyId, FileId, NameId, Signature, Symbol, SymbolDb, SymbolFlags, SymbolId, SymbolKind,
};
use std::fmt;
use std::io;
use std::path::PathBuf;

const MAGIC: &[u8; 4] = b"LSSD";
/// Bump whenever the layout or the meaning of stored data changes.
pub const FORMAT_VERSION: u32 = 4;
const SYMBOL_BYTES: usize = 32;
const BASE_BYTES: usize = 20;

/// Identifies what a shard was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardKey<'a> {
    pub path: &'a str,
    pub content_hash: u64,
    pub flags_hash: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Well-formed but built from different inputs or an older format.
    Stale,
    Corrupt(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale => f.write_str("stale shard"),
            Self::Corrupt(why) => write!(f, "corrupt shard: {why}"),
        }
    }
}

impl std::error::Error for DecodeError {}

struct Writer(Vec<u8>);

impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("shard section exceeds 4 GiB"));
    }
    fn opt(&mut self, id: Option<u32>) {
        self.u32(id.map_or(0, |i| i + 1));
    }
}

/// Serializes `db` into `out` (cleared first).
pub fn encode_shard(key: &ShardKey, db: &SymbolDb, out: &mut Vec<u8>) {
    out.clear();
    let mut w = Writer(std::mem::take(out));
    w.0.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
    w.u64(key.content_hash);
    w.u64(key.flags_hash);
    w.len(key.path.len());
    w.0.extend_from_slice(key.path.as_bytes());

    w.len(db.names.len());
    let mut end = 0usize;
    for (_, s) in db.names.iter() {
        end += s.len();
        w.len(end);
    }
    for (_, s) in db.names.iter() {
        w.0.extend_from_slice(s.as_bytes());
    }

    w.len(db.files.len());
    for f in db.files.ids() {
        let p = db.files.path(f);
        w.len(p.len());
        w.0.extend_from_slice(p.as_bytes());
        w.u64(db.files.content_hash(f));
    }

    w.len(db.len());
    for s in db.symbols().iter() {
        w.u32(s.name.index() as u32);
        w.opt(s.parent.map(|p| p.index() as u32));
        // Families are recomputed globally, never persisted.
        w.u32(0);
        w.u32(s.file.index() as u32);
        w.u32(s.name_offset);
        w.u32(s.extent_start);
        w.u32(s.extent_end);
        w.0.push(s.kind as u8);
        w.0.push(s.access as u8);
        w.0.extend_from_slice(&s.flags.bits().to_le_bytes());
    }

    w.len(db.bases().len());
    for b in db.bases() {
        w.u32(b.derived.index() as u32);
        w.u32(b.base_name.index() as u32);
        w.opt(b.qual.map(|q| q.index() as u32));
        w.opt(b.base.map(|p| p.index() as u32));
        w.0.push(b.access as u8);
        w.0.push(b.is_virtual as u8);
        w.0.extend_from_slice(&[0, 0]);
    }

    w.len(db.out_of_line().len());
    for &(s, q) in db.out_of_line() {
        w.u32(s.index() as u32);
        w.u32(q.index() as u32);
    }

    w.len(db.signatures().len());
    for s in db.signatures() {
        w.u32(s.symbol.index() as u32);
        w.u32(s.arity as u32);
        w.u32(s.hash);
    }

    let sum = hash_bytes(&w.0);
    w.u64(sum);
    *out = w.0;
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

type R<T> = Result<T, DecodeError>;

const fn bad(why: &'static str) -> DecodeError {
    DecodeError::Corrupt(why)
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or(bad("truncated"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn str(&mut self, n: usize) -> R<&'a str> {
        std::str::from_utf8(self.bytes(n)?).map_err(|_| bad("invalid utf-8"))
    }
    /// A count that must be satisfiable by the remaining bytes (guards allocations).
    fn count(&mut self, min_bytes_each: usize) -> R<usize> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_bytes_each) > self.b.len() - self.pos {
            return Err(bad("count exceeds data"));
        }
        Ok(n)
    }
    fn opt(&mut self, limit: usize) -> R<Option<u32>> {
        match self.u32()? {
            0 => Ok(None),
            v if (v as usize) <= limit => Ok(Some(v - 1)),
            _ => Err(bad("index out of range")),
        }
    }
    fn idx(&mut self, limit: usize) -> R<u32> {
        let v = self.u32()?;
        if (v as usize) < limit { Ok(v) } else { Err(bad("index out of range")) }
    }
}

/// Rebuilds a shard; `Stale` if it was built from different inputs.
pub fn decode_shard(bytes: &[u8], key: &ShardKey) -> R<SymbolDb> {
    if bytes.len() < 8 + 4 + 4 + 8 + 8 || &bytes[..4] != MAGIC {
        return Err(bad("bad header"));
    }
    let (body, trailer) = bytes.split_at(bytes.len() - 8);
    if hash_bytes(body) != u64::from_le_bytes(trailer.try_into().unwrap()) {
        return Err(bad("checksum mismatch"));
    }
    decode_body(body, key)
}

fn decode_body(body: &[u8], key: &ShardKey) -> R<SymbolDb> {
    let mut r = Reader { b: body, pos: 4 };
    if r.u32()? != FORMAT_VERSION {
        return Err(DecodeError::Stale);
    }
    let (content, flags) = (r.u64()?, r.u64()?);
    let plen = r.u32()? as usize;
    if content != key.content_hash || flags != key.flags_hash || r.str(plen)? != key.path {
        return Err(DecodeError::Stale);
    }

    let mut db = SymbolDb::new();

    let n_names = r.count(4)?;
    let mut ends = Vec::with_capacity(n_names);
    for _ in 0..n_names {
        ends.push(r.u32()? as usize);
    }
    let total = ends.last().copied().unwrap_or(0);
    let blob = r.str(total)?;
    let mut start = 0;
    for (i, &end) in ends.iter().enumerate() {
        let s = blob.get(start..end).ok_or(bad("bad name range"))?;
        // Interning in order reproduces the original IDs unless there are duplicates.
        if db.names.intern(s).index() != i {
            return Err(bad("duplicate name"));
        }
        start = end;
    }

    let n_files = r.count(12)?;
    for i in 0..n_files {
        let len = r.u32()? as usize;
        let path = r.str(len)?;
        let hash = r.u64()?;
        let id = db.files.intern(path);
        if id.index() != i {
            return Err(bad("duplicate file"));
        }
        db.files.set_content_hash(id, hash);
    }

    let n_syms = r.count(SYMBOL_BYTES)?;
    for i in 0..n_syms {
        let name = r.idx(n_names)?;
        // Parents are always added before their children; this also rules out cycles.
        let parent = r.opt(i)?;
        let _family = r.u32()?;
        let file = r.idx(n_files)?;
        let (name_offset, extent_start, extent_end) = (r.u32()?, r.u32()?, r.u32()?);
        let tail = r.bytes(4)?;
        db.add_symbol(Symbol {
            name: NameId::from_usize(name as usize),
            parent: parent.map(|p| SymbolId::from_usize(p as usize)),
            family: None::<FamilyId>,
            file: FileId::from_usize(file as usize),
            name_offset,
            extent_start,
            extent_end,
            kind: SymbolKind::from_u8(tail[0]).ok_or(bad("bad kind"))?,
            access: Access::from_u8(tail[1]).ok_or(bad("bad access"))?,
            flags: SymbolFlags::from_bits_truncate(u16::from_le_bytes([tail[2], tail[3]])),
        });
    }

    let n_bases = r.count(BASE_BYTES)?;
    for _ in 0..n_bases {
        let derived = r.idx(n_syms)?;
        let base_name = r.idx(n_names)?;
        let qual = r.opt(n_names)?;
        let base = r.opt(n_syms)?;
        let tail = r.bytes(4)?;
        db.add_base(BaseEdge {
            derived: SymbolId::from_usize(derived as usize),
            base_name: NameId::from_usize(base_name as usize),
            qual: qual.map(|q| NameId::from_usize(q as usize)),
            base: base.map(|p| SymbolId::from_usize(p as usize)),
            access: Access::from_u8(tail[0]).ok_or(bad("bad access"))?,
            is_virtual: tail[1] != 0,
        });
    }

    let n_ool = r.count(8)?;
    for _ in 0..n_ool {
        let s = r.idx(n_syms)?;
        let q = r.idx(n_names)?;
        db.add_out_of_line(SymbolId::from_usize(s as usize), NameId::from_usize(q as usize));
    }

    let n_sigs = r.count(12)?;
    let mut prev: Option<u32> = None;
    for _ in 0..n_sigs {
        let s = r.idx(n_syms)?;
        let arity = r.u32()?;
        let hash = r.u32()?;
        if prev.is_some_and(|p| p >= s) || arity > u16::MAX as u32 {
            return Err(bad("bad signature"));
        }
        prev = Some(s);
        db.add_signature(Signature { symbol: SymbolId::from_usize(s as usize), arity: arity as u16, hash });
    }

    if r.pos != body.len() {
        return Err(bad("trailing bytes"));
    }
    Ok(db)
}

/// Result of a cache probe.
#[derive(Debug)]
pub enum Lookup {
    Hit(Box<SymbolDb>),
    Miss,
    /// A shard exists but was built from different content/flags/format.
    Stale,
    Corrupt(&'static str),
}

pub struct ShardCache {
    root: PathBuf,
}

impl ShardCache {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn shard_path(&self, key: &ShardKey) -> PathBuf {
        let mut h = crate::hash::FieldHasher::new();
        h.str(key.path).u64(key.flags_hash);
        let h = h.finish();
        self.root.join(format!("{:02x}", h >> 56)).join(format!("{h:016x}.shard"))
    }

    pub fn load(&self, key: &ShardKey) -> Lookup {
        match std::fs::read(self.shard_path(key)) {
            Ok(bytes) => match decode_shard(&bytes, key) {
                Ok(db) => Lookup::Hit(Box::new(db)),
                Err(DecodeError::Stale) => Lookup::Stale,
                Err(DecodeError::Corrupt(why)) => Lookup::Corrupt(why),
            },
            Err(_) => Lookup::Miss,
        }
    }

    pub fn store(&self, key: &ShardKey, db: &SymbolDb) -> io::Result<()> {
        let mut buf = Vec::new();
        encode_shard(key, db, &mut buf);
        let path = self.shard_path(key);
        std::fs::create_dir_all(path.parent().expect("shard path has a parent"))?;
        // One worker owns a given shard path per run; the pid separates processes.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{Symbol, SymbolFlags};
    use rayon::prelude::*;

    const KEY: ShardKey<'static> = ShardKey { path: "/src/a.h", content_hash: 0xAA, flags_hash: 0xBB };

    fn sample() -> SymbolDb {
        let mut db = SymbolDb::new();
        let file = db.files.intern("/src/a.h");
        db.files.set_content_hash(file, 0xAA);
        let ns = db.intern("engine");
        let cls = db.intern("Actor");
        let tick = db.intern("Tick");
        let base = db.intern("Base");
        let ns_id = db.add_symbol(Symbol::new(ns, SymbolKind::Namespace, file, 10));
        let c = db.add_symbol(Symbol::new(cls, SymbolKind::Class, file, 40).with_parent(Some(ns_id)).with_extent(30, 400));
        db.add_symbol(
            Symbol::new(tick, SymbolKind::Method, file, 100)
                .with_parent(Some(c))
                .with_access(Access::Protected)
                .with_flags(SymbolFlags::VIRTUAL | SymbolFlags::CONST),
        );
        db.add_base(BaseEdge { derived: c, base_name: base, qual: None, base: None, access: Access::Public, is_virtual: true });
        db.add_out_of_line(c, base);
        db.add_signature(Signature { symbol: c, arity: 2, hash: 0xDEAD });
        db.rebuild_indexes();
        db
    }

    fn assert_same(a: &SymbolDb, b: &SymbolDb) {
        assert_eq!(a.symbols().as_slice(), b.symbols().as_slice());
        assert_eq!(a.bases(), b.bases());
        assert_eq!(a.out_of_line(), b.out_of_line());
        assert_eq!(a.signatures(), b.signatures());
        assert_eq!(a.names.iter().collect::<Vec<_>>(), b.names.iter().collect::<Vec<_>>());
        assert_eq!(a.files.len(), b.files.len());
        for f in a.files.ids() {
            assert_eq!(a.files.path(f), b.files.path(f));
            assert_eq!(a.files.content_hash(f), b.files.content_hash(f));
        }
    }

    fn encoded() -> Vec<u8> {
        let mut v = Vec::new();
        encode_shard(&KEY, &sample(), &mut v);
        v
    }

    #[test]
    fn roundtrip() {
        let db = sample();
        let got = decode_shard(&encoded(), &KEY).unwrap();
        assert_same(&db, &got);
    }

    #[test]
    fn empty_db_roundtrip() {
        let mut v = Vec::new();
        encode_shard(&KEY, &SymbolDb::new(), &mut v);
        let db = decode_shard(&v, &KEY).unwrap();
        assert!(db.is_empty() && db.names.is_empty());
    }

    #[test]
    fn stale_when_any_input_differs() {
        let bytes = encoded();
        for key in [
            ShardKey { content_hash: 1, ..KEY },
            ShardKey { flags_hash: 1, ..KEY },
            ShardKey { path: "/src/b.h", ..KEY },
        ] {
            assert_eq!(decode_shard(&bytes, &key).unwrap_err(), DecodeError::Stale);
        }
    }

    #[test]
    fn rejects_every_truncation_and_bit_flip() {
        let bytes = encoded();
        for n in 0..bytes.len() {
            assert!(decode_shard(&bytes[..n], &KEY).is_err(), "truncated to {n}");
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x40;
            assert!(decode_shard(&b, &KEY).is_err(), "flip at {i}");
        }
    }

    /// Mutations with a *valid* checksum must still never panic or yield invalid IDs.
    #[test]
    fn validation_survives_checksum_forgery() {
        let bytes = encoded();
        let mut accepted = 0;
        for i in 4..bytes.len() - 8 {
            for delta in [1u8, 0x80, 0xFF] {
                let mut b = bytes.clone();
                b[i] = b[i].wrapping_add(delta);
                let n = b.len() - 8;
                let sum = hash_bytes(&b[..n]);
                b[n..].copy_from_slice(&sum.to_le_bytes());
                if let Ok(db) = decode_shard(&b, &KEY) {
                    accepted += 1;
                    // Whatever got through must be internally consistent.
                    let mut copy = SymbolDb::new();
                    copy.merge(&db);
                    for id in db.symbols().ids() {
                        let _ = db.qualified_name(id);
                    }
                }
            }
        }
        // Some mutations are legitimately valid (offsets, flags); most are not.
        assert!(accepted < (bytes.len() * 3) / 2);
    }

    #[test]
    fn rejects_parent_cycles_and_forward_parents() {
        let mut b = encoded();
        // First symbol record starts after: header, path, names, files.
        let db = sample();
        let mut off = 4 + 4 + 8 + 8 + 4 + KEY.path.len() + 4 + 4 * db.names.len() + db.names.arena_bytes();
        off += 4 + 4 + db.files.path(db.files.ids().next().unwrap()).len() + 8 + 4; // files + n_syms
        b[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes()); // symbol 0 -> parent symbol 0
        let n = b.len() - 8;
        let sum = hash_bytes(&b[..n]);
        b[n..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_shard(&b, &KEY).unwrap_err(), DecodeError::Corrupt("index out of range"));
    }

    #[test]
    fn rejects_huge_counts() {
        // Huge counts must fail fast rather than allocate.
        let mut b = encoded();
        let n = b.len() - 8;
        let off = 4 + 4 + 8 + 8 + 4 + KEY.path.len();
        b[off..off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let sum = hash_bytes(&b[..n]);
        b[n..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_shard(&b, &KEY).unwrap_err(), DecodeError::Corrupt("count exceeds data"));
    }

    #[test]
    fn disk_cache_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShardCache::open(dir.path().join("cache")).unwrap();
        assert!(matches!(cache.load(&KEY), Lookup::Miss));

        cache.store(&KEY, &sample()).unwrap();
        match cache.load(&KEY) {
            Lookup::Hit(db) => assert_same(&db, &sample()),
            other => panic!("expected hit, got {other:?}"),
        }
        // Same path+flags, changed content: same slot, now stale.
        let changed = ShardKey { content_hash: 0xCC, ..KEY };
        assert!(matches!(cache.load(&changed), Lookup::Stale));
        cache.store(&changed, &sample()).unwrap();
        assert!(matches!(cache.load(&changed), Lookup::Hit(_)));
        assert!(matches!(cache.load(&KEY), Lookup::Stale));
        // Different flags for the same file get their own shard.
        let other_flags = ShardKey { flags_hash: 0xDD, ..KEY };
        assert_ne!(cache.shard_path(&KEY), cache.shard_path(&other_flags));
        assert!(matches!(cache.load(&other_flags), Lookup::Miss));
    }

    #[test]
    fn detects_on_disk_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShardCache::open(dir.path()).unwrap();
        cache.store(&KEY, &sample()).unwrap();
        let p = cache.shard_path(&KEY);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[20] ^= 1;
        std::fs::write(&p, bytes).unwrap();
        assert!(matches!(cache.load(&KEY), Lookup::Corrupt(_)));
        std::fs::write(&p, b"").unwrap();
        assert!(matches!(cache.load(&KEY), Lookup::Corrupt(_)));
    }

    #[test]
    fn parallel_store_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShardCache::open(dir.path()).unwrap();
        let paths: Vec<String> = (0..400).map(|i| format!("/src/f{i}.h")).collect();
        paths.par_iter().for_each(|p| {
            let key = ShardKey { path: p, content_hash: 7, flags_hash: 9 };
            cache.store(&key, &sample()).unwrap();
        });
        let hits = paths
            .par_iter()
            .filter(|p| matches!(cache.load(&ShardKey { path: p, content_hash: 7, flags_hash: 9 }), Lookup::Hit(_)))
            .count();
        assert_eq!(hits, 400);
        let leftovers = walk(dir.path()).into_iter().filter(|p| p.extension().is_some_and(|e| e != "shard")).count();
        assert_eq!(leftovers, 0, "no temp files may remain");
    }

    fn walk(p: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(p).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() { out.extend(walk(&path)) } else { out.push(path) }
        }
        out
    }
}
