//! Zero-copy source loading.
//!
//! Large files are memory-mapped; small files are read into one exact-size
//! buffer (a map/unmap pair costs more than a `read` for tiny files). Either
//! way the lexer sees a validated `&str` borrowed from the file's storage.
//!
//! Caveats of mapping:
//! * Mapped files must not be truncated while in use (SIGBUS on Unix; on
//!   Windows truncation/deletion fails). The indexer should drop `SourceFile`s
//!   right after lexing; the LSP should feed unsaved buffers via
//!   [`SourceFile::from_string`].
//! * A leading UTF-8 BOM is skipped, so offsets are relative to the text after
//!   the BOM. Invalid UTF-8 is (by default) repaired lossily, which may shift
//!   offsets after the first bad byte; see [`SourceFile::is_lossy`].

use memchr::memchr_iter;
use memmap2::Mmap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Offsets are `u32`, so files must be smaller than 4 GiB.
pub const MAX_FILE_SIZE: u64 = u32::MAX as u64;

#[derive(Debug, Clone, Copy)]
pub struct LoadOptions {
    /// Files at least this large are memory-mapped. `u64::MAX` disables mmap.
    pub mmap_threshold: u64,
    /// Repair invalid UTF-8 (U+FFFD) instead of failing.
    pub lossy_utf8: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self { mmap_threshold: 64 * 1024, lossy_utf8: true }
    }
}

#[derive(Debug)]
pub enum LoadError {
    Io(io::Error),
    TooLarge { size: u64 },
    InvalidUtf8 { valid_up_to: usize },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::TooLarge { size } => write!(f, "file too large ({size} bytes, max {MAX_FILE_SIZE})"),
            Self::InvalidUtf8 { valid_up_to } => write!(f, "invalid UTF-8 after byte {valid_up_to}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for LoadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

enum Storage {
    /// Whole map validated as UTF-8 at load time.
    Mapped(Mmap),
    Owned(String),
}

pub struct SourceFile {
    storage: Storage,
    /// Start of the text within storage (skips a BOM).
    start: usize,
    lossy: bool,
}

fn bom_len(bytes: &[u8]) -> usize {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 }
}

impl SourceFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        Self::load_with(path, &LoadOptions::default())
    }

    pub fn load_with(path: impl AsRef<Path>, opts: &LoadOptions) -> Result<Self, LoadError> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        if size > MAX_FILE_SIZE {
            return Err(LoadError::TooLarge { size });
        }
        if size == 0 {
            return Ok(Self::from_string(String::new()));
        }
        if size >= opts.mmap_threshold {
            // SAFETY: see the module docs; the file must not be truncated
            // while this `SourceFile` is alive.
            if let Ok(map) = unsafe { Mmap::map(&file) }
                && map.len() as u64 <= MAX_FILE_SIZE
            {
                return Self::from_map(map, opts);
            }
        }
        let mut buf = Vec::with_capacity(size as usize + 1);
        file.read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_FILE_SIZE {
            return Err(LoadError::TooLarge { size: buf.len() as u64 });
        }
        Self::from_bytes(buf, opts)
    }

    fn from_map(map: Mmap, opts: &LoadOptions) -> Result<Self, LoadError> {
        #[cfg(unix)]
        let _ = map.advise(memmap2::Advice::Sequential);
        let start = bom_len(&map);
        match std::str::from_utf8(&map[start..]) {
            Ok(_) => Ok(Self { storage: Storage::Mapped(map), start, lossy: false }),
            Err(_) if opts.lossy_utf8 => {
                let s = String::from_utf8_lossy(&map[start..]).into_owned();
                Ok(Self { storage: Storage::Owned(s), start: 0, lossy: true })
            }
            Err(e) => Err(LoadError::InvalidUtf8 { valid_up_to: start + e.valid_up_to() }),
        }
    }

    fn from_bytes(buf: Vec<u8>, opts: &LoadOptions) -> Result<Self, LoadError> {
        let start = bom_len(&buf);
        match String::from_utf8(buf) {
            Ok(s) => Ok(Self { storage: Storage::Owned(s), start, lossy: false }),
            Err(e) if opts.lossy_utf8 => {
                let bytes = e.into_bytes();
                let s = String::from_utf8_lossy(&bytes[start..]).into_owned();
                Ok(Self { storage: Storage::Owned(s), start: 0, lossy: true })
            }
            Err(e) => Err(LoadError::InvalidUtf8 { valid_up_to: e.utf8_error().valid_up_to() }),
        }
    }

    /// Wraps in-memory text (e.g. an unsaved editor buffer).
    ///
    /// Panics if the text is 4 GiB or larger.
    pub fn from_string(text: String) -> Self {
        assert!(text.len() as u64 <= MAX_FILE_SIZE, "source text exceeds 4 GiB");
        let start = bom_len(text.as_bytes());
        Self { storage: Storage::Owned(text), start, lossy: false }
    }

    #[inline]
    pub fn text(&self) -> &str {
        match &self.storage {
            // SAFETY: the map was validated as UTF-8 from `start` in `from_map`,
            // and `start` sits on a char boundary (it is 0 or a full BOM).
            Storage::Mapped(map) => unsafe { std::str::from_utf8_unchecked(&map[self.start..]) },
            Storage::Owned(s) => &s[self.start..],
        }
    }

    pub fn len(&self) -> usize {
        self.text().len()
    }

    pub fn is_empty(&self) -> bool {
        self.text().is_empty()
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self.storage, Storage::Mapped(_))
    }

    /// True if invalid UTF-8 was replaced while loading.
    pub fn is_lossy(&self) -> bool {
        self.lossy
    }
}

/// Byte offsets of line starts, for offset <-> (line, column) conversion.
#[derive(Debug, Clone, Default)]
pub struct LineIndex {
    starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        starts.push(0);
        starts.extend(memchr_iter(b'\n', text.as_bytes()).map(|i| i as u32 + 1));
        Self { starts }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset where the 0-based `line` starts.
    pub fn line_start(&self, line: usize) -> Option<u32> {
        self.starts.get(line).copied()
    }

    /// 0-based line containing `offset`.
    pub fn line_of(&self, offset: u32) -> usize {
        self.starts.partition_point(|&s| s <= offset) - 1
    }

    /// 0-based `(line, byte column)`. LSP wants UTF-16 columns; convert at the edge.
    pub fn line_col(&self, offset: u32) -> (usize, u32) {
        let line = self.line_of(offset);
        (line, offset - self.starts[line])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn opts(threshold: u64, lossy: bool) -> LoadOptions {
        LoadOptions { mmap_threshold: threshold, lossy_utf8: lossy }
    }

    #[test]
    fn loads_mapped_and_owned_identically() {
        let f = write_tmp(b"int main() { return 0; }\n");
        let mapped = SourceFile::load_with(f.path(), &opts(0, true)).unwrap();
        let owned = SourceFile::load_with(f.path(), &opts(u64::MAX, true)).unwrap();
        assert!(mapped.is_mapped());
        assert!(!owned.is_mapped());
        assert_eq!(mapped.text(), owned.text());
        assert_eq!(mapped.len(), 25);
    }

    #[test]
    fn empty_file() {
        let f = write_tmp(b"");
        let s = SourceFile::load_with(f.path(), &opts(0, true)).unwrap();
        assert!(s.is_empty());
        assert!(!s.is_mapped());
    }

    #[test]
    fn strips_bom() {
        let f = write_tmp(b"\xEF\xBB\xBFclass A {};");
        for t in [0, u64::MAX] {
            let s = SourceFile::load_with(f.path(), &opts(t, true)).unwrap();
            assert_eq!(s.text(), "class A {};");
        }
        assert_eq!(SourceFile::from_string("\u{feff}x".into()).text(), "x");
    }

    #[test]
    fn invalid_utf8_lossy_or_error() {
        let f = write_tmp(b"// caf\xE9\nint x;");
        for t in [0, u64::MAX] {
            let s = SourceFile::load_with(f.path(), &opts(t, true)).unwrap();
            assert!(s.is_lossy());
            assert_eq!(s.text(), "// caf\u{FFFD}\nint x;");
            let err = SourceFile::load_with(f.path(), &opts(t, false)).err().unwrap();
            assert!(matches!(err, LoadError::InvalidUtf8 { valid_up_to: 6 }), "{err}");
        }
    }

    #[test]
    fn missing_file_is_io_error() {
        let err = SourceFile::load("definitely/not/here.cpp").err().unwrap();
        assert!(matches!(err, LoadError::Io(_)));
    }

    #[test]
    fn line_index() {
        let text = "ab\ncd\r\n\nlast";
        let li = LineIndex::new(text);
        assert_eq!(li.line_count(), 4);
        assert_eq!(li.line_col(0), (0, 0));
        assert_eq!(li.line_col(2), (0, 2));
        assert_eq!(li.line_col(3), (1, 0));
        assert_eq!(li.line_col(7), (2, 0));
        assert_eq!(li.line_col(8), (3, 0));
        assert_eq!(li.line_col(text.len() as u32), (3, 4));
        assert_eq!(li.line_start(1), Some(3));
        assert_eq!(LineIndex::new("").line_col(0), (0, 0));
    }
}
