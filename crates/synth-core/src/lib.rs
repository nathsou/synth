//! Foundational data structures shared by every `synth` crate.
//!
//! - [`id`]: typed `u32` indices and the arenas they index ([`IndexVec`]).
//! - [`hash`]: a deterministic fast hasher and map/set aliases.
//! - [`ordered`]: insertion-ordered maps and sets for deterministic iteration.
//! - [`intern`]: string interning.

pub mod hash;
pub mod id;
pub mod intern;
pub mod ordered;

pub use hash::{FxBuildHasher, FxHashMap, FxHashSet, FxHasher, fx_hash};
pub use id::{IdRange, Idx, IndexVec};
pub use intern::{Interner, Sym};
pub use ordered::{OrderedMap, OrderedSet};
