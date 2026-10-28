#![allow(dead_code)]

use lightspeed::workspace::compile_commands::CompileDb;
use lightspeed::workspace::index::{Index, IndexConfig};
use lightspeed::symbols::SymbolId;
use std::collections::HashMap;

/// Everything the indexes and hierarchy answer, keyed by names and source
/// offsets (not IDs), so an incrementally patched index can be compared with a
/// fresh one. `path_depth` is how many trailing path components identify a file.
pub fn observable(idx: &Index, path_depth: usize) -> Vec<String> {
    let label = |id: SymbolId| {
        let s = idx.db.symbol(id);
        let path = idx.db.file_path(id);
        let parts: Vec<&str> = path.rsplit('/').take(path_depth).collect();
        let file = parts.into_iter().rev().collect::<Vec<_>>().join("/");
        format!("{}@{file}:{}", idx.db.qualified_name(id), s.name_offset)
    };
    let sorted = |ids: &[SymbolId]| {
        let mut v: Vec<String> = ids.iter().map(|&i| label(i)).collect();
        v.sort();
        v.join(",")
    };
    let mut same_name_memo: HashMap<_, String> = HashMap::new();
    let mut out = Vec::new();
    for (id, s) in idx.db.symbols().iter_enumerated() {
        if idx.db.is_dead(id) {
            continue;
        }
        let name = idx.db.name(id);
        let same_name = same_name_memo.entry(name).or_insert_with(|| sorted(idx.db.lookup(name))).clone();
        let children = sorted(idx.db.children(id));
        let canon = label(idx.hier.canonical(id));
        let fam = sorted(idx.hier.family_members(&idx.db, id));
        let defs = sorted(idx.hier.out_of_line_defs(id));
        let (mut ab, mut ad) = (Vec::new(), Vec::new());
        idx.hier.all_bases(id, &mut ab);
        idx.hier.all_derived(id, &mut ad);
        let bases = sorted(idx.hier.bases(id));
        let derived = sorted(idx.hier.derived(id));
        let (ab, ad) = (sorted(&ab), sorted(&ad));
        out.push(format!("{} {:?} | named [{same_name}] | children [{children}] | canonical {canon} | family [{fam}] | defs [{defs}] | bases [{bases}] derived [{derived}] all [{ab}] [{ad}] | {}", label(id), s.kind, s.family.is_some()));
    }
    out.sort();
    out
}

/// Small deterministic generator, so a failing seed reproduces.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Writes a one-entry `compile_commands.json` for `a.cpp` in `root`.
pub fn write_cdb(root: &str, include_root: bool) {
    let mut args = vec!["clang++", "-c", "a.cpp"];
    if include_root {
        args.extend(["-I", root]);
    }
    let cdb = serde_json::json!([{"directory": root, "file": "a.cpp", "arguments": args}]);
    std::fs::write(format!("{root}/compile_commands.json"), serde_json::to_string(&cdb).unwrap()).unwrap();
}

/// A temporary source tree that can be indexed through a generated `compile_commands.json`.
pub struct Ws {
    pub dir: tempfile::TempDir,
    pub root: String,
}

impl Ws {
    pub fn new(files: &[(&str, &str)], _tus: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().replace('\\', "/");
        let ws = Self { dir, root };
        for (p, t) in files {
            ws.write(p, t);
        }
        ws
    }

    pub fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    pub fn path(&self, rel: &str) -> String {
        format!("{}/{rel}", self.root)
    }

    pub fn index(&self, tus: &[&str]) -> Index {
        let entries: Vec<String> = tus
            .iter()
            .map(|f| format!(r#"{{"directory":"{}","file":"{f}","arguments":["clang++","-c","{f}","-I","{}"]}}"#, self.root, self.root))
            .collect();
        let cdb = CompileDb::from_json(&format!("[{}]", entries.join(","))).unwrap();
        Index::build(cdb, IndexConfig::default())
    }
}

/// Strips the `$` marker and returns the text with its byte offset.
pub fn cursor(src: &str) -> (String, u32) {
    let at = src.find('$').expect("marker");
    (src.replacen('$', "", 1), at as u32)
}

pub fn names(idx: &Index, ids: &[SymbolId]) -> Vec<String> {
    ids.iter().map(|&i| idx.db.qualified_name(i)).collect()
}
