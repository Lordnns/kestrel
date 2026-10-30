//! The "Data Processing -- Scalar Floating-Point and Advanced SIMD" group
//! (bits 28..25 = x111): scalar floating point lives in `fp.rs`, everything
//! else (NEON vector and scalar forms) in `neon/`. Crypto encodings surface
//! as `Exit::Undefined`.

use super::bits::*;
use super::{fp, neon, Ctx, Step};

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    // Scalar floating point: bit 30 clear, bits 28..25 = 0b1111.
    if !bit(i, 30) && field(i, 28, 25) == 0b1111 {
        return fp::exec(c);
    }
    neon::exec(c)
}
