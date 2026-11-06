//! Every indexed file's text, lz4-compressed in memory.
//!
//! Type inference and references re-read the same declaring headers thousands of
//! times per request; this turns those disk reads into a memory copy.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use rayon::prelude::*;

use crate::base::source::SourceFile;
use crate::pp::vfs::path_key;
use crate::query::{Text, TextSource};

struct Packed {
    len: usize,
    data: Box<[u8]>,
}

fn pack(text: &str) -> Packed {
    Packed { len: text.len(), data: lz4_flex::block::compress(text.as_bytes()).into_boxed_slice() }
}

thread_local! {
    /// Last few unpacked files of this thread: one request asks for the same header over and over.
    static RECENT: RefCell<Vec<(Arc<Packed>, Arc<str>)>> = const { RefCell::new(Vec::new()) };
}
const RECENT_MAX: usize = 8;

fn unpack(p: &Arc<Packed>) -> Option<Arc<str>> {
    if let Some(hit) = RECENT.with(|r| r.borrow().iter().find(|(k, _)| Arc::ptr_eq(k, p)).map(|(_, t)| t.clone())) {
        return Some(hit);
    }
    let bytes = lz4_flex::block::decompress(&p.data, p.len).ok()?;
    let text: Arc<str> = String::from_utf8(bytes).ok()?.into();
    RECENT.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= RECENT_MAX {
            r.remove(0);
        }
        r.push((p.clone(), text.clone()));
    });
    Some(text)
}

#[derive(Default)]
pub struct TextStore {
    map: RwLock<HashMap<u64, Arc<Packed>>>,
    /// Bumped by every `put`/`remove`, so a slow disk read never overwrites a newer entry.
    epoch: AtomicU64,
    packed_bytes: AtomicUsize,
    raw_bytes: AtomicUsize,
}

impl TextStore {
    /// The text of `path`, read from disk (and kept) when not stored yet.
    pub fn get(&self, path: &str) -> Option<Arc<str>> {
        let key = path_key(path);
        let hit = self.map.read().ok()?.get(&key).cloned();
        match hit {
            Some(p) => unpack(&p),
            None => unpack(&self.load(path, key)?),
        }
    }

    fn load(&self, path: &str, key: u64) -> Option<Arc<Packed>> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let packed = Arc::new(pack(SourceFile::load(path).ok()?.text()));
        let mut map = self.map.write().ok()?;
        if self.epoch.load(Ordering::Acquire) != epoch {
            return Some(packed);
        }
        Some(self.insert(&mut map, key, packed, false))
    }

    fn insert(&self, map: &mut HashMap<u64, Arc<Packed>>, key: u64, packed: Arc<Packed>, replace: bool) -> Arc<Packed> {
        if let Some(old) = map.get(&key) {
            if !replace {
                return old.clone();
            }
            self.packed_bytes.fetch_sub(old.data.len(), Ordering::Relaxed);
            self.raw_bytes.fetch_sub(old.len, Ordering::Relaxed);
        }
        self.packed_bytes.fetch_add(packed.data.len(), Ordering::Relaxed);
        self.raw_bytes.fetch_add(packed.len, Ordering::Relaxed);
        map.insert(key, packed.clone());
        packed
    }

    /// Replaces the stored text of `path` (it changed on disk).
    pub fn put(&self, path: &str, text: &str) {
        let packed = Arc::new(pack(text));
        if let Ok(mut map) = self.map.write() {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.insert(&mut map, path_key(path), packed, true);
        }
    }

    /// Forgets `path`; the next read goes to disk.
    pub fn remove(&self, path: &str) {
        if let Ok(mut map) = self.map.write() {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            if let Some(old) = map.remove(&path_key(path)) {
                self.packed_bytes.fetch_sub(old.data.len(), Ordering::Relaxed);
                self.raw_bytes.fetch_sub(old.len, Ordering::Relaxed);
            }
        }
    }

    /// Loads every file in `paths` that is not stored yet, in parallel.
    pub fn warm(&self, paths: &[String]) {
        paths.par_iter().for_each(|p| {
            let key = path_key(p);
            if !self.map.read().is_ok_and(|m| m.contains_key(&key)) {
                self.load(p, key);
            }
        });
    }

    /// (files, raw bytes, compressed bytes).
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.map.read().map_or(0, |m| m.len()), self.raw_bytes.load(Ordering::Relaxed), self.packed_bytes.load(Ordering::Relaxed))
    }
}

impl TextSource for TextStore {
    fn text(&self, path: &str) -> Option<Text> {
        self.get(path).map(Text::Shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_replace_and_remove() {
        let dir = std::env::temp_dir().join(format!("ls_textstore_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.h");
        let path = f.to_string_lossy().replace('\\', "/");
        std::fs::write(&f, "struct A { int x; };\n".repeat(50)).unwrap();
        let s = TextStore::default();
        let first = s.get(&path).unwrap();
        assert_eq!(&*first, "struct A { int x; };\n".repeat(50));
        let (files, raw, packed) = s.stats();
        assert_eq!(files, 1);
        assert!(packed < raw);

        // Served from memory: the file on disk changing does not show until `put`.
        std::fs::write(&f, "changed").unwrap();
        assert_eq!(&*s.get(&path).unwrap(), &*first);
        s.put(&path, "changed");
        assert_eq!(&*s.get(&path).unwrap(), "changed");
        assert_eq!(s.stats().1, "changed".len());

        s.remove(&path);
        assert_eq!(s.stats().0, 0);
        std::fs::write(&f, "again").unwrap();
        assert_eq!(&*s.get(&path).unwrap(), "again");
        assert!(s.get("definitely/not/here.h").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
