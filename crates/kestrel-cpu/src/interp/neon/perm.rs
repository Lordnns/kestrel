//! Element copies (DUP, INS, UMOV, SMOV), permutes (UZP, TRN, ZIP), EXT,
//! table lookup (TBL, TBX), vector logic and the modified-immediate group
//! (MOVI, MVNI, ORR, BIC, FMOV).

use super::super::bits::*;
use super::super::{Ctx, Flow, Step};
use super::*;
use crate::float::{self as fpu, Fmt};
use crate::Unmodelled;

/// Advanced SIMD copy, and the scalar DUP (element).
pub fn copy(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, op, imm4) = (bit(i, 30), bit(i, 29), field(i, 14, 11));
    let imm5 = field(i, 20, 16);
    let tz = imm5.trailing_zeros();
    if tz > 3 {
        return Err(c.undef());
    }
    let es = 8 << tz;
    let index = imm5 >> (tz + 1);
    let (n, d) = (rn(i), rd(i));
    if scalar {
        if op || imm4 != 0 {
            return Err(c.undef());
        }
        let v = get(vreg(c, n), es, index);
        write(c, d, v as u128, es);
        return Ok(Flow::Next);
    }
    let bits = datasize(q, false, es);
    match (op, imm4) {
        (false, 0b0000 | 0b0001) => {
            // DUP (element), DUP (general)
            if es == 64 && !q {
                return Err(c.undef());
            }
            let v = if imm4 == 0 {
                get(vreg(c, n), es, index)
            } else {
                c.s.xr(n)
            };
            let mut r = 0u128;
            for e in 0..bits / es {
                put(&mut r, es, e, v);
            }
            write(c, d, r, bits);
        }
        (false, 0b0011) if q => {
            // INS (general)
            let v = c.s.xr(n);
            put(&mut c.s.v[d as usize], es, index, v);
        }
        (false, 0b0101) => {
            // SMOV: sign-extend into Wd (Q=0) or Xd (Q=1).
            if es == 64 || (es == 32 && !q) {
                return Err(c.undef());
            }
            let v = sext(get(vreg(c, n), es, index), es) as u64;
            c.s.set_xr(d, if q { v } else { v & 0xffff_ffff });
        }
        (false, 0b0111) => {
            // UMOV (MOV alias for 32 and 64 bits): Xd only for 64-bit elements.
            if (es == 64) != q {
                return Err(c.undef());
            }
            let v = get(vreg(c, n), es, index);
            c.s.set_xr(d, v);
        }
        (true, _) if q => {
            // INS (element)
            let v = get(vreg(c, n), es, imm4 >> tz);
            put(&mut c.s.v[d as usize], es, index, v);
        }
        _ => return Err(c.undef()),
    }
    Ok(Flow::Next)
}

/// UZP1, UZP2, TRN1, TRN2, ZIP1, ZIP2.
pub fn permute(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (q, size, opc) = (bit(i, 30), field(i, 23, 22), field(i, 14, 12));
    if (size == 3 && !q) || opc & 3 == 0 {
        return Err(c.undef());
    }
    let es = 8 << size;
    let bits = datasize(q, false, es);
    let lanes = bits / es;
    let part = opc >> 2;
    let (n, m) = (vreg(c, rn(i)), vreg(c, rm(i)));
    let mut r = 0u128;
    for e in 0..lanes {
        let (src, k) = match opc & 3 {
            // UZP: even or odd elements of the concatenation Vm:Vn.
            1 => {
                let k = 2 * e + part;
                if k < lanes {
                    (n, k)
                } else {
                    (m, k - lanes)
                }
            }
            // TRN: even lanes from Vn, odd lanes from Vm.
            2 => (if e & 1 == 0 { n } else { m }, (e & !1) + part),
            // ZIP: interleave the low (or high) halves.
            _ => (if e & 1 == 0 { n } else { m }, part * lanes / 2 + e / 2),
        };
        put(&mut r, es, e, get(src, es, k));
    }
    write(c, rd(i), r, bits);
    Ok(Flow::Next)
}

/// EXT: bytes `imm4` upward of the concatenation Vm:Vn.
pub fn ext(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (q, imm4) = (bit(i, 30), field(i, 14, 11));
    if field(i, 23, 22) != 0 || (!q && imm4 >= 8) {
        return Err(c.undef());
    }
    let (n, m) = (vreg(c, rn(i)), vreg(c, rm(i)));
    let pos = imm4 * 8;
    let r = if q {
        if pos == 0 {
            n
        } else {
            (n >> pos) | (m << (128 - pos))
        }
    } else {
        let concat = ((m as u64 as u128) << 64) | n as u64 as u128;
        concat >> pos
    };
    write(c, rd(i), r, if q { 128 } else { 64 });
    Ok(Flow::Next)
}

/// TBL, TBX: byte lookup in a table of one to four consecutive registers.
pub fn table(c: &mut Ctx) -> Step {
    let i = c.insn;
    if field(i, 23, 22) != 0 {
        return Err(c.undef());
    }
    let q = bit(i, 30);
    let regs = field(i, 14, 13) + 1;
    let tbx = bit(i, 12);
    let bits = datasize(q, false, 8);
    let (idx, d) = (vreg(c, rm(i)), vreg(c, rd(i)));
    let mut r = 0u128;
    for e in 0..bits / 8 {
        let k = get(idx, 8, e) as u32;
        let v = if k < 16 * regs {
            get(vreg(c, rn(i) + k / 16), 8, k % 16)
        } else if tbx {
            get(d, 8, e)
        } else {
            0
        };
        put(&mut r, 8, e, v);
    }
    write(c, rd(i), r, bits);
    Ok(Flow::Next)
}

/// AND, BIC, ORR, ORN, EOR, BSL, BIT, BIF.
pub fn logic(c: &mut Ctx) -> Step {
    let i = c.insn;
    let n = vreg(c, rn(i));
    let m = vreg(c, rm(i));
    let d = vreg(c, rd(i));
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
    write(c, rd(i), r, if bit(i, 30) { 128 } else { 64 });
    Ok(Flow::Next)
}

/// MOVI, MVNI, ORR and BIC (immediate), FMOV (vector, immediate).
pub fn modified_imm(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (q, op, cmode) = (bit(i, 30), bit(i, 29), field(i, 15, 12));
    let imm8 = ((field(i, 18, 16) << 5) | field(i, 9, 5)) as u64;
    if bit(i, 11) {
        // o2 = 1 is FMOV (half precision) or unallocated.
        return Err(if cmode == 0b1111 && !op {
            c.unsupported(Unmodelled::HalfPrecision)
        } else {
            c.undef()
        });
    }
    let rep32 = |w: u64| w | (w << 32);
    // AdvSIMDExpandImm.
    let imm64: u64 = match cmode {
        0b0000..=0b0111 => rep32(imm8 << (8 * (cmode >> 1))),
        0b1000..=0b1011 => (imm8 << (8 * ((cmode >> 1) & 1))) * 0x0001_0001_0001_0001,
        0b1100 => rep32((imm8 << 8) | 0xff),
        0b1101 => rep32((imm8 << 16) | 0xffff),
        0b1110 if !op => imm8 * 0x0101_0101_0101_0101,
        0b1110 => (0..8)
            .filter(|b| (imm8 >> b) & 1 != 0)
            .fold(0u64, |acc, b| acc | 0xff << (8 * b)),
        _ if !op => rep32(fpu::expand_imm(imm8 as u32, Fmt::S)),
        _ if q => fpu::expand_imm(imm8 as u32, Fmt::D),
        _ => return Err(c.undef()),
    };
    let bits = if q { 128 } else { 64 };
    let imm = (imm64 as u128) | ((imm64 as u128) << 64);
    let d = vreg(c, rd(i));
    // ORR and BIC are cmode 0xx1 and 10x1; MVNI inverts MOVI except where op
    // selects MOVI 64-bit or FMOV double.
    let r = match (op, cmode) {
        (false, 0b0001 | 0b0011 | 0b0101 | 0b0111 | 0b1001 | 0b1011) => d | imm,
        (true, 0b0001 | 0b0011 | 0b0101 | 0b0111 | 0b1001 | 0b1011) => d & !imm,
        (true, 0b1110 | 0b1111) | (false, _) => imm,
        (true, _) => !imm,
    };
    write(c, rd(i), r, bits);
    Ok(Flow::Next)
}
