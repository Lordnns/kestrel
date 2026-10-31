//! Advanced SIMD (NEON), vector and scalar forms.
//!
//! Decoding follows the Advanced SIMD encoding groups of the Arm ARM. Every
//! operation goes through one set of lane helpers that work for all element
//! sizes; results are written with [`write`], which clears the bits above
//! the operation's width (the 64-bit vector forms and every scalar form
//! clear the top of the register). Saturating operations set FPSR.QC.
//!
//! - `int.rs`: integer arithmetic, compares, shifts, widening and narrowing
//! - `vfp.rs`: floating-point lanes, calling [`crate::float`] per element
//! - `perm.rs`: copies, permutes, table lookup, logic and immediates
//! - `mem.rs`: structure loads and stores (LD1-LD4, ST1-ST4)
//! - `crypto.rs`: AES, SHA-1 and SHA-256

mod crypto;
mod int;
mod mem;
mod perm;
mod vfp;

pub use mem::exec as load_store;

use super::bits::*;
use super::{Ctx, Step};

/// Every Advanced SIMD data-processing encoding (bits 28..25 = x111) that is
/// not scalar floating point. Kept out of line so the integer fast path
/// stays compact.
#[inline(never)]
pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    let scalar = bit(i, 28);
    // Vector forms have bit 31 clear; scalar forms are 0b01 in bits 31..30.
    if (scalar && field(i, 31, 30) != 0b01) || (!scalar && bit(i, 31)) {
        return Err(c.undef());
    }
    if !bit(i, 24) {
        if bit(i, 21) {
            if bit(i, 10) {
                return int::three_same(c, scalar);
            }
            return match (field(i, 11, 10), field(i, 20, 17)) {
                (0b10, 0b0000) => int::misc(c, scalar),
                (0b10, 0b1000) if scalar => int::pairwise_scalar(c),
                (0b10, 0b1000) => int::across(c),
                // Cryptographic AES (vector space) and two-register SHA
                // (scalar space).
                (0b10, 0b0100) if scalar => crypto::sha_two(c),
                (0b10, 0b0100) => crypto::aes(c),
                // Two-register misc, half precision.
                (0b10, 0b1100) => vfp::misc_half(c, scalar),
                (0b00, _) => int::three_different(c, scalar),
                _ => Err(c.undef()),
            };
        }
        if field(i, 23, 21) == 0 && !bit(i, 15) && bit(i, 10) {
            return perm::copy(c, scalar);
        }
        if scalar && field(i, 23, 21) == 0 && !bit(i, 15) && field(i, 11, 10) == 0 {
            // Cryptographic three-register SHA.
            return crypto::sha_three(c);
        }
        if bit(i, 15) && bit(i, 10) {
            return int::three_same_extra(c, scalar);
        }
        if bit(i, 22) && field(i, 15, 14) == 0 && bit(i, 10) {
            return vfp::three_same_half(c, scalar);
        }
        if !scalar && !bit(i, 15) && !bit(i, 10) {
            return if bit(i, 29) {
                perm::ext(c)
            } else if bit(i, 11) {
                perm::permute(c)
            } else {
                perm::table(c)
            };
        }
        return Err(c.undef());
    }
    if !bit(i, 10) {
        return int::by_element(c, scalar);
    }
    if bit(i, 23) {
        return Err(c.undef());
    }
    if field(i, 22, 19) == 0 {
        return if scalar {
            Err(c.undef())
        } else {
            perm::modified_imm(c)
        };
    }
    int::shift_imm(c, scalar)
}

// ---- lane helpers ----------------------------------------------------------

/// Element `idx` of `es` bits.
#[inline]
fn get(v: u128, es: u32, idx: u32) -> u64 {
    (v >> (es * idx)) as u64 & ones(es)
}

/// Replace element `idx` of `es` bits.
#[inline]
fn put(v: &mut u128, es: u32, idx: u32, x: u64) {
    let sh = es * idx;
    let m = (ones(es) as u128) << sh;
    *v = (*v & !m) | (((x & ones(es)) as u128) << sh);
}

/// The value of an element as a signed or unsigned integer.
#[inline]
fn ext(x: u64, es: u32, unsigned: bool) -> i128 {
    if unsigned {
        (x & ones(es)) as i128
    } else {
        sext(x, es) as i128
    }
}

/// Saturate to an `es`-bit signed or unsigned integer; true if it clipped.
fn sat(x: i128, es: u32, unsigned: bool) -> (u64, bool) {
    let (lo, hi) = if unsigned {
        (0, (1i128 << es) - 1)
    } else {
        (-(1i128 << (es - 1)), (1i128 << (es - 1)) - 1)
    };
    let (v, clipped) = if x > hi {
        (hi, true)
    } else if x < lo {
        (lo, true)
    } else {
        (x, false)
    };
    (v as u64 & ones(es), clipped)
}

/// All ones in an `es`-bit element when `b`.
#[inline]
fn mask_if(b: bool, es: u32) -> u64 {
    if b {
        ones(es)
    } else {
        0
    }
}

/// The low `bits` bits of a register.
#[inline]
fn width_mask(bits: u32) -> u128 {
    if bits >= 128 {
        u128::MAX
    } else {
        (1u128 << bits) - 1
    }
}

#[inline]
fn vreg(c: &Ctx, n: u32) -> u128 {
    c.s.v[n as usize & 31]
}

/// Write the low `bits` bits of `v` to Vd and clear the rest.
#[inline]
fn write(c: &mut Ctx, d: u32, v: u128, bits: u32) {
    c.s.v[d as usize] = v & width_mask(bits);
}

/// Write a 64-bit narrowing result: the low half for the base form (upper
/// half cleared), the high half for the "2" form (low half kept).
fn write_narrow(c: &mut Ctx, d: u32, v: u64, upper: bool) {
    let r = &mut c.s.v[d as usize];
    *r = if upper {
        (*r & u64::MAX as u128) | ((v as u128) << 64)
    } else {
        v as u128
    };
}

/// Set FPSR.QC (bit 27) if any lane saturated.
#[inline]
fn set_qc(c: &mut Ctx, saturated: bool) {
    if saturated {
        c.s.fpsr |= 1 << 27;
    }
}

/// Register fields.
#[inline]
fn rd(i: u32) -> u32 {
    field(i, 4, 0)
}
#[inline]
fn rn(i: u32) -> u32 {
    field(i, 9, 5)
}
#[inline]
fn rm(i: u32) -> u32 {
    field(i, 20, 16)
}

/// Width in bits of a vector (Q) or scalar operation on `es`-bit elements.
#[inline]
fn datasize(q: bool, scalar: bool, es: u32) -> u32 {
    if scalar {
        es
    } else if q {
        128
    } else {
        64
    }
}

/// `x` shifted left by `sh` (right when negative, rounding if asked), in
/// exact arithmetic. A left shift by `es` or more returns a value that
/// truncates to zero and saturates correctly.
fn shifted(x: i128, sh: i32, es: u32, round: bool) -> i128 {
    if sh >= 0 {
        if sh as u32 >= es {
            x.signum() << 100
        } else {
            x << sh
        }
    } else {
        let amt = sh.unsigned_abs().min(120);
        if round {
            (x + (1 << (amt - 1))) >> amt
        } else {
            x >> amt
        }
    }
}
