#![allow(dead_code)]

use lightspeed::index::Index;
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
