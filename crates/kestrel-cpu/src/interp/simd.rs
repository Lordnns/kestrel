//! SIMD and floating point.
//!
//! Scalar floating point lives in `fp.rs`. Of Advanced SIMD (NEON), only the
//! operations integer code commonly needs are here: register moves (UMOV,
//! INS, DUP), vector logic (AND/BIC/ORR/ORN/EOR/BSL/BIT/BIF) and MOVI/MVNI.
//! The rest of NEON surfaces as `Exit::Undefined` with the exact encoding so
//! it is easy to prioritise.

use super::bits::*;
use super::{fp, Ctx, Flow, Step};

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    // Scalar floating point: bit 30 clear, bits 28..25 = 0b1111.
    if !bit(i, 30) && field(i, 28, 25) == 0b1111 {
        return fp::exec(c);
    }
    // Advanced SIMD scalar: bits 31..30 = 0b01, bits 28..24 = 0b11110.
    if field(i, 31, 30) == 0b01 && field(i, 28, 24) == 0b11110 {
        return fp::scalar_simd(c);
    }
    // Advanced SIMD copy: DUP (general), INS (general), UMOV.
    if field(i, 31, 31) == 0 && field(i, 29, 21) == 0b0_0111_0000 && bit(i, 10) && !bit(i, 15) {
        return copy(c);
    }
    // Advanced SIMD three-same, logical subgroup.
    if !bit(i, 31) && field(i, 28, 24) == 0b01110 && bit(i, 21) && field(i, 15, 10) == 0b000111 {
        return logic3(c);
    }
    // Advanced SIMD modified immediate (MOVI/MVNI).
    if !bit(i, 31) && field(i, 28, 19) == 0b01111_00000 && bit(i, 10) && !bit(i, 11) {
        return modified_imm(c);
    }
    Err(c.undef())
}

/// Element size in bytes and index, from the imm5 field.
fn elem(imm5: u32) -> Option<(u32, u32)> {
    let tz = imm5.trailing_zeros();
    if tz > 3 {
        return None;
    }
    Some((1 << tz, imm5 >> (tz + 1)))
}

fn copy(c: &mut Ctx) -> Step {
    let i = c.insn;
    let q = bit(i, 30);
    let op = bit(i, 29);
    let imm4 = field(i, 14, 11);
    let Some((esize, index)) = elem(field(i, 20, 16)) else {
        return Err(c.undef());
    };
    let bits = esize * 8;
    let emask = ones(bits) as u128;
    let rn = field(i, 9, 5);
    let rd = field(i, 4, 0) as usize;
    match (op, imm4) {
        (false, 0b0001) => {
            // DUP Vd.T, Rn
            let v = c.s.xr(rn) as u128 & emask;
            let lanes = if q { 128 / bits } else { 64 / bits };
            let mut out = 0u128;
            for l in 0..lanes {
                out |= v << (l * bits);
            }
            c.s.v[rd] = out;
        }
        (false, 0b0011) if q => {
            // INS Vd.Ts[index], Rn
            let shift = index * bits;
            let v = c.s.xr(rn) as u128 & emask;
            c.s.v[rd] = (c.s.v[rd] & !(emask << shift)) | (v << shift);
        }
        (false, 0b0111) => {
            // UMOV Rd, Vn.Ts[index] (MOV alias for 32/64-bit)
            if (esize == 8) != q {
                return Err(c.undef());
            }
            let v = (c.s.v[rn as usize] >> (index * bits)) & emask;
            c.s.set_xr(rd as u32, v as u64);
        }
        _ => return Err(c.undef()),
    }
    Ok(Flow::Next)
}

fn logic3(c: &mut Ctx) -> Step {
    let i = c.insn;
    let q = bit(i, 30);
    let n = c.s.v[field(i, 9, 5) as usize];
    let m = c.s.v[field(i, 20, 16) as usize];
    let d = c.s.v[field(i, 4, 0) as usize];
    let r = match (bit(i, 29), field(i, 23, 22)) {
        (false, 0b00) => n & m,
        (false, 0b01) => n & !m,
        (false, 0b10) => n | m,
        (false, 0b11) => n | !m,
        (true, 0b00) => n ^ m,
        (true, 0b01) => (d & n) | (!d & m), // BSL
        (true, 0b10) => (m & n) | (!m & d), // BIT
        _ => (!m & n) | (m & d),            // BIF
    };
    let r = if q { r } else { r & u64::MAX as u128 };
    c.s.v[field(i, 4, 0) as usize] = r;
    Ok(Flow::Next)
}

fn modified_imm(c: &mut Ctx) -> Step {
    let i = c.insn;
    let q = bit(i, 30);
    let op = bit(i, 29);
    let cmode = field(i, 15, 12);
    let imm8 = ((field(i, 18, 16) << 5) | field(i, 9, 5)) as u64;
    // ORR/BIC immediate (cmode xxx1 below 0b1100) and FMOV immediate are TODO.
    let is_movi_mvni = matches!(
        cmode,
        0b0000 | 0b0010 | 0b0100 | 0b0110 | 0b1000 | 0b1010 | 0b1100 | 0b1101 | 0b1110
    );
    if !is_movi_mvni {
        return Err(c.undef());
    }
    let imm64: u64 = match cmode {
        0b0000 | 0b0010 | 0b0100 | 0b0110 => {
            let w = imm8 << (8 * (cmode >> 1));
            w | (w << 32)
        }
        0b1000 | 0b1010 => {
            let h = imm8 << (8 * ((cmode >> 1) & 1));
            h * 0x0001_0001_0001_0001
        }
        0b1100 => {
            let w = (imm8 << 8) | 0xff;
            w | (w << 32)
        }
        0b1101 => {
            let w = (imm8 << 16) | 0xffff;
            w | (w << 32)
        }
        _ if !op => imm8 * 0x0101_0101_0101_0101, // MOVI 8-bit
        _ => {
            // MOVI 64-bit: each imm8 bit expands to a whole byte.
            (0..8).fold(0u64, |acc, b| {
                if (imm8 >> b) & 1 != 0 {
                    acc | 0xff << (8 * b)
                } else {
                    acc
                }
            })
        }
    };
    // MVNI is MOVI inverted, except cmode 1110 where op selects the 64-bit form.
    let imm64 = if op && cmode != 0b1110 { !imm64 } else { imm64 };
    let v = if q {
        (imm64 as u128) | ((imm64 as u128) << 64)
    } else {
        imm64 as u128
    };
    c.s.v[field(i, 4, 0) as usize] = v;
    Ok(Flow::Next)
}
