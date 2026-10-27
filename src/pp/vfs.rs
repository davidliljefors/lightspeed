//! File access abstraction for `#include` resolution.
//!
//! The preprocessor only asks two questions of the world: "does this path
//! exist?" and "give me its contents". Keeping that behind a trait lets tests
//! run against an in-memory tree and lets the LSP overlay unsaved buffers.

use std::collections::HashMap;
use std::path::Path;

use crate::source::SourceFile;

/// Shared, read-only file access. Implementations must be cheap to call from
/// many threads at once (no global locks on the hot path).
pub trait FileProvider: Sync {
    fn exists(&self, path: &str) -> bool;
    fn load(&self, path: &str) -> Option<SourceFile>;
}

/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskFs;

impl FileProvider for DiskFs {
    fn exists(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn load(&self, path: &str) -> Option<SourceFile> {
        SourceFile::load(path).ok()
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
