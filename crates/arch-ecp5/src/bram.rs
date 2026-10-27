//! Block RAM (`DP16KD`) mapping.

use synth_rtl::{MemId, Memory, Module};

/// Whether memory `mem` goes to block RAM.
pub fn fits(_mem: &Memory, _min_bits: u64) -> bool {
    false
}

/// Replaces memory `mi` with block RAMs; returns their number.
pub fn map(_m: &mut Module, _mi: MemId) -> Result<usize, String> {
    Ok(0)
}
