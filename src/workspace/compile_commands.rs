//! `compile_commands.json` ingestion.
//!
//! The file is memory-mapped and parsed zero-copy with serde (strings borrow
//! from the map unless they contain JSON escapes). Per-entry flag extraction
//! runs in parallel; entries with identical flags then share one [`FlagSet`]
//! (a typical engine has thousands of TUs but only a handful of distinct flag
//! sets), which keeps memory small and makes `flags_hash` a cheap cache key.
//!
//! Extracted: `-I`/`-isystem`/`-iquote`/`-idirafter`/`-imsvc`, `-D`/`-U`,
//! `-include`, `-std=`; plus `/I`, `/D`, `/U`, `/FI`, `/std:`,
//! `/external:I` when the compiler is `cl`/`clang-cl`. Everything else is
//! ignored. Paths are made absolute against `directory` (no filesystem
//! access, no symlink resolution) and use `/` separators on Windows.
//!
//! Command strings are split shell-style: whitespace separates, double or
//! single quotes group, and `\"` is a literal quote. Other backslashes are
//! literal so Windows paths survive.

use crate::base::hash::FieldHasher;
use crate::base::ids::IndexVec;
use crate::base::source::{LoadError, SourceFile};
use hashbrown::HashTable;
use rayon::prelude::*;
use rustc_hash::FxBuildHasher;
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt;
use std::hash::BuildHasher;
use std::path::Path;

crate::define_id!(
    /// Index of a deduplicated [`FlagSet`] in [`CompileDb::flag_sets`].
    pub FlagsId
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IncludeKind {
    /// `-iquote`: only for `#include "..."`.
    Quote,
    /// `-I`
    Normal,
    /// `-isystem`, `-idirafter`, `-imsvc`, `/external:I`
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncludeDir {
    pub path: String,
    pub kind: IncludeKind,
}

/// A `-D`/`-U`. `-DFOO` yields value `"1"`; `-DFOO=` yields `""`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroOp {
    pub name: String,
    pub value: Option<String>,
    pub undef: bool,
}

/// Preprocessor-relevant flags, in command-line order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlagSet {
    pub include_dirs: Vec<IncludeDir>,
    pub macros: Vec<MacroOp>,
    pub force_includes: Vec<String>,
    pub std: Option<String>,
    /// `args[0]`, used to discover the compiler's built-in include paths.
    pub compiler: Option<String>,
}

impl FlagSet {
    /// Stable XXH3 over all fields; order-sensitive (flag order is semantic).
    pub fn hash(&self) -> u64 {
        let mut h = FieldHasher::new();
        h.u64(self.include_dirs.len() as u64);
        for d in &self.include_dirs {
            h.str(&d.path).u64(d.kind as u64);
        }
        h.u64(self.macros.len() as u64);
        for m in &self.macros {
            h.str(&m.name).u64(m.undef as u64);
            match &m.value {
                Some(v) => h.u64(1).str(v),
                None => h.u64(0),
            };
        }
        h.u64(self.force_includes.len() as u64);
        for f in &self.force_includes {
            h.str(f);
        }
        match &self.std {
            Some(s) => h.u64(1).str(s),
            None => h.u64(0),
        };
        match &self.compiler {
            Some(s) => h.u64(1).str(s),
            None => h.u64(0),
        };
        h.finish()
    }
}

fn normalize(path: String) -> String {
    if cfg!(windows) && path.contains('\\') { path.replace('\\', "/") } else { path }
}

pub(crate) fn absolutize(dir: &str, p: &str) -> String {
    if Path::new(p).is_absolute() || p.starts_with('/') {
        normalize(p.to_owned())
    } else {
        normalize(Path::new(dir).join(p).to_string_lossy().into_owned())
    }
}

/// Shell-style split; see the module docs for the (deliberately small) rules.
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
                in_arg = true;
            }
            ('"' | '\'', None) => {
                quote = Some(c);
                in_arg = true;
            }
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            (c, _) => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    args
}

fn is_msvc_driver(compiler: &str) -> bool {
    let name = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler).to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == "cl" || stem == "clang-cl"
}

/// `(value, args consumed)` if `arg` is `flag` (value in the next arg) or
/// `flagVALUE` (joined).
fn flag_value<'a>(arg: &'a str, flag: &str, next: Option<&'a String>) -> Option<(&'a str, usize)> {
    let rest = arg.strip_prefix(flag)?;
    if rest.is_empty() { next.map(|n| (n.as_str(), 2)) } else { Some((rest, 1)) }
}

fn macro_op(spec: &str, undef: bool) -> MacroOp {
    if undef {
        return MacroOp { name: spec.to_owned(), value: None, undef };
    }
    match spec.split_once('=') {
        Some((n, v)) => MacroOp { name: n.to_owned(), value: Some(v.to_owned()), undef },
        None => MacroOp { name: spec.to_owned(), value: Some("1".to_owned()), undef },
    }
}

const GCC_INCLUDES: &[(&str, IncludeKind)] = &[
    ("isystem", IncludeKind::System),
    ("iquote", IncludeKind::Quote),
    ("idirafter", IncludeKind::System),
    ("imsvc", IncludeKind::System),
];
const MSVC_INCLUDES: &[(&str, IncludeKind)] = &[("external:I", IncludeKind::System), ("imsvc", IncludeKind::System)];

/// Extracts flags from a full argv (`args[0]` is the compiler).
pub fn parse_args(args: &[String], directory: &str) -> FlagSet {
    let mut out = FlagSet { compiler: args.first().map(|c| normalize(c.clone())), ..FlagSet::default() };
    let msvc = args.first().is_some_and(|c| is_msvc_driver(c));
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        let next = args.get(i + 1);
        let mut consumed = 1;
        let slash = msvc && a.starts_with('/');
        if a.starts_with('-') || slash {
            let body = &a[1..];
            let special = if slash { MSVC_INCLUDES } else { GCC_INCLUDES };
            let special_hit = special
                .iter()
                .find_map(|&(flag, kind)| flag_value(body, flag, next).map(|(v, n)| (kind, v, n)));
            if let Some((kind, v, n)) = special_hit {
                out.include_dirs.push(IncludeDir { path: absolutize(directory, v), kind });
                consumed = n;
            } else if let Some((v, n)) = flag_value(body, "I", next) {
                out.include_dirs.push(IncludeDir { path: absolutize(directory, v), kind: IncludeKind::Normal });
                consumed = n;
            } else if let Some((v, n)) = flag_value(body, "D", next) {
                out.macros.push(macro_op(v, false));
                consumed = n;
            } else if let Some((v, n)) = flag_value(body, "U", next) {
                out.macros.push(macro_op(v, true));
                consumed = n;
            } else if !slash && body == "include" {
                if let Some(n) = next {
                    out.force_includes.push(absolutize(directory, n));
                    consumed = 2;
                }
            } else if slash && let Some((v, n)) = flag_value(body, "FI", next) {
                out.force_includes.push(absolutize(directory, v));
                consumed = n;
            } else if let Some(v) = a.strip_prefix("-std=").or_else(|| a.strip_prefix("/std:")) {
                out.std = Some(v.to_owned());
            } else if body == "o" || (!slash && matches!(body, "MF" | "MT" | "MQ" | "Xclang" | "x")) {
                consumed = 2; // flags whose value must not be mistaken for anything else
            }
        }
        i += consumed;
    }
    out
}

#[derive(Deserialize)]
struct RawEntry<'a> {
    #[serde(borrow)]
    directory: Cow<'a, str>,
    #[serde(borrow)]
    file: Cow<'a, str>,
    #[serde(borrow, default)]
    command: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    arguments: Option<Vec<Cow<'a, str>>>,
}

#[derive(Debug)]
pub enum CompileDbError {
    Load(LoadError),
    Json(serde_json::Error),
}

impl fmt::Display for CompileDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "invalid compile_commands.json: {e}"),
        }
    }
}

impl std::error::Error for CompileDbError {}

#[derive(Debug, Clone)]
pub struct CompileEntry {
    /// Absolute, `/`-separated.
    pub file: String,
    pub flags: FlagsId,
    pub flags_hash: u64,
}

#[derive(Default)]
pub struct CompileDb {
    pub entries: Vec<CompileEntry>,
    pub flag_sets: IndexVec<FlagsId, FlagSet>,
    flag_hashes: IndexVec<FlagsId, u64>,
    /// File-path hash -> entry index (first entry wins for duplicates).
    by_file: HashTable<u32>,
}

fn hash_path(p: &str) -> u64 {
    FxBuildHasher.hash_one(p)
}

impl CompileDb {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CompileDbError> {
        let src = SourceFile::load(path).map_err(CompileDbError::Load)?;
        Self::from_json(src.text())
    }

    /// Loads a generated `.ninja` build file (see [`crate::workspace::ninja`]).
    pub fn load_ninja(path: impl AsRef<Path>) -> Result<Self, CompileDbError> {
        let path = path.as_ref();
        let src = SourceFile::load(path).map_err(CompileDbError::Load)?;
        let abs = std::path::absolute(path).map_err(|e| CompileDbError::Load(LoadError::Io(e)))?;
        let dir = abs.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Self::from_parsed(crate::workspace::ninja::parse(src.text(), &dir)))
    }

    pub fn from_json(json: &str) -> Result<Self, CompileDbError> {
        let raw: Vec<RawEntry> = serde_json::from_str(json).map_err(CompileDbError::Json)?;

        // Parallel and lock-free: each entry is independent.
        let parsed: Vec<(String, std::sync::Arc<FlagSet>, u64)> = raw
            .par_iter()
            .map(|e| {
                let args = match (&e.arguments, &e.command) {
                    (Some(a), _) => a.iter().map(|s| s.to_string()).collect(),
                    (None, Some(c)) => split_command(c),
                    (None, None) => Vec::new(),
                };
                let flags = parse_args(&args, &e.directory);
                let hash = flags.hash();
                (absolutize(&e.directory, &e.file), std::sync::Arc::new(flags), hash)
            })
            .collect();
        Ok(Self::from_parsed(parsed))
    }

    /// Builds the database from `(absolute file, flags, flags hash)` triples.
    pub(crate) fn from_parsed(parsed: Vec<(String, std::sync::Arc<FlagSet>, u64)>) -> Self {
        // Sequential dedupe: one hash probe per entry. Entries sharing an Arc skip the deep compare.
        let mut db = Self::default();
        let mut ids: hashbrown::HashMap<u64, FlagsId, FxBuildHasher> = Default::default();
        // Keeping the Arc alive keeps its address unique.
        let mut by_ptr: hashbrown::HashMap<usize, (FlagsId, std::sync::Arc<FlagSet>), FxBuildHasher> = Default::default();
        db.entries.reserve(parsed.len());
        for (file, flags, hash) in parsed {
            let key = std::sync::Arc::as_ptr(&flags) as usize;
            let id = match by_ptr.get(&key) {
                Some(&(id, _)) => id,
                None => {
                    let id = match ids.get(&hash) {
                        Some(&id) if db.flag_sets[id] == *flags => id,
                        _ => {
                            let id = db.flag_sets.push((*flags).clone());
                            db.flag_hashes.push(hash);
                            ids.insert(hash, id);
                            id
                        }
                    };
                    by_ptr.insert(key, (id, flags));
                    id
                }
            };
            db.entries.push(CompileEntry { file, flags: id, flags_hash: hash });
        }
        db.build_file_index();
        db
    }

    fn build_file_index(&mut self) {
        let Self { entries, by_file, .. } = self;
        *by_file = HashTable::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            let h = hash_path(&e.file);
            if by_file.find(h, |&j| entries[j as usize].file == e.file).is_none() {
                by_file.insert_unique(h, i as u32, |&j| hash_path(&entries[j as usize].file));
            }
        }
    }

    /// Entry for an absolute, `/`-separated path.
    pub fn get(&self, file: &str) -> Option<&CompileEntry> {
        self.index_of(file).map(|i| &self.entries[i])
    }

    pub fn index_of(&self, file: &str) -> Option<usize> {
        self.by_file.find(hash_path(file), |&j| self.entries[j as usize].file == file).map(|&j| j as usize)
    }

    pub fn flags(&self, entry: &CompileEntry) -> &FlagSet {
        &self.flag_sets[entry.flags]
    }

    /// Appends each compiler's built-in include directories (as system dirs)
    /// to the flag sets that use it, so `<windows.h>`, `<vector>` etc. resolve.
    /// Runs the compiler once per distinct (compiler, std) pair, in parallel.
    /// Returns the number of directories discovered.
    pub fn add_system_includes(&mut self) -> usize {
        let mut keys: Vec<(String, Option<String>)> = self
            .flag_sets
            .iter()
            .filter_map(|f| f.compiler.clone().map(|c| (c, f.std.clone())))
            .collect();
        keys.sort();
        keys.dedup();
        let found: Vec<Vec<String>> = keys.par_iter().map(|(c, s)| crate::workspace::sysinc::discover(c, s.as_deref())).collect();
        let total = found.iter().map(Vec::len).sum();
        for f in self.flag_sets.iter_mut() {
            let Some(c) = &f.compiler else { continue };
            let Ok(i) = keys.binary_search(&(c.clone(), f.std.clone())) else { continue };
            for d in &found[i] {
                if !f.include_dirs.iter().any(|x| &x.path == d) {
                    f.include_dirs.push(IncludeDir { path: d.clone(), kind: IncludeKind::System });
                }
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::ids::Idx;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_commands() {
        assert_eq!(split_command("clang++ -c a.cpp"), ["clang++", "-c", "a.cpp"]);
        assert_eq!(split_command("  a   b  "), ["a", "b"]);
        assert_eq!(split_command(r#"cc -DNAME=\"x y\" -I"C:\My Dir\inc""#), ["cc", "-DNAME=\"x", "y\"", r"-IC:\My Dir\inc"]);
        assert_eq!(split_command(r#"cc "-DMSG=hello world" ''"#), ["cc", "-DMSG=hello world", ""]);
        assert_eq!(split_command(r"cc C:\src\a.cpp"), ["cc", r"C:\src\a.cpp"]);
        assert!(split_command("").is_empty());
    }

    #[test]
    fn gcc_style_flags() {
        let f = parse_args(
            &v(&[
                "g++", "-I", "inc", "-Iabs/x", "-isystem", "/sys", "-iquote", "q", "-DFOO", "-D", "BAR=2", "-DEMPTY=",
                "-UFOO", "-include", "pch.h", "-std=c++20", "-o", "-DNOT_A_DEFINE", "-c", "a.cpp",
            ]),
            "/proj",
        );
        let dirs: Vec<_> = f.include_dirs.iter().map(|d| (d.path.as_str(), d.kind)).collect();
        assert_eq!(
            dirs,
            [
                ("/proj/inc", IncludeKind::Normal),
                ("/proj/abs/x", IncludeKind::Normal),
                ("/sys", IncludeKind::System),
                ("/proj/q", IncludeKind::Quote)
            ]
        );
        let m: Vec<_> = f.macros.iter().map(|m| (m.name.as_str(), m.value.as_deref(), m.undef)).collect();
        assert_eq!(
            m,
            [("FOO", Some("1"), false), ("BAR", Some("2"), false), ("EMPTY", Some(""), false), ("FOO", None, true)]
        );
        assert_eq!(f.force_includes, ["/proj/pch.h"]);
        assert_eq!(f.std.as_deref(), Some("c++20"));
    }

    #[test]
    fn msvc_style_flags() {
        let f = parse_args(
            &v(&["C:/VS/cl.exe", "/I", "inc", "/DX=1", "/UY", "/FI", "pch.h", "/std:c++17", "/external:I", "ext", "/c", "a.cpp"]),
            "/p",
        );
        assert_eq!(f.include_dirs.len(), 2);
        assert_eq!(f.include_dirs[1].kind, IncludeKind::System);
        assert_eq!(f.macros.len(), 2);
        assert_eq!(f.force_includes, ["/p/pch.h"]);
        assert_eq!(f.std.as_deref(), Some("c++17"));
    }

    #[test]
    fn slash_args_ignored_for_gcc_drivers() {
        // A source path like /Dev/a.cpp must not parse as `/D ev/a.cpp`.
        let f = parse_args(&v(&["clang++", "-c", "/Dev/a.cpp", "/Ifoo"]), "/p");
        assert!(f.macros.is_empty() && f.include_dirs.is_empty());
    }

    #[test]
    fn missing_value_is_tolerated() {
        let f = parse_args(&v(&["cc", "-I"]), "/p");
        assert!(f.include_dirs.is_empty());
        let f = parse_args(&v(&["cc", "-include"]), "/p");
        assert!(f.force_includes.is_empty());
        assert_eq!(parse_args(&[], "/p"), FlagSet::default());
    }

    #[test]
    fn hash_is_order_and_content_sensitive() {
        let a = parse_args(&v(&["cc", "-Ia", "-Ib"]), "/p");
        let b = parse_args(&v(&["cc", "-Ib", "-Ia"]), "/p");
        let c = parse_args(&v(&["cc", "-Ia", "-Ib"]), "/p");
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash(), c.hash());
        let d1 = parse_args(&v(&["cc", "-DA=1"]), "/p");
        let d2 = parse_args(&v(&["cc", "-UA"]), "/p");
        assert_ne!(d1.hash(), d2.hash());
    }

    #[test]
    fn loads_json_and_dedupes_flag_sets() {
        let json = r#"[
          {"directory":"/p","file":"a.cpp","command":"clang++ -Iinc -DX -c a.cpp"},
          {"directory":"/p","file":"/p/sub/b.cpp","arguments":["clang++","-Iinc","-DX","-c","sub/b.cpp"]},
          {"directory":"/p","file":"c.cpp","command":"clang++ -Iother -c c.cpp"},
          {"directory":"/p","file":"a.cpp","command":"clang++ -DDUP -c a.cpp"},
          {"directory":"/p","file":"nocmd.cpp"}
        ]"#;
        let db = CompileDb::from_json(json).unwrap();
        assert_eq!(db.entries.len(), 5);
        assert_eq!(db.flag_sets.len(), 4, "a/b share; c, dup-a, nocmd differ");
        assert_eq!(db.entries[0].flags, db.entries[1].flags);
        let a = db.get("/p/a.cpp").unwrap();
        assert_eq!(db.flags(a).macros[0].name, "X", "first duplicate wins");
        assert_eq!(db.get("/p/sub/b.cpp").unwrap().flags_hash, a.flags_hash);
        assert!(db.get("/p/missing.cpp").is_none());
        assert!(db.flags(db.get("/p/nocmd.cpp").unwrap()).include_dirs.is_empty());
    }

    #[test]
    fn json_errors_and_empty() {
        assert!(matches!(CompileDb::from_json("not json"), Err(CompileDbError::Json(_))));
        assert!(matches!(CompileDb::from_json(r#"[{"file":"x"}]"#), Err(CompileDbError::Json(_))));
        assert_eq!(CompileDb::from_json("[]").unwrap().entries.len(), 0);
        assert!(matches!(CompileDb::load("no/such/compile_commands.json"), Err(CompileDbError::Load(_))));
    }

    #[test]
    fn json_escapes_in_command() {
        let json = r#"[{"directory":"/p","file":"a.cpp","command":"cc -DMSG=\\\"hi\\\" -c a.cpp"}]"#;
        let db = CompileDb::from_json(json).unwrap();
        assert_eq!(db.flag_sets[FlagsId::from_usize(0)].macros[0].value.as_deref(), Some("\"hi\""));
    }

    #[test]
    fn loads_from_disk_via_mmap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compile_commands.json");
        let json = crate::testgen::compile_commands_json(3000, 12);
        std::fs::write(&path, &json).unwrap();
        let db = CompileDb::load(&path).unwrap();
        assert_eq!(db.entries.len(), 3000);
        assert_eq!(db.flag_sets.len(), 12);
        assert!(db.get("/work/engine/src/file1234.cpp").is_some());
    }
}
