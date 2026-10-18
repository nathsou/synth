//! Typed `u32` indices and the `Vec`-backed arenas they index into.
//!
//! Every graph in the toolchain (AST, RTL, AIG, netlist, routing) is stored as
//! arenas addressed by small typed IDs instead of pointers. This keeps the data
//! compact, trivially `Send`, cheap to copy and deterministic to iterate.

use std::fmt;
use std::iter::FusedIterator;
use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// A dense index type usable as the key of an [`IndexVec`].
pub trait Idx: Copy + Eq + Ord + std::hash::Hash + fmt::Debug {
    /// Builds an index from a `usize`. Panics if it does not fit in `u32`.
    fn from_usize(i: usize) -> Self;
    /// Returns the index as a `usize`.
    fn index(self) -> usize;
}

/// Defines a `u32` newtype implementing [`Idx`].
///
/// ```
/// synth_core::define_id!(pub struct CellId);
/// let c = CellId::new(3);
/// assert_eq!(c.index(), 3);
/// ```
#[macro_export]
macro_rules! define_id {
    ($(#[$meta:meta])* $vis:vis struct $name:ident) => {
        $(#[$meta])*
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        $vis struct $name(u32);

        impl $name {
            /// Builds an ID from a `usize`. Panics if it does not fit in `u32`.
            #[inline]
            #[allow(dead_code)]
            $vis const fn new(i: usize) -> Self {
                assert!(i <= u32::MAX as usize, "index overflow");
                Self(i as u32)
            }

            /// Builds an ID from its raw `u32` representation.
            #[inline]
            #[allow(dead_code)]
            $vis const fn from_raw(raw: u32) -> Self {
                Self(raw)
            }

            /// Returns the raw `u32` representation.
            #[inline]
            #[allow(dead_code)]
            $vis const fn raw(self) -> u32 {
                self.0
            }

            /// Returns the ID as a `usize`.
            #[inline]
            #[allow(dead_code)]
            $vis const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl $crate::id::Idx for $name {
            #[inline]
            fn from_usize(i: usize) -> Self {
                Self::new(i)
            }
            #[inline]
            fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

/// A `Vec<T>` indexed by a typed ID `I`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IndexVec<I: Idx, T> {
    raw: Vec<T>,
    _marker: PhantomData<fn(&I)>,
}

impl<I: Idx, T> IndexVec<I, T> {
    /// Creates an empty arena.
    pub const fn new() -> Self {
        Self { raw: Vec::new(), _marker: PhantomData }
    }

    /// Creates an empty arena with room for `n` elements.
    pub fn with_capacity(n: usize) -> Self {
        Self { raw: Vec::with_capacity(n), _marker: PhantomData }
    }

    /// Creates an arena of `n` copies of `value`.
    pub fn from_elem(value: T, n: usize) -> Self
    where
        T: Clone,
    {
        Self { raw: vec![value; n], _marker: PhantomData }
    }

    /// Wraps an existing vector.
    pub fn from_raw(raw: Vec<T>) -> Self {
        Self { raw, _marker: PhantomData }
    }

    /// Appends an element and returns its ID.
    #[inline]
    pub fn push(&mut self, value: T) -> I {
        let id = I::from_usize(self.raw.len());
        self.raw.push(value);
        id
    }

    /// The ID the next [`push`](Self::push) will return.
    #[inline]
    pub fn next_id(&self) -> I {
        I::from_usize(self.raw.len())
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

    /// Iterates over the elements in ID order.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.raw.iter()
    }

    #[inline]
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.raw.iter_mut()
    }

    /// Iterates over `(id, &element)` pairs in ID order.
    pub fn iter_enumerated(&self) -> impl DoubleEndedIterator<Item = (I, &T)> + ExactSizeIterator {
        self.raw.iter().enumerate().map(|(i, t)| (I::from_usize(i), t))
    }

    /// Iterates over `(id, &mut element)` pairs in ID order.
    pub fn iter_enumerated_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = (I, &mut T)> + ExactSizeIterator {
        self.raw.iter_mut().enumerate().map(|(i, t)| (I::from_usize(i), t))
    }

    /// Iterates over all valid IDs.
    pub fn ids(&self) -> IdRange<I> {
        IdRange::new(0, self.raw.len())
    }

    /// Resizes to `n` elements, filling with clones of `value`.
    pub fn resize(&mut self, n: usize, value: T)
    where
        T: Clone,
    {
        self.raw.resize(n, value);
    }

    /// Borrows two distinct elements mutably.
    pub fn pair_mut(&mut self, a: I, b: I) -> (&mut T, &mut T) {
        let (a, b) = (a.index(), b.index());
        assert_ne!(a, b, "pair_mut on the same index");
        if a < b {
            let (lo, hi) = self.raw.split_at_mut(b);
            (&mut lo[a], &mut hi[0])
        } else {
            let (lo, hi) = self.raw.split_at_mut(a);
            (&mut hi[0], &mut lo[b])
        }
    }

    pub fn as_slice(&self) -> &[T] {
        &self.raw
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.raw
    }

    pub fn into_raw(self) -> Vec<T> {
        self.raw
    }

    pub fn clear(&mut self) {
        self.raw.clear();
    }
}

impl<I: Idx, T> Default for IndexVec<I, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Idx, T: fmt::Debug> fmt::Debug for IndexVec<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter_enumerated()).finish()
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

impl<I: Idx, T> FromIterator<T> for IndexVec<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(iter: It) -> Self {
        Self::from_raw(iter.into_iter().collect())
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a IndexVec<I, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.raw.iter()
    }
}

impl<I: Idx, T> IntoIterator for IndexVec<I, T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.raw.into_iter()
    }
}

/// A half-open range of IDs.
#[derive(Clone, Debug)]
pub struct IdRange<I: Idx> {
    start: usize,
    end: usize,
    _marker: PhantomData<fn() -> I>,
}

impl<I: Idx> IdRange<I> {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end, _marker: PhantomData }
    }
}

impl<I: Idx> Iterator for IdRange<I> {
    type Item = I;
    #[inline]
    fn next(&mut self) -> Option<I> {
        if self.start < self.end {
            let id = I::from_usize(self.start);
            self.start += 1;
            Some(id)
        } else {
            None
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end - self.start;
        (n, Some(n))
    }
}

impl<I: Idx> DoubleEndedIterator for IdRange<I> {
    fn next_back(&mut self) -> Option<I> {
        if self.start < self.end {
            self.end -= 1;
            Some(I::from_usize(self.end))
        } else {
            None
        }
    }
}

impl<I: Idx> ExactSizeIterator for IdRange<I> {}
impl<I: Idx> FusedIterator for IdRange<I> {}

#[cfg(test)]
mod tests {
    use super::*;

    define_id!(struct TestId);

    #[test]
    fn push_and_index() {
        let mut v: IndexVec<TestId, &str> = IndexVec::new();
        let a = v.push("a");
        let b = v.push("b");
        assert_eq!(a.index(), 0);
        assert_eq!(b.index(), 1);
        assert_eq!(v[b], "b");
        v[a] = "z";
        assert_eq!(v.iter().copied().collect::<Vec<_>>(), ["z", "b"]);
        assert_eq!(v.ids().collect::<Vec<_>>(), [a, b]);
        assert_eq!(format!("{a:?}"), "TestId(0)");
    }

    #[test]
    fn pair_mut_both_orders() {
        let mut v: IndexVec<TestId, i32> = (0..4).collect();
        let (x, y) = v.pair_mut(TestId::new(3), TestId::new(1));
        std::mem::swap(x, y);
        assert_eq!(v.as_slice(), &[0, 3, 2, 1]);
    }
}
