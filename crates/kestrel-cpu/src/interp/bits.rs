//! Bit-manipulation helpers shared by the decoders.

use crate::Flags;

/// Extract bits [hi:lo] of `v`.
#[inline(always)]
pub fn field(v: u32, hi: u32, lo: u32) -> u32 {
    (v >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}

#[inline(always)]
pub fn bit(v: u32, n: u32) -> bool {
    (v >> n) & 1 != 0
}

/// Sign-extend the low `bits` bits of `v` to 64 bits.
#[inline(always)]
pub fn sext(v: u64, bits: u32) -> i64 {
    let s = 64 - bits;
    ((v << s) as i64) >> s
}

/// All-ones mask of `n` bits (n may be 64).
#[inline(always)]
pub fn ones(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Mask a value to the operation width (32 or 64).
#[inline(always)]
pub fn trunc(v: u64, sf: bool) -> u64 {
    if sf {
        v
    } else {
        v & 0xffff_ffff
    }
}

/// Rotate right within `width` bits.
#[inline(always)]
pub fn ror(v: u64, amount: u32, width: u32) -> u64 {
    let a = amount % width;
    if a == 0 {
        return v & ones(width);
    }
    ((v >> a) | (v << (width - a))) & ones(width)
}

/// Shift used by shifted-register forms. `kind`: 0 LSL, 1 LSR, 2 ASR, 3 ROR.
#[inline(always)]
pub fn shift(v: u64, kind: u32, amount: u32, sf: bool) -> u64 {
    let width = if sf { 64 } else { 32 };
    let v = trunc(v, sf);
    let amount = amount % width;
    match kind {
        0 => trunc(v << amount, sf),
        1 => v >> amount,
        2 => {
            if sf {
                ((v as i64) >> amount) as u64
            } else {
                (((v as u32 as i32) >> amount) as u32) as u64
            }
        }
        _ => ror(v, amount, width),
    }
}

/// Register extension for extended-register forms (UXTB..SXTX), then shift.
#[inline(always)]
pub fn extend(v: u64, option: u32, shift: u32) -> u64 {
    let e = match option {
        0 => v & 0xff,
        1 => v & 0xffff,
        2 => v & 0xffff_ffff,
        3 => v,
        4 => sext(v & 0xff, 8) as u64,
        5 => sext(v & 0xffff, 16) as u64,
        6 => sext(v & 0xffff_ffff, 32) as u64,
        _ => v,
    };
    e << shift
}

/// AddWithCarry from the architecture: returns (result, flags), truncated to the width.
#[inline(always)]
pub fn add_with_carry(x: u64, y: u64, carry: bool, sf: bool) -> (u64, Flags) {
    let c = carry as u64;
    if sf {
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(c);
        let signed = (x as i64 as i128) + (y as i64 as i128) + c as i128;
        let flags = Flags {
            n: (r as i64) < 0,
            z: r == 0,
            c: c1 || c2,
            v: (r as i64 as i128) != signed,
        };
        (r, flags)
    } else {
        let (x, y) = (x as u32, y as u32);
        let unsigned = x as u64 + y as u64 + c;
        let r = unsigned as u32;
        let signed = x as i32 as i64 + y as i32 as i64 + c as i64;
        let flags = Flags {
            n: (r as i32) < 0,
            z: r == 0,
            c: unsigned >> 32 != 0,
            v: r as i32 as i64 != signed,
        };
        (r as u64, flags)
    }
}

/// Flags for logical ops with S=1 (ANDS/BICS/TST): C and V are cleared.
#[inline(always)]
pub fn logic_flags(r: u64, sf: bool) -> Flags {
    let neg = if sf {
        (r as i64) < 0
    } else {
        (r as u32 as i32) < 0
    };
    Flags {
        n: neg,
        z: trunc(r, sf) == 0,
        c: false,
        v: false,
    }
}

/// Replicate an `esize`-bit element across 64 bits.
#[inline(always)]
fn replicate(v: u64, esize: u32) -> u64 {
    let mut out = 0u64;
    let mut i = 0;
    while i < 64 {
        out |= v << i;
        i += esize;
    }
    out
}

/// DecodeBitMasks from the architecture reference: returns (wmask, tmask)
/// for logical-immediate and bitfield instructions, or None if reserved.
pub fn decode_bit_masks(
    n: bool,
    imms: u32,
    immr: u32,
    immediate: bool,
    width: u32,
) -> Option<(u64, u64)> {
    let combined = ((n as u32) << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let esize = 1u32 << len;
    if esize > width {
        return None;
    }
    let diff = s.wrapping_sub(r) & levels;
    let welem = ones(s + 1);
    let telem = ones(diff + 1);
    let wmask = replicate(ror(welem, r, esize), esize) & ones(width);
    let tmask = replicate(telem, esize) & ones(width);
    Some((wmask, tmask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmask_immediates() {
        // and x0, x0, #0xff  => N=1 immr=0 imms=7
        assert_eq!(decode_bit_masks(true, 7, 0, true, 64).unwrap().0, 0xff);
        // orr w0, wzr, #0x55555555 => N=0 immr=0 imms=0b111100
        assert_eq!(
            decode_bit_masks(false, 0b111100, 0, true, 32).unwrap().0,
            0x5555_5555
        );
        // and x0, x0, #0xfffffffffffffff0 => N=1 immr=60 imms=59
        assert_eq!(
            decode_bit_masks(true, 59, 60, true, 64).unwrap().0,
            0xffff_ffff_ffff_fff0
        );
        // all-ones is reserved for logical immediates
        assert!(decode_bit_masks(true, 63, 0, true, 64).is_none());
    }

    #[test]
    fn carry_and_overflow() {
        let (r, f) = add_with_carry(u64::MAX, 1, false, true);
        assert_eq!(r, 0);
        assert!(f.z && f.c && !f.v);
        let (_, f) = add_with_carry(i64::MAX as u64, 1, false, true);
        assert!(f.v && f.n);
        // 32-bit subtract 1 - 2 via x + !y + 1
        let (r, f) = add_with_carry(1, !2u64 & 0xffff_ffff, true, false);
        assert_eq!(r, 0xffff_ffff);
        assert!(f.n && !f.c);
    }
}
