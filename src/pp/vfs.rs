//! File access abstraction for `#include` resolution.
//!
//! The preprocessor only asks two questions of the world: "does this path
//! exist?" and "give me its contents". Keeping that behind a trait lets tests
//! run against an in-memory tree and lets the LSP overlay unsaved buffers.

use std::collections::HashMap;

use crate::source::SourceFile;

/// Shared, read-only file access. Implementations must be cheap to call from
/// many threads at once (no global locks on the hot path).
pub trait FileProvider: Sync {
    fn exists(&self, path: &str) -> bool;
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
        dir_cache::is_file(path)
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

mod dir_cache {
    use std::path::Path;
    use std::sync::{Arc, OnceLock, RwLock};

    use hashbrown::{HashMap, HashSet};
    use rustc_hash::FxBuildHasher;

    use crate::hash::hash_bytes;

    type Listing = HashSet<u64, FxBuildHasher>;
    type Shard = RwLock<HashMap<u64, Arc<OnceLock<Listing>>, FxBuildHasher>>;

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

    fn list(dir: &str) -> Listing {
        let mut out = Listing::default();
        let p = match dir {
            "" => "/".to_owned(),
            d if d.ends_with(':') => format!("{d}/"),
            d => d.to_owned(),
        };
        let Ok(rd) = std::fs::read_dir(&p) else { return out };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let is_file = ft.is_file() || (ft.is_symlink() && Path::new(&e.path()).is_file());
            if is_file && let Some(n) = e.file_name().to_str() {
                out.insert(ci_hash(n));
            }
        }
        out
    }

    pub fn is_file(path: &str) -> bool {
        let (dir, name) = split(path);
        if name.is_empty() {
            return false;
        }
        let key = ci_hash(dir);
        let shard = &shards()[key as usize % SHARDS];
        let cell = shard.read().unwrap().get(&key).cloned();
        let cell = cell.unwrap_or_else(|| shard.write().unwrap().entry(key).or_default().clone());
        // One thread lists the directory; concurrent askers wait for it instead of repeating the I/O.
        cell.get_or_init(|| list(dir)).contains(&ci_hash(name))
    }

    pub fn invalidate(path: &str) {
        let key = ci_hash(split(path).0);
        shards()[key as usize % SHARDS].write().unwrap().remove(&key);
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
    if cfg!(windows) {
        crate::hash::hash_bytes(p.to_ascii_lowercase().as_bytes())
    } else {
        crate::hash::hash_bytes(p.as_bytes())
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
