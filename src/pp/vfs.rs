//! File access abstraction for `#include` resolution.
//!
//! The preprocessor only asks two questions of the world: "does this path
//! exist?" and "give me its contents". Keeping that behind a trait lets tests
//! run against an in-memory tree and lets the LSP overlay unsaved buffers.

use std::collections::HashMap;

use crate::base::source::SourceFile;

/// Shared, read-only file access. Implementations must be cheap to call from
/// many threads at once (no global locks on the hot path).
pub trait FileProvider: Sync {
    fn exists(&self, path: &str) -> bool;
    /// [`Self::exists`] when the caller already has `key == path_key(path)`.
    fn exists_keyed(&self, path: &str, _key: u64) -> bool {
        self.exists(path)
    }
    fn load(&self, path: &str) -> Option<SourceFile>;
}

/// The real file system. Existence checks are answered from process-wide
/// directory listings (one `read_dir` per directory instead of one `stat` per
/// probed candidate), which is what keeps include resolution fast when there are
/// hundreds of include directories and dozens of threads.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskFs;

impl FileProvider for DiskFs {
    fn exists(&self, path: &str) -> bool {
        dir_cache::is_file(path, path_key(path))
    }

    fn exists_keyed(&self, path: &str, key: u64) -> bool {
        dir_cache::is_file(path, key)
    }

    fn load(&self, path: &str) -> Option<SourceFile> {
        SourceFile::load(path).ok()
    }
}

/// Forgets the cached listing of the directory containing `path` (call after
/// a file there was created, deleted or renamed).
pub fn invalidate_dir_of(path: &str) {
    dir_cache::invalidate(path);
}

/// Forgets every cached directory listing, as a fresh process would start.
pub fn clear_dir_cache() {
    dir_cache::clear();
}

/// Every directory listed so far with the sorted [`path_key`]s of the files in
/// it. Every `#include` candidate ever probed lies in one of these, so this is
/// the world the preprocessor's include searches saw.
pub fn dir_snapshot() -> Vec<(String, Vec<u64>)> {
    dir_cache::snapshot()
}

/// Lists `dir` from disk now (bypassing the cache): sorted [`path_key`]s of its files.
pub fn list_dir_keys(dir: &str) -> Vec<u64> {
    dir_cache::list(dir).sorted_keys()
}

/// What a missed `#include` candidate records as its dependency: the file name
/// alone, so one entry covers the candidate in every search directory. A file of
/// that name appearing anywhere later invalidates it (never wrongly the reverse).
pub fn name_key(path: &str) -> u64 {
    path_key(path.rsplit('/').next().unwrap_or(path)) ^ 0x6e61_6d65_6b65_7931
}

/// Lists `dir` from disk now: `(path_key, name_key)` of its files, sorted by `path_key`.
pub fn list_dir_entries(dir: &str) -> Vec<(u64, u64)> {
    let mut v = Vec::new();
    dir_cache::each_file(dir, |full| v.push((path_key(full), name_key(full))));
    v.sort_unstable();
    v
}

/// Replaces the cached listing of `dir` with `keys` (a fresh [`list_dir_keys`]).
pub fn seed_dir(dir: &str, keys: &[u64]) {
    dir_cache::seed(dir, keys);
}

mod dir_cache {
    use std::path::Path;
    use std::sync::{Arc, OnceLock, RwLock};

    use hashbrown::{HashMap, HashSet};
    use rustc_hash::FxBuildHasher;

    use super::path_key;
    use crate::base::hash::hash_bytes;

    /// The files of one directory, as [`path_key`]s of their full paths.
    pub struct Dir {
        path: String,
        keys: HashSet<u64, FxBuildHasher>,
    }

    impl Dir {
        pub fn sorted_keys(&self) -> Vec<u64> {
            let mut v: Vec<u64> = self.keys.iter().copied().collect();
            v.sort_unstable();
            v
        }
    }

    type Shard = RwLock<HashMap<u64, Arc<OnceLock<Dir>>, FxBuildHasher>>;

    const SHARDS: usize = 64;

    fn shards() -> &'static [Shard; SHARDS] {
        static S: OnceLock<[Shard; SHARDS]> = OnceLock::new();
        S.get_or_init(|| std::array::from_fn(|_| RwLock::new(HashMap::with_hasher(FxBuildHasher))))
    }

    /// Case-insensitive on Windows, without allocating for ordinary names.
    fn ci_hash(s: &str) -> u64 {
        if !cfg!(windows) {
            return hash_bytes(s.as_bytes());
        }
        let b = s.as_bytes();
        if b.len() <= 256 {
            let mut buf = [0u8; 256];
            for (d, c) in buf.iter_mut().zip(b) {
                *d = c.to_ascii_lowercase();
            }
            hash_bytes(&buf[..b.len()])
        } else {
            hash_bytes(&b.to_ascii_lowercase())
        }
    }

    fn split(path: &str) -> (&str, &str) {
        match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => (".", path),
        }
    }

    /// Calls `f` with the full path of every file in `dir`.
    pub fn each_file(dir: &str, mut f: impl FnMut(&str)) {
        let p = match dir {
            "" => "/".to_owned(),
            d if d.ends_with(':') => format!("{d}/"),
            d => d.to_owned(),
        };
        let Ok(rd) = std::fs::read_dir(&p) else { return };
        let mut full = String::with_capacity(dir.len() + 64);
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let is_file = ft.is_file() || (ft.is_symlink() && Path::new(&e.path()).is_file());
            if is_file && let Some(n) = e.file_name().to_str() {
                full.clear();
                full.push_str(dir);
                full.push('/');
                full.push_str(n);
                f(&full);
            }
        }
    }

    pub fn list(dir: &str) -> Dir {
        let mut out = Dir { path: dir.to_owned(), keys: HashSet::default() };
        each_file(dir, |full| {
            out.keys.insert(path_key(full));
        });
        out
    }

    fn cell_of(dir: &str) -> Arc<OnceLock<Dir>> {
        let key = ci_hash(dir);
        let shard = &shards()[key as usize % SHARDS];
        let cell = shard.read().unwrap().get(&key).cloned();
        cell.unwrap_or_else(|| shard.write().unwrap().entry(key).or_default().clone())
    }

    pub fn is_file(path: &str, key: u64) -> bool {
        let (dir, name) = split(path);
        if name.is_empty() {
            return false;
        }
        // One thread lists the directory; concurrent askers wait for it instead of repeating the I/O.
        cell_of(dir).get_or_init(|| list(dir)).keys.contains(&key)
    }

    pub fn invalidate(path: &str) {
        let key = ci_hash(split(path).0);
        shards()[key as usize % SHARDS].write().unwrap().remove(&key);
    }

    pub fn seed(dir: &str, keys: &[u64]) {
        let key = ci_hash(dir);
        let cell = Arc::new(OnceLock::new());
        let _ = cell.set(Dir { path: dir.to_owned(), keys: keys.iter().copied().collect() });
        shards()[key as usize % SHARDS].write().unwrap().insert(key, cell);
    }

    pub fn clear() {
        for sh in shards() {
            sh.write().unwrap().clear();
        }
    }

    pub fn snapshot() -> Vec<(String, Vec<u64>)> {
        let mut out = Vec::new();
        for sh in shards() {
            for cell in sh.read().unwrap().values() {
                if let Some(d) = cell.get() {
                    out.push((d.path.clone(), d.sorted_keys()));
                }
            }
        }
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// In-memory file tree; paths are stored after [`normalize_path`].
#[derive(Clone, Debug, Default)]
pub struct MemFs {
    files: HashMap<String, String>,
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, path: &str, text: &str) -> Self {
        self.add(path, text);
        self
    }

    pub fn add(&mut self, path: &str, text: &str) {
        let mut p = path.to_owned();
        normalize_path(&mut p);
        self.files.insert(p, text.to_owned());
    }
}

impl FileProvider for MemFs {
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    fn load(&self, path: &str) -> Option<SourceFile> {
        self.files.get(path).map(|s| SourceFile::from_string(s.clone()))
    }
}

/// Lexically normalizes a path in place: `\` becomes `/`, `.` segments and
/// duplicate slashes vanish, and `a/b/..` collapses to `a`. Leading `..`
/// segments of relative paths are kept. No file system access.
pub fn normalize_path(p: &mut String) {
    if !p.contains(['\\', '.']) && !p.contains("//") {
        return;
    }
    let src = std::mem::take(p);
    let rooted = src.starts_with(['/', '\\']);
    let mut segs: Vec<&str> = Vec::new();
    for s in src.split(['/', '\\']) {
        match s {
            "" | "." => {}
            ".." => match segs.last() {
                Some(&l) if l != ".." && !l.ends_with(':') => {
                    segs.pop();
                }
                Some(&l) if l.ends_with(':') => {}
                _ if rooted => {}
                _ => segs.push(".."),
            },
            _ => segs.push(s),
        }
    }
    if rooted {
        p.push('/');
    }
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            p.push('/');
        }
        p.push_str(s);
    }
}

/// Stable identity of a path for "already included" bookkeeping.
/// Case-insensitive on Windows.
pub fn path_key(p: &str) -> u64 {
    let b = p.as_bytes();
    if !cfg!(windows) {
        return crate::base::hash::hash_bytes(b);
    }
    if b.len() <= 256 {
        let mut buf = [0u8; 256];
        for (d, c) in buf.iter_mut().zip(b) {
            *d = c.to_ascii_lowercase();
        }
        crate::base::hash::hash_bytes(&buf[..b.len()])
    } else {
        crate::base::hash::hash_bytes(&b.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        let mut p = s.to_owned();
        normalize_path(&mut p);
        p
    }

    #[test]
    fn disk_listing_cache_answers_existence_and_invalidates() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_str().unwrap().replace('\\', "/");
        std::fs::write(format!("{root}/A.h"), "").unwrap();
        std::fs::create_dir(format!("{root}/sub")).unwrap();
        let fs = DiskFs;
        assert!(fs.exists(&format!("{root}/A.h")));
        assert!(!fs.exists(&format!("{root}/sub")), "directories are not files");
        assert!(!fs.exists(&format!("{root}/missing.h")));
        assert!(!fs.exists(&format!("{root}/nodir/x.h")));
        if cfg!(windows) {
            assert!(fs.exists(&format!("{root}/a.H")));
        }
        std::fs::write(format!("{root}/new.h"), "").unwrap();
        assert!(!fs.exists(&format!("{root}/new.h")), "listing is cached until invalidated");
        invalidate_dir_of(&format!("{root}/new.h"));
        assert!(fs.exists(&format!("{root}/new.h")));
    }

    #[test]
    fn normalizes() {
        assert_eq!(n("a/b/../c.h"), "a/c.h");
        assert_eq!(n("a\\b\\.\\c.h"), "a/b/c.h");
        assert_eq!(n("./a//b"), "a/b");
        assert_eq!(n("../a"), "../a");
        assert_eq!(n("a/../../b"), "../b");
        assert_eq!(n("/a/../../b"), "/b");
        assert_eq!(n("C:/x/../y.h"), "C:/y.h");
        assert_eq!(n("C:/../y.h"), "C:/y.h");
    }
}
