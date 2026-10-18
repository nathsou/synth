//! Insertion-ordered map and set.
//!
//! Iteration follows insertion order, independent of hash values, so any
//! output derived from iterating one of these is deterministic.

use std::borrow::Borrow;
use std::fmt;
use std::hash::Hash;

use crate::hash::FxHashMap;

/// A hash map that iterates in insertion order.
///
/// Removal ([`swap_remove`](Self::swap_remove)) moves the last entry into the
/// removed slot, which is O(1) but perturbs order; use it only where that is
/// acceptable.
#[derive(Clone)]
pub struct OrderedMap<K, V> {
    entries: Vec<(K, V)>,
    index: FxHashMap<K, u32>,
}

impl<K: Hash + Eq + Clone, V> OrderedMap<K, V> {
    pub fn new() -> Self {
        Self { entries: Vec::new(), index: FxHashMap::default() }
    }

    pub fn with_capacity(n: usize) -> Self {
        let mut index = FxHashMap::default();
        index.reserve(n);
        Self { entries: Vec::with_capacity(n), index }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Inserts or replaces a value. Returns the entry's position and the
    /// previous value, if any. Replacing keeps the original position.
    pub fn insert_full(&mut self, key: K, value: V) -> (usize, Option<V>) {
        if let Some(&i) = self.index.get(&key) {
            let old = std::mem::replace(&mut self.entries[i as usize].1, value);
            (i as usize, Some(old))
        } else {
            let i = self.entries.len();
            self.index.insert(key.clone(), u32::try_from(i).expect("map too large"));
            self.entries.push((key, value));
            (i, None)
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.insert_full(key, value).1
    }

    /// Returns the position of `key`, inserting `make()` first if absent.
    pub fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> (usize, &mut V) {
        let i = match self.index.get(&key) {
            Some(&i) => i as usize,
            None => {
                let i = self.entries.len();
                self.index.insert(key.clone(), u32::try_from(i).expect("map too large"));
                self.entries.push((key, make()));
                i
            }
        };
        (i, &mut self.entries[i].1)
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.index.get(key).map(|&i| &self.entries[i as usize].1)
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.index.get(key).map(|&i| &mut self.entries[i as usize].1)
    }

    pub fn get_index_of<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.index.get(key).map(|&i| i as usize)
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.index.contains_key(key)
    }

    pub fn get_index(&self, i: usize) -> Option<(&K, &V)> {
        self.entries.get(i).map(|(k, v)| (k, v))
    }

    pub fn get_index_mut(&mut self, i: usize) -> Option<(&K, &mut V)> {
        self.entries.get_mut(i).map(|(k, v)| (&*k, v))
    }

    /// Removes `key` by moving the last entry into its slot.
    pub fn swap_remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.index.remove(key)? as usize;
        let (_, v) = self.entries.swap_remove(i);
        if let Some((moved, _)) = self.entries.get(i) {
            *self.index.get_mut::<K>(moved).unwrap() = i as u32;
        }
        Some(v)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&K, &V)> + ExactSizeIterator {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn iter_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = (&K, &mut V)> + ExactSizeIterator {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> + ExactSizeIterator {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + ExactSizeIterator {
        self.entries.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut V> + ExactSizeIterator {
        self.entries.iter_mut().map(|(_, v)| v)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }

    pub fn into_entries(self) -> Vec<(K, V)> {
        self.entries
    }
}

impl<K, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        Self { entries: Vec::new(), index: FxHashMap::default() }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for OrderedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.entries.iter().map(|(k, v)| (k, v))).finish()
    }
}

impl<K: Hash + Eq + Clone, V> FromIterator<(K, V)> for OrderedMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut m = Self::new();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

impl<K: Hash + Eq + Clone, V> std::ops::Index<usize> for OrderedMap<K, V> {
    type Output = V;
    fn index(&self, i: usize) -> &V {
        &self.entries[i].1
    }
}

/// A hash set that iterates in insertion order.
#[derive(Clone)]
pub struct OrderedSet<T> {
    map: OrderedMap<T, ()>,
}

impl<T: Hash + Eq + Clone> OrderedSet<T> {
    pub fn new() -> Self {
        Self { map: OrderedMap::new() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Inserts a value; returns its position and whether it was newly added.
    pub fn insert_full(&mut self, value: T) -> (usize, bool) {
        let (i, old) = self.map.insert_full(value, ());
        (i, old.is_none())
    }

    pub fn insert(&mut self, value: T) -> bool {
        self.insert_full(value).1
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(value)
    }

    pub fn get_index_of<Q>(&self, value: &Q) -> Option<usize>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_index_of(value)
    }

    pub fn get_index(&self, i: usize) -> Option<&T> {
        self.map.get_index(i).map(|(k, _)| k)
    }

    pub fn swap_remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.swap_remove(value).is_some()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.map.keys()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

impl<T> Default for OrderedSet<T> {
    fn default() -> Self {
        Self { map: OrderedMap::default() }
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.map.entries.iter().map(|(k, _)| k)).finish()
    }
}

impl<T: Hash + Eq + Clone> FromIterator<T> for OrderedSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut s = Self::new();
        for v in iter {
            s.insert(v);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_order_is_kept() {
        let mut m = OrderedMap::new();
        for k in [5, 1, 9, 3] {
            m.insert(k, k * 10);
        }
        assert_eq!(m.insert(1, 11), Some(10));
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), [5, 1, 9, 3]);
        assert_eq!(m.get(&1), Some(&11));
        assert_eq!(m[2], 90);
    }

    #[test]
    fn swap_remove_fixes_index() {
        let mut m: OrderedMap<&str, i32> = [("a", 1), ("b", 2), ("c", 3)].into_iter().collect();
        assert_eq!(m.swap_remove("a"), Some(1));
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), ["c", "b"]);
        assert_eq!(m.get("c"), Some(&3));
        assert_eq!(m.get_index_of("c"), Some(0));
        assert_eq!(m.swap_remove("zz"), None);
    }

    #[test]
    fn set_basics() {
        let mut s = OrderedSet::new();
        assert!(s.insert("x"));
        assert!(!s.insert("x"));
        assert!(s.insert("y"));
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), ["x", "y"]);
    }
}
