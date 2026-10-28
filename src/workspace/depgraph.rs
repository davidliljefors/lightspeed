//! Reverse dependency index: which translation units must be re-indexed when a
//! file changes.
//!
//! Inputs are the sorted path keys each TU depended on (see
//! [`PpOutput::deps`](crate::pp::PpOutput::deps)), so it is built from what the
//! preprocessor actually resolved rather than from any build system.
//!
//! Layout: paths are interned to dense ids; the reverse map is a CSR (`base`)
//! plus a small unsorted `delta` of recent additions. Removed edges are never
//! deleted in place; an entry is live only if the TU's current forward list
//! still contains the id, so replacing a TU costs O(new deps) and the index is
//! rebuilt (counting sort, no hashing) only once the delta grows.

use rustc_hash::FxBuildHasher;
use std::collections::HashMap;

/// Index of a translation unit (position in the compile database).
pub type TuId = u32;

const DELTA_LIMIT: usize = 1 << 16;

#[derive(Default)]
pub struct DepGraph {
    ids: HashMap<u64, u32, FxBuildHasher>,
    /// Per TU: sorted dense ids it depends on.
    fwd: Vec<Box<[u32]>>,
    /// CSR over dense ids: `base_tus[base_off[id]..base_off[id + 1]]`.
    base_off: Vec<u32>,
    base_tus: Vec<TuId>,
    delta: Vec<(u32, TuId)>,
    scratch: Vec<u32>,
}

impl DepGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the dependency set of `tu`. `keys` are path keys, any order.
    pub fn set_deps(&mut self, tu: TuId, keys: &[u64]) {
        if self.fwd.len() <= tu as usize {
            self.fwd.resize_with(tu as usize + 1, || Box::from([]));
        }
        let mut ids = std::mem::take(&mut self.scratch);
        ids.clear();
        for &k in keys {
            let next = self.ids.len() as u32;
            ids.push(*self.ids.entry(k).or_insert(next));
        }
        ids.sort_unstable();
        ids.dedup();
        let old = &self.fwd[tu as usize];
        for &id in &ids {
            if old.binary_search(&id).is_err() {
                self.delta.push((id, tu));
            }
        }
        self.fwd[tu as usize] = ids.as_slice().into();
        self.scratch = ids;
        if self.delta.len() > DELTA_LIMIT {
            self.compact();
        }
    }

    /// Drops `tu` (for example when it leaves the compile database).
    pub fn remove_tu(&mut self, tu: TuId) {
        if let Some(f) = self.fwd.get_mut(tu as usize) {
            *f = Box::from([]);
        }
    }

    /// Fills `out` with the sorted TUs that depend on the file with `key`.
    pub fn dependents(&self, key: u64, out: &mut Vec<TuId>) {
        out.clear();
        let Some(&id) = self.ids.get(&key) else { return };
        let live = |tu: TuId| self.fwd[tu as usize].binary_search(&id).is_ok();
        if (id as usize + 1) < self.base_off.len() {
            let r = self.base_off[id as usize] as usize..self.base_off[id as usize + 1] as usize;
            out.extend(self.base_tus[r].iter().copied().filter(|&t| live(t)));
        }
        out.extend(self.delta.iter().filter(|&&(i, t)| i == id && live(t)).map(|&(_, t)| t));
        out.sort_unstable();
        out.dedup();
    }

    /// Number of distinct files known to the graph.
    pub fn file_count(&self) -> usize {
        self.ids.len()
    }

    /// Rebuilds the CSR from the forward lists, folding in the delta.
    pub fn compact(&mut self) {
        let n = self.ids.len();
        let mut off = vec![0u32; n + 1];
        for f in &self.fwd {
            for &id in f.iter() {
                off[id as usize + 1] += 1;
            }
        }
        for i in 0..n {
            off[i + 1] += off[i];
        }
        let mut tus = vec![0; off[n] as usize];
        let mut cur = off.clone();
        for (tu, f) in self.fwd.iter().enumerate() {
            for &id in f.iter() {
                tus[cur[id as usize] as usize] = tu as TuId;
                cur[id as usize] += 1;
            }
        }
        self.base_off = off;
        self.base_tus = tus;
        self.delta.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(g: &DepGraph, k: u64) -> Vec<TuId> {
        let mut v = Vec::new();
        g.dependents(k, &mut v);
        v
    }

    #[test]
    fn replace_and_remove() {
        let mut g = DepGraph::new();
        g.set_deps(0, &[10, 11, 12]);
        g.set_deps(1, &[11, 12]);
        g.set_deps(2, &[12]);
        assert_eq!(deps(&g, 12), [0, 1, 2]);
        assert_eq!(deps(&g, 10), [0]);
        g.compact();
        assert_eq!(deps(&g, 12), [0, 1, 2]);

        g.set_deps(1, &[11, 13]);
        assert_eq!(deps(&g, 12), [0, 2]);
        assert_eq!(deps(&g, 13), [1]);
        g.remove_tu(0);
        assert_eq!(deps(&g, 11), [1]);
        assert_eq!(deps(&g, 999), Vec::<TuId>::new());

        // Re-adding a dropped edge must not duplicate it.
        g.set_deps(1, &[11]);
        g.set_deps(1, &[11, 13]);
        assert_eq!(deps(&g, 13), [1]);
    }

    #[test]
    fn survives_many_compactions() {
        let mut g = DepGraph::new();
        for round in 0..5u64 {
            for tu in 0..3000u32 {
                let keys: Vec<u64> = (0..40).map(|k| (tu as u64 * 7 + k * 13 + round * 3) % 5000).collect();
                g.set_deps(tu, &keys);
            }
        }
        let mut want = Vec::new();
        for tu in 0..3000u32 {
            if (0..40u64).any(|k| (tu as u64 * 7 + k * 13 + 12) % 5000 == 42) {
                want.push(tu);
            }
        }
        assert_eq!(deps(&g, 42), want);
    }
}
