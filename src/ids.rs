//! Typed 32-bit indices and typed vectors.
//!
//! IDs wrap `NonZeroU32` (storing `index + 1`), so `Option<Id>` is 4 bytes.

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// A typed index into an [`IndexVec`] or [`Interner`](crate::interner::Interner).
pub trait Idx: Copy + Eq + Ord + Hash + fmt::Debug {
    /// Panics if `i` does not fit (`i >= u32::MAX`).
    fn from_usize(i: usize) -> Self;
    fn index(self) -> usize;
}

/// Defines a `NonZeroU32`-backed typed ID implementing [`Idx`].
#[macro_export]
macro_rules! define_id {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        $vis struct $name(::std::num::NonZeroU32);

        impl $crate::ids::Idx for $name {
            #[inline]
            fn from_usize(i: usize) -> Self {
                let raw = u32::try_from(i)
                    .ok()
                    .and_then(|v| v.checked_add(1))
                    .and_then(::std::num::NonZeroU32::new)
                    .expect(concat!(stringify!($name), " overflow"));
                Self(raw)
            }

            #[inline]
            fn index(self) -> usize {
                self.0.get() as usize - 1
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0.get() - 1)
            }
        }
    };
}

/// A `Vec<T>` indexed by a typed ID instead of `usize`.
pub struct IndexVec<I, T> {
    raw: Vec<T>,
    _marker: PhantomData<fn(&I)>,
}

impl<I, T> Default for IndexVec<I, T> {
    fn default() -> Self {
        Self { raw: Vec::new(), _marker: PhantomData }
    }
}

impl<I, T: Clone> Clone for IndexVec<I, T> {
    fn clone(&self) -> Self {
        Self { raw: self.raw.clone(), _marker: PhantomData }
    }
}

impl<I, T: fmt::Debug> fmt::Debug for IndexVec<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.raw.fmt(f)
    }
}

impl<I: Idx, T> IndexVec<I, T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { raw: Vec::with_capacity(cap), _marker: PhantomData }
    }

    #[inline]
    pub fn push(&mut self, value: T) -> I {
        let id = I::from_usize(self.raw.len());
        self.raw.push(value);
        id
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    #[inline]
    pub fn get(&self, id: I) -> Option<&T> {
        self.raw.get(id.index())
    }

    #[inline]
    pub fn get_mut(&mut self, id: I) -> Option<&mut T> {
        self.raw.get_mut(id.index())
    }

    pub fn as_slice(&self) -> &[T] {
        &self.raw
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.raw.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.raw.iter_mut()
    }

    pub fn iter_enumerated(&self) -> impl ExactSizeIterator<Item = (I, &T)> + Clone {
        self.raw.iter().enumerate().map(|(i, t)| (I::from_usize(i), t))
    }

    pub fn ids(&self) -> impl ExactSizeIterator<Item = I> + Clone {
        (0..self.raw.len()).map(I::from_usize)
    }

    pub fn clear(&mut self) {
        self.raw.clear();
    }

    pub fn reserve(&mut self, additional: usize) {
        self.raw.reserve(additional);
    }
}

impl<I: Idx, T> Index<I> for IndexVec<I, T> {
    type Output = T;
    #[inline]
    fn index(&self, id: I) -> &T {
        &self.raw[id.index()]
    }
}

impl<I: Idx, T> IndexMut<I> for IndexVec<I, T> {
    #[inline]
    fn index_mut(&mut self, id: I) -> &mut T {
        &mut self.raw[id.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    define_id!(TestId);

    #[test]
    fn option_id_is_four_bytes() {
        assert_eq!(size_of::<Option<TestId>>(), 4);
        assert_eq!(size_of::<TestId>(), 4);
    }

    #[test]
    fn roundtrip_and_debug() {
        let id = TestId::from_usize(41);
        assert_eq!(id.index(), 41);
        assert_eq!(format!("{id:?}"), "TestId(41)");
        assert_eq!(TestId::from_usize(0).index(), 0);
    }

    #[test]
    #[should_panic(expected = "overflow")]
    fn overflow_panics() {
        let _ = TestId::from_usize(u32::MAX as usize);
    }

    #[test]
    fn index_vec_basics() {
        let mut v: IndexVec<TestId, &str> = IndexVec::new();
        let a = v.push("a");
        let b = v.push("b");
        assert_eq!((v[a], v[b], v.len()), ("a", "b", 2));
        v[a] = "z";
        assert_eq!(v.get(a), Some(&"z"));
        assert_eq!(v.get(TestId::from_usize(9)), None);
        let all: Vec<_> = v.iter_enumerated().map(|(i, s)| (i.index(), *s)).collect();
        assert_eq!(all, [(0, "z"), (1, "b")]);
        assert_eq!(v.ids().count(), 2);
    }
}
