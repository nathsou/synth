//! Foundational data structures shared by every `synth` crate.
//!
//! - [`id`]: typed `u32` indices and the arenas they index ([`IndexVec`]).
//! - [`hash`]: a deterministic fast hasher and map/set aliases.
//! - [`ordered`]: insertion-ordered maps and sets for deterministic iteration.
//! - [`intern`]: string interning.
//! - [`bin`]: a compact binary serialization format for databases.
//! - [`rng`]: a deterministic pseudo-random number generator.
//! - [`logic`]: four-state bit vectors with Verilog semantics.
//! - [`diag`]: source maps, spans and diagnostics.

pub mod bin;
pub mod diag;
pub mod hash;
pub mod id;
pub mod intern;
pub mod logic;
pub mod ordered;
pub mod rng;

pub use diag::{Diagnostic, Diagnostics, FileId, Severity, SourceMap, Span};
pub use hash::{FxBuildHasher, FxHashMap, FxHashSet, FxHasher, fx_hash};
pub use id::{IdRange, Idx, IndexVec};
pub use intern::{Interner, Sym};
pub use logic::{Logic, LogicVec};
pub use ordered::{OrderedMap, OrderedSet};
pub use rng::Rng;
