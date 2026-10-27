//! Compressed sparse row multimap: `K -> &[V]` in two flat vectors.
//!
//! Built with a stable counting sort (O(n), two allocations). Used for
//! name -> symbols, parent -> children, base -> derived, family -> members.

use crate::ids::Idx;
use std::marker::PhantomData;

pub struct Csr<K, V> {
    /// `offsets[k]..offsets[k + 1]` is the bucket of key `k`.
    offsets: Vec<u32>,
    items: Vec<V>,
    _marker: PhantomData<fn(&K)>,
}

impl<K, V> Default for Csr<K, V> {
    fn default() -> Self {
        Self { offsets: Vec::new(), items: Vec::new(), _marker: PhantomData }
    }
}

impl<K: Idx, V: Copy> Csr<K, V> {
    /// Builds from `(key, value)` pairs; `pairs` is iterated twice, so it
    /// should be cheap to clone. Order within a bucket follows input order.
    /// Every key must be `< num_keys`.
    pub fn build(num_keys: usize, pairs: impl Iterator<Item = (K, V)> + Clone) -> Self {
        let mut offsets = vec![0u32; num_keys + 2];
        let mut total = 0usize;
        for (k, _) in pairs.clone() {
            offsets[k.index() + 2] += 1;
            total += 1;
        }
        for i in 2..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        // Now offsets[k + 1] is the start of bucket k; placing items bumps it
        // to the end of bucket k == start of bucket k + 1.
        let mut items = match pairs.clone().next() {
            Some((_, first)) => vec![first; total],
            None => Vec::new(),
        };
        for (k, v) in pairs {
            let slot = &mut offsets[k.index() + 1];
            items[*slot as usize] = v;
            *slot += 1;
        }
        offsets.pop();
        Self { offsets, items, _marker: PhantomData }
    }

    /// Items for `key`; empty if the key is unknown or added after the build.
    #[inline]
    pub fn get(&self, key: K) -> &[V] {
        let k = key.index();
        match (self.offsets.get(k), self.offsets.get(k + 1)) {
            (Some(&s), Some(&e)) => &self.items[s as usize..e as usize],
            _ => &[],
        }
    }

    pub fn num_keys(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::define_id!(K);

    fn k(i: usize) -> K {
        K::from_usize(i)
    }

    #[test]
    fn groups_stably() {
        let pairs = [(k(2), 'a'), (k(0), 'b'), (k(2), 'c'), (k(0), 'd'), (k(2), 'e')];
        let csr = Csr::build(4, pairs.iter().copied());
        assert_eq!(csr.get(k(0)), ['b', 'd']);
        assert!(csr.get(k(1)).is_empty());
        assert_eq!(csr.get(k(2)), ['a', 'c', 'e']);
        assert!(csr.get(k(3)).is_empty());
        assert_eq!(csr.len(), 5);
        assert_eq!(csr.num_keys(), 4);
    }

    #[test]
    fn empty_and_out_of_range() {
        let csr: Csr<K, u32> = Csr::build(3, std::iter::empty());
        assert!(csr.is_empty());
        assert!(csr.get(k(1)).is_empty());
        assert!(csr.get(k(99)).is_empty());
        let d: Csr<K, u32> = Csr::default();
        assert!(d.get(k(0)).is_empty());
    }

    #[test]
    fn matches_naive_grouping() {
        let mut state = 12345u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let pairs: Vec<(K, u64)> = (0..2000).map(|i| (k((next() % 50) as usize), i)).collect();
        let csr = Csr::build(50, pairs.iter().copied());
        for key in 0..50 {
            let expect: Vec<u64> =
                pairs.iter().filter(|(kk, _)| kk.index() == key).map(|(_, v)| *v).collect();
            assert_eq!(csr.get(k(key)), &expect[..], "key {key}");
        }
    }
}
