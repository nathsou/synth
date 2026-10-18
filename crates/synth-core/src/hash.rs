//! A fast, deterministic, non-cryptographic hasher (the "Fx" hash used by rustc)
//! and map/set aliases built on it.
//!
//! `std`'s default `RandomState` seeds every map differently, which would make
//! iteration order, and therefore our output, vary between runs. These aliases
//! never use random seeds. Code must still never let hash iteration order leak
//! into results; use [`OrderedMap`](crate::OrderedMap) when order matters.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The Fx hasher: one rotate, xor and multiply per word.
#[derive(Default, Clone, Copy, Debug)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.add_to_hash(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            self.add_to_hash(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64);
            bytes = &bytes[4..];
        }
        for &b in bytes {
            self.add_to_hash(b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// A `BuildHasher` producing [`FxHasher`]s.
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
/// A `HashMap` using [`FxHasher`].
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;
/// A `HashSet` using [`FxHasher`].
pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;

/// Hashes a single value with [`FxHasher`].
pub fn fx_hash<T: std::hash::Hash + ?Sized>(value: &T) -> u64 {
    let mut h = FxHasher::default();
    value.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED_U32: u64 = 0x5582aca867c703d8;

    #[test]
    fn stable_across_runs() {
        // Pin the hash so an accidental algorithm change is noticed: results
        // that depend on hash values (e.g. tie-breaking) would change with it.
        assert_eq!(fx_hash(&0x1234_5678u32), PINNED_U32);
        let mut m: FxHashMap<u32, u32> = FxHashMap::default();
        m.insert(1, 2);
        assert_eq!(m[&1], 2);
    }
}
