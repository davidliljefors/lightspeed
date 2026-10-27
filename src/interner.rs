//! String interner backed by one contiguous arena (no per-string allocation).

use crate::ids::Idx;
use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;
use std::hash::BuildHasher;
use std::marker::PhantomData;

pub struct Interner<I: Idx> {
    arena: String,
    /// End offset of each string; start is the previous end (or 0).
    ends: Vec<u32>,
    table: HashTable<I>,
    _marker: PhantomData<I>,
}

impl<I: Idx> Default for Interner<I> {
    fn default() -> Self {
        Self { arena: String::new(), ends: Vec::new(), table: HashTable::new(), _marker: PhantomData }
    }
}

#[inline]
fn slice<'a>(arena: &'a str, ends: &[u32], i: usize) -> &'a str {
    let start = if i == 0 { 0 } else { ends[i - 1] as usize };
    &arena[start..ends[i] as usize]
}

#[inline]
fn hash_str(s: &str) -> u64 {
    FxBuildHasher.hash_one(s)
}

impl<I: Idx> Interner<I> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Approximate heap bytes: arena, offsets and hash table.
    pub fn heap_bytes(&self) -> usize {
        self.arena.capacity() + self.ends.capacity() * 4 + self.table.capacity() * (std::mem::size_of::<I>() + 1)
    }

    pub fn with_capacity(strings: usize, bytes: usize) -> Self {
        Self {
            arena: String::with_capacity(bytes),
            ends: Vec::with_capacity(strings),
            table: HashTable::with_capacity(strings),
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn intern(&mut self, s: &str) -> I {
        self.intern_new(s).0
    }

    /// Returns the ID and whether the string was newly inserted.
    pub fn intern_new(&mut self, s: &str) -> (I, bool) {
        let hash = hash_str(s);
        let Self { arena, ends, table, .. } = self;
        if let Some(&id) = table.find(hash, |&id| slice(arena, ends, id.index()) == s) {
            return (id, false);
        }
        assert!(arena.len() + s.len() <= u32::MAX as usize, "interner arena exceeds 4 GiB");
        let id = I::from_usize(ends.len());
        arena.push_str(s);
        ends.push(arena.len() as u32);
        table.insert_unique(hash, id, |&id| hash_str(slice(arena, ends, id.index())));
        (id, true)
    }

    #[inline]
    pub fn lookup(&self, s: &str) -> Option<I> {
        self.table
            .find(hash_str(s), |&id| slice(&self.arena, &self.ends, id.index()) == s)
            .copied()
    }

    #[inline]
    pub fn get(&self, id: I) -> &str {
        slice(&self.arena, &self.ends, id.index())
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn arena_bytes(&self) -> usize {
        self.arena.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (I, &str)> {
        (0..self.ends.len()).map(|i| (I::from_usize(i), slice(&self.arena, &self.ends, i)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::define_id!(S);

    #[test]
    fn dedups_and_resolves() {
        let mut i: Interner<S> = Interner::new();
        let a = i.intern("hello");
        let b = i.intern("world");
        let a2 = i.intern("hello");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(i.get(a), "hello");
        assert_eq!(i.get(b), "world");
        assert_eq!(i.len(), 2);
        assert_eq!(i.intern_new("world"), (b, false));
        assert!(i.intern_new("new").1);
    }

    #[test]
    fn empty_string_and_lookup() {
        let mut i: Interner<S> = Interner::new();
        let e = i.intern("");
        assert_eq!(i.get(e), "");
        assert_eq!(i.lookup(""), Some(e));
        assert_eq!(i.lookup("nope"), None);
        assert_eq!(i.intern("x").index(), 1);
    }

    #[test]
    fn survives_many_rehashes() {
        let mut i: Interner<S> = Interner::new();
        let ids: Vec<S> = (0..20_000).map(|n| i.intern(&format!("sym_{n}"))).collect();
        for (n, id) in ids.iter().enumerate() {
            assert_eq!(i.get(*id), format!("sym_{n}"));
            assert_eq!(i.lookup(&format!("sym_{n}")), Some(*id));
        }
        assert_eq!(i.len(), 20_000);
        assert_eq!(i.iter().count(), 20_000);
    }

    #[test]
    fn unicode() {
        let mut i: Interner<S> = Interner::new();
        let a = i.intern("größe");
        assert_eq!(i.get(a), "größe");
    }
}
