//! Integer Advanced SIMD: three same, two-register misc, three different,
//! across lanes, scalar pairwise, shift by immediate and by element.
//! Floating-point encodings in these groups go to `vfp.rs`.

use super::super::bits::*;
use super::super::{Ctx, Flow, Step};
use super::*;

/// Three registers of the same arrangement.
pub fn three_same(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, size, opc) = (bit(i, 30), bit(i, 29), field(i, 23, 22), field(i, 15, 11));
    if opc >= 0b11000 {
        return vfp::three_same(c, scalar);
    }
    if opc == 0b00011 {
        return if scalar {
            Err(c.undef())
        } else {
            perm::logic(c)
        };
    }
    let ok = if scalar {
        match opc {
            0b00001 | 0b00101 | 0b01001 | 0b01011 => true,
            0b00110 | 0b00111 | 0b01000 | 0b01010 | 0b10000 | 0b10001 => size == 3,
            0b10110 => size == 1 || size == 2,
            _ => false,
        }
    } else {
        match opc {
            0b10110 => size == 1 || size == 2,
            0b10011 if u => size == 0, // PMUL
            0b10111 => !u && (size != 3 || q),
            0b00001 | 0b00101 | 0b00110 | 0b00111 | 0b01000..=0b01011 | 0b10000 | 0b10001 => {
                size != 3 || q
            }
            _ => size != 3,
        }
    };
    if !ok {
        return Err(c.undef());
    }
    let es = 8 << size;
    let bits = datasize(q, scalar, es);
    let lanes = bits / es;
    let (n, m, d) = (vreg(c, rn(i)), vreg(c, rm(i)), vreg(c, rd(i)));
    let mut r = 0u128;
    let mut qc = false;
    for e in 0..lanes {
        let (x, s) = if matches!(opc, 0b10100 | 0b10101 | 0b10111) {
            // Pairwise: adjacent elements of the concatenation Vm:Vn.
            let src = if 2 * e < lanes { n } else { m };
            let k = (2 * e) % lanes;
            let (a, b) = (get(src, es, k), get(src, es, k + 1));
            let (sa, sb) = (ext(a, es, u), ext(b, es, u));
            let v = match opc {
                0b10100 => sa.max(sb),
                0b10101 => sa.min(sb),
                _ => sa + sb,
            };
            (v as u64, false)
        } else {
            same_op(opc, u, get(n, es, e), get(m, es, e), get(d, es, e), es)
        };
        put(&mut r, es, e, x);
        qc |= s;
    }
    write(c, rd(i), r, bits);
    set_qc(c, qc);
    Ok(Flow::Next)
}

/// One element of an integer three-same operation; `d` is the destination
/// element for the accumulating forms.
fn same_op(opc: u32, u: bool, a: u64, b: u64, d: u64, es: u32) -> (u64, bool) {
    let (sa, sb) = (ext(a, es, u), ext(b, es, u));
    let t = |x: i128| (x as u64, false);
    match opc {
        0b00000 => t((sa + sb) >> 1),              // SHADD, UHADD
        0b00001 => sat(sa + sb, es, u),            // SQADD, UQADD
        0b00010 => t((sa + sb + 1) >> 1),          // SRHADD, URHADD
        0b00100 => t((sa - sb) >> 1),              // SHSUB, UHSUB
        0b00101 => sat(sa - sb, es, u),            // SQSUB, UQSUB
        0b00110 => (mask_if(sa > sb, es), false),  // CMGT, CMHI
        0b00111 => (mask_if(sa >= sb, es), false), // CMGE, CMHS
        // SSHL, SQSHL, SRSHL, SQRSHL and the unsigned forms: the shift is the
        // signed bottom byte of the second operand.
        0b01000..=0b01011 => {
            let r = shifted(sa, b as u8 as i8 as i32, es, opc & 2 != 0);
            if opc & 1 != 0 {
                sat(r, es, u)
            } else {
                t(r)
            }
        }
        0b01100 => t(sa.max(sb)),
        0b01101 => t(sa.min(sb)),
        0b01110 => t((sa - sb).abs()),                // SABD, UABD
        0b01111 => t(d as i128 + (sa - sb).abs()),    // SABA, UABA
        0b10000 if u => t(a as i128 - b as i128),     // SUB
        0b10000 => t(a as i128 + b as i128),          // ADD
        0b10001 if u => (mask_if(a == b, es), false), // CMEQ
        0b10001 => (mask_if(a & b != 0, es), false),  // CMTST
        0b10010 if u => t(d.wrapping_sub(a.wrapping_mul(b)) as i128), // MLS
        0b10010 => t(d.wrapping_add(a.wrapping_mul(b)) as i128), // MLA
        0b10011 if u => t(pmul(a, b) as i128),        // PMUL
        0b10011 => t(a.wrapping_mul(b) as i128),      // MUL
        _ => doubling_mul_high(a, b, es, u),          // SQDMULH, SQRDMULH
    }
}

/// SQDMULH / SQRDMULH: the high half of 2 * a * b, optionally rounded.
fn doubling_mul_high(a: u64, b: u64, es: u32, round: bool) -> (u64, bool) {
    let p = 2 * ext(a, es, false) * ext(b, es, false);
    let p = if round { p + (1 << (es - 1)) } else { p };
    sat(p >> es, es, false)
}

/// Polynomial (carry-less) multiply, `a` and `b` under 2^32.
fn pmul(a: u64, b: u64) -> u64 {
    (0..32)
        .filter(|k| (b >> k) & 1 != 0)
        .fold(0, |acc, k| acc ^ (a << k))
}

/// SQRDMLAH, SQRDMLSH (Armv8.1 RDM): rounding doubling multiply
/// accumulating into the high half, saturated.
fn rdm(a: u64, b: u64, d: u64, es: u32, sub: bool) -> (u64, bool) {
    let p = 2 * ext(a, es, false) * ext(b, es, false);
    let acc = (ext(d, es, false) << es) + if sub { -p } else { p } + (1 << (es - 1));
    sat(acc >> es, es, false)
}

/// Three same (extra): SQRDMLAH and SQRDMLSH. The dot-product and complex
/// forms (Armv8.2 DotProd, Armv8.3) are not implemented.
pub fn three_same_extra(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, size, opc) = (bit(i, 30), bit(i, 29), field(i, 23, 22), field(i, 14, 11));
    if opc == 0b0010 {
        // SDOT, UDOT (FEAT_DotProd): byte group e of Vm with byte group e
        // of Vn.
        if scalar || size != 2 {
            return Err(c.undef());
        }
        let m = vreg(c, rm(i));
        dot(c, u, q, |e| get(m, 32, e));
        return Ok(Flow::Next);
    }
    if !u || opc > 1 || !(size == 1 || size == 2) {
        return Err(c.undef());
    }
    let es = 8 << size;
    let bits = datasize(q, scalar, es);
    let (n, m, d) = (vreg(c, rn(i)), vreg(c, rm(i)), vreg(c, rd(i)));
    let mut r = 0u128;
    let mut qc = false;
    for e in 0..bits / es {
        let (x, s) = rdm(get(n, es, e), get(m, es, e), get(d, es, e), es, opc == 1);
        put(&mut r, es, e, x);
        qc |= s;
    }
    write(c, rd(i), r, bits);
    set_qc(c, qc);
    Ok(Flow::Next)
}

/// SDOT and UDOT: each 32-bit lane of Vd gains the sum of the four products
/// of byte group e of Vn with the bytes of `group(e)`, wrapping.
fn dot(c: &mut Ctx, unsigned: bool, q: bool, group: impl Fn(u32) -> u64) {
    let i = c.insn;
    let (n, d) = (vreg(c, rn(i)), vreg(c, rd(i)));
    let bits = if q { 128 } else { 64 };
    let mut r = 0u128;
    for e in 0..bits / 32 {
        let g = group(e);
        let sum: i128 = (0..4)
            .map(|k| ext(get(n, 8, 4 * e + k), 8, unsigned) * ext(g >> (8 * k), 8, unsigned))
            .sum();
        put(&mut r, 32, e, get(d, 32, e).wrapping_add(sum as u64));
    }
    write(c, rd(i), r, bits);
}

/// Two-register miscellaneous.
pub fn misc(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, size, opc) = (bit(i, 30), bit(i, 29), field(i, 23, 22), field(i, 16, 12));
    if opc >= 0b11000 || opc == 0b10110 || opc == 0b10111 || (0b01100..=0b01111).contains(&opc) {
        return vfp::misc(c, scalar);
    }
    // RBIT is encoded with size = 01 but works on bytes.
    let es = if u && opc == 0b00101 { 8 } else { 8 << size };
    let ok = match (u, opc) {
        (_, 0b00011) | (_, 0b00111) => size != 3 || q || scalar, // SUQADD, USQADD, SQABS, SQNEG
        (_, 0b01000) | (_, 0b01001) | (false, 0b01010) | (_, 0b01011) => {
            if scalar {
                size == 3
            } else {
                size != 3 || q
            }
        }
        (_, 0b10100) | (_, 0b10010) => size != 3, // XTN, SQXTN, UQXTN, SQXTUN
        _ if scalar => false,
        (false, 0b00000) | (_, 0b00010) | (_, 0b00110) | (_, 0b00100) | (true, 0b10011) => {
            size != 3
        }
        (true, 0b00000) => size < 2,   // REV32
        (false, 0b00001) => size == 0, // REV16
        (false, 0b00101) => size == 0, // CNT
        (true, 0b00101) => size < 2,   // NOT, RBIT
        _ => false,
    };
    if !ok {
        return Err(c.undef());
    }
    let n = vreg(c, rn(i));
    let d = vreg(c, rd(i));
    match opc {
        0b10010 | 0b10100 => return narrow(c, scalar, u, opc, es, n),
        0b00010 | 0b00110 => {
            // SADDLP, UADDLP, SADALP, UADALP
            let bits = datasize(q, false, es);
            let es2 = es * 2;
            let mut r = 0u128;
            for e in 0..bits / es2 {
                let mut x = ext(get(n, es, 2 * e), es, u) + ext(get(n, es, 2 * e + 1), es, u);
                if opc == 0b00110 {
                    x += get(d, es2, e) as i128;
                }
                put(&mut r, es2, e, x as u64);
            }
            write(c, rd(i), r, bits);
            return Ok(Flow::Next);
        }
        0b10011 => {
            // SHLL, SHLL2: widen and shift by the element size.
            let mut r = 0u128;
            let base = if q { 64 / es } else { 0 };
            for e in 0..64 / es {
                put(&mut r, es * 2, e, get(n, es, base + e) << es);
            }
            write(c, rd(i), r, 128);
            return Ok(Flow::Next);
        }
        _ => {}
    }
    let bits = datasize(q, scalar, es);
    let mut r = 0u128;
    let mut qc = false;
    for e in 0..bits / es {
        let a = get(n, es, e);
        let sa = ext(a, es, false);
        let t = |x: i128| (x as u64, false);
        let (x, s) = match (u, opc) {
            // REV64, REV32, REV16: reverse elements within each container.
            (_, 0b00000) | (false, 0b00001) => {
                let container = match (u, opc) {
                    (false, 0b00000) => 64,
                    (true, _) => 32,
                    _ => 16,
                };
                let k = e ^ (container / es - 1);
                put(&mut r, es, k, a);
                continue;
            }
            (false, 0b00011) => sat(ext(get(d, es, e), es, false) + ext(a, es, true), es, false), // SUQADD
            (true, 0b00011) => sat(ext(get(d, es, e), es, true) + sa, es, true), // USQADD
            (false, 0b00100) => {
                t(((a ^ (a >> 1)) & ones(es - 1)).leading_zeros() as i128 - (65 - es) as i128)
            } // CLS
            (true, 0b00100) => t(a.leading_zeros() as i128 - (64 - es) as i128), // CLZ
            (false, 0b00101) => t(a.count_ones() as i128),                       // CNT
            (true, 0b00101) if size == 0 => t(!a as i128),                       // NOT
            (true, 0b00101) => t((a as u8).reverse_bits() as i128),              // RBIT
            (false, 0b00111) => sat(sa.abs(), es, false),                        // SQABS
            (true, 0b00111) => sat(-sa, es, false),                              // SQNEG
            (false, 0b01000) => (mask_if(sa > 0, es), false),
            (true, 0b01000) => (mask_if(sa >= 0, es), false),
            (false, 0b01001) => (mask_if(sa == 0, es), false),
            (true, 0b01001) => (mask_if(sa <= 0, es), false),
            (false, 0b01010) => (mask_if(sa < 0, es), false),
            (false, 0b01011) => t(sa.abs()), // ABS
            _ => t(-sa),                     // NEG
        };
        put(&mut r, es, e, x);
        qc |= s;
    }
    write(c, rd(i), r, bits);
    set_qc(c, qc);
    Ok(Flow::Next)
}

/// XTN, SQXTN, UQXTN, SQXTUN: narrow 2*es-bit elements to es bits.
fn narrow(c: &mut Ctx, scalar: bool, u: bool, opc: u32, es: u32, n: u128) -> Step {
    let i = c.insn;
    let lanes = if scalar { 1 } else { 64 / es };
    let mut r = 0u64;
    let mut qc = false;
    for e in 0..lanes {
        let x = get(n, es * 2, e);
        let (v, s) = match (u, opc) {
            (false, 0b10010) => (x, false),
            (true, 0b10010) => sat(ext(x, es * 2, false), es, true), // SQXTUN
            (false, _) => sat(ext(x, es * 2, false), es, false),     // SQXTN
            (true, _) => sat(ext(x, es * 2, true), es, true),        // UQXTN
        };
        r |= (v & ones(es)) << (e * es);
        qc |= s;
    }
    if scalar {
        write(c, rd(i), r as u128, es);
    } else {
        write_narrow(c, rd(i), r, bit(i, 30));
    }
    set_qc(c, qc);
    Ok(Flow::Next)
}

/// Widening and narrowing operations on three registers.
pub fn three_different(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, size, opc) = (bit(i, 30), bit(i, 29), field(i, 23, 22), field(i, 15, 12));
    let sqd = matches!(opc, 0b1001 | 0b1011 | 0b1101);
    let ok = match opc {
        _ if scalar => sqd && !u && (size == 1 || size == 2),
        _ if sqd => !u && (size == 1 || size == 2),
        // PMULL: 8-bit lanes, or one 64-bit lane (FEAT_PMULL).
        0b1110 => !u && (size == 0 || size == 3),
        0b1111 => false,
        _ => size != 3,
    };
    if !ok {
        return Err(c.undef());
    }
    let es = 8 << size;
    let es2 = es * 2;
    let (n, m, d) = (vreg(c, rn(i)), vreg(c, rm(i)), vreg(c, rd(i)));
    let lanes = if scalar { 1 } else { 64 / es };
    // The "2" forms read the upper halves; scalar forms always set bit 30.
    let base = if q && !scalar { lanes } else { 0 };
    if opc == 0b1110 && size == 3 {
        // PMULL, PMULL2 (1Q): one 128-bit carry-less product.
        let (a, b) = (get(n, 64, base), get(m, 64, base));
        let r = (0..64)
            .filter(|k| (b >> k) & 1 != 0)
            .fold(0u128, |acc, k| acc ^ ((a as u128) << k));
        write(c, rd(i), r, 128);
        return Ok(Flow::Next);
    }
    if matches!(opc, 0b0100 | 0b0110) {
        // ADDHN, RADDHN, SUBHN, RSUBHN: high half of a wide sum.
        let mut r = 0u64;
        for e in 0..lanes {
            let (a, b) = (get(n, es2, e) as u128, get(m, es2, e) as u128);
            let mut x = if opc == 0b0100 {
                a + b
            } else {
                a.wrapping_sub(b)
            };
            if u {
                x = x.wrapping_add(1 << (es - 1));
            }
            r |= ((x >> es) as u64 & ones(es)) << (e * es);
        }
        write_narrow(c, rd(i), r, q);
        return Ok(Flow::Next);
    }
    let mut r = 0u128;
    let mut qc = false;
    for e in 0..lanes {
        let b = get(m, es, base + e);
        let (x, s) = if matches!(opc, 0b0001 | 0b0011) {
            // SADDW, SSUBW and the unsigned forms: wide first operand.
            let a = ext(get(n, es2, e), es2, u);
            let v = if opc == 0b0001 {
                a + ext(b, es, u)
            } else {
                a - ext(b, es, u)
            };
            (v as u64, false)
        } else {
            long_op(opc, u, get(n, es, base + e), b, get(d, es2, e), es)
        };
        put(&mut r, es2, e, x);
        qc |= s;
    }
    write(c, rd(i), r, if scalar { es2 } else { 128 });
    set_qc(c, qc);
    Ok(Flow::Next)
}

/// One element of a widening operation: `a`, `b` are es bits, the result and
/// the accumulator `d` are 2*es bits. Shared with the by-element forms.
fn long_op(opc: u32, u: bool, a: u64, b: u64, d: u64, es: u32) -> (u64, bool) {
    let es2 = es * 2;
    let (sa, sb) = (ext(a, es, u), ext(b, es, u));
    let t = |x: i128| (x as u64, false);
    let doubled = || sat(2 * sa * sb, es2, false);
    match opc {
        0b0000 => t(sa + sb),                     // SADDL, UADDL
        0b0010 => t(sa - sb),                     // SSUBL, USUBL
        0b0101 => t(d as i128 + (sa - sb).abs()), // SABAL, UABAL
        0b0111 => t((sa - sb).abs()),             // SABDL, UABDL
        0b1000 => t(d as i128 + sa * sb),         // SMLAL, UMLAL
        0b1010 => t(d as i128 - sa * sb),         // SMLSL, UMLSL
        0b1100 => t(sa * sb),                     // SMULL, UMULL
        0b1101 => doubled(),                      // SQDMULL
        0b1110 => t(pmul(a, b) as i128),          // PMULL
        _ => {
            // SQDMLAL (0b1001), SQDMLSL (0b1011): both steps saturate.
            let (p, s1) = doubled();
            let p = ext(p, es2, false);
            let acc = ext(d, es2, false);
            let (v, s2) = sat(if opc == 0b1001 { acc + p } else { acc - p }, es2, false);
            (v, s1 || s2)
        }
    }
}

/// ADDV, SADDLV, UADDLV, SMAXV, UMAXV, SMINV, UMINV.
pub fn across(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (q, u, size, opc) = (bit(i, 30), bit(i, 29), field(i, 23, 22), field(i, 16, 12));
    if opc == 0b01100 || opc == 0b01111 {
        return vfp::across(c);
    }
    let valid_op = matches!(opc, 0b00011 | 0b01010 | 0b11010) || (opc == 0b11011 && !u);
    if !valid_op || size == 3 || (size == 2 && !q) {
        return Err(c.undef());
    }
    let es = 8 << size;
    let n = vreg(c, rn(i));
    let lanes = datasize(q, false, es) / es;
    let vals = (0..lanes).map(|e| ext(get(n, es, e), es, u));
    let (r, out) = match opc {
        0b00011 => (vals.sum::<i128>(), es * 2),
        0b01010 => (vals.max().unwrap(), es),
        0b11010 => (vals.min().unwrap(), es),
        _ => (vals.sum::<i128>(), es),
    };
    write(c, rd(i), r as u64 as u128, out);
    Ok(Flow::Next)
}

/// Scalar pairwise: ADDP (integer) and the floating-point forms.
pub fn pairwise_scalar(c: &mut Ctx) -> Step {
    let i = c.insn;
    if bit(i, 29) {
        return vfp::pairwise_scalar(c);
    }
    if matches!(field(i, 16, 12), 0b01100 | 0b01101 | 0b01111) {
        // FADDP, FMAXP, FMINP, FMAXNMP, FMINNMP (scalar, half precision)
        return vfp::pairwise_scalar(c);
    }
    if field(i, 16, 12) != 0b11011 || field(i, 23, 22) != 3 {
        return Err(c.undef());
    }
    let n = vreg(c, rn(i));
    let r = get(n, 64, 0).wrapping_add(get(n, 64, 1));
    write(c, rd(i), r as u128, 64);
    Ok(Flow::Next)
}

/// Shift by immediate, vector and scalar.
pub fn shift_imm(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, opc) = (bit(i, 30), bit(i, 29), field(i, 15, 11));
    let immh = field(i, 22, 19);
    let immhb = field(i, 22, 16);
    let es = 8 << (31 - immh.leading_zeros());
    if opc == 0b11100 || opc == 0b11111 {
        return vfp::fixed_convert(c, scalar, es, 2 * es - immhb);
    }
    let narrowing = (0b10000..=0b10011).contains(&opc);
    let left = matches!(opc, 0b01010 | 0b01100 | 0b01110 | 0b10100);
    let ok = match opc {
        0b00000 | 0b00010 | 0b00100 | 0b00110 | 0b01010 => true,
        0b01000 | 0b01100 => u,
        0b01110 => true,
        0b10000 | 0b10001 => u || !scalar, // SHRN/RSHRN have no scalar form
        0b10010 | 0b10011 => true,
        0b10100 => !scalar,
        _ => false,
    };
    let size_ok = if narrowing || opc == 0b10100 {
        es != 64
    } else if scalar {
        es == 64 || matches!(opc, 0b01100 | 0b01110)
    } else {
        es != 64 || q
    };
    if !ok || !size_ok {
        return Err(c.undef());
    }
    let sh = if left {
        (immhb - es) as i32
    } else {
        (2 * es - immhb) as i32
    };
    let n = vreg(c, rn(i));
    let d = vreg(c, rd(i));
    if narrowing {
        // SHRN, RSHRN, SQSHRN, SQRSHRN, UQSHRN, UQRSHRN, SQSHRUN, SQRSHRUN.
        let lanes = if scalar { 1 } else { 64 / es };
        let round = opc & 1 != 0;
        let (in_unsigned, out) = match (u, opc) {
            (false, 0b10000 | 0b10001) => (true, None),
            (true, 0b10000 | 0b10001) => (false, Some(true)),
            (_, _) => (u, Some(u)),
        };
        let mut r = 0u64;
        let mut qc = false;
        for e in 0..lanes {
            let x = shifted(
                ext(get(n, es * 2, e), es * 2, in_unsigned),
                -sh,
                es * 2,
                round,
            );
            let (v, s) = match out {
                None => (x as u64 & ones(es), false),
                Some(unsigned) => sat(x, es, unsigned),
            };
            r |= v << (e * es);
            qc |= s;
        }
        if scalar {
            write(c, rd(i), r as u128, es);
        } else {
            write_narrow(c, rd(i), r, q);
        }
        set_qc(c, qc);
        return Ok(Flow::Next);
    }
    if opc == 0b10100 {
        // SSHLL, USHLL (and the SXTL, UXTL aliases).
        let base = if q { 64 / es } else { 0 };
        let mut r = 0u128;
        for e in 0..64 / es {
            let x = ext(get(n, es, base + e), es, u) << sh;
            put(&mut r, es * 2, e, x as u64);
        }
        write(c, rd(i), r, 128);
        return Ok(Flow::Next);
    }
    let bits = datasize(q, scalar, es);
    let mut r = 0u128;
    let mut qc = false;
    for e in 0..bits / es {
        let (a, acc) = (get(n, es, e), get(d, es, e));
        let (x, s) = match (u, opc) {
            (_, 0b00000..=0b00110) => {
                // SSHR, SSRA, SRSHR, SRSRA and the unsigned forms.
                let v = shifted(ext(a, es, u), -sh, es, opc & 0b100 != 0) as u64;
                let v = if opc & 0b010 != 0 {
                    acc.wrapping_add(v)
                } else {
                    v
                };
                (v, false)
            }
            (true, 0b01000) => {
                // SRI: insert the shifted value below the kept top bits.
                let keep = !ones(es).checked_shr(sh as u32).unwrap_or(0) & ones(es);
                let v = shifted(ext(a, es, true), -sh, es, false) as u64;
                ((acc & keep) | v, false)
            }
            (false, 0b01010) => (a << sh, false), // SHL
            (true, 0b01010) => {
                // SLI
                let keep = ones(sh as u32);
                ((acc & keep) | (a << sh), false)
            }
            (true, 0b01100) => sat(shifted(ext(a, es, false), sh, es, false), es, true), // SQSHLU
            _ => sat(shifted(ext(a, es, u), sh, es, false), es, u), // SQSHL, UQSHL
        };
        put(&mut r, es, e, x);
        qc |= s;
    }
    write(c, rd(i), r, bits);
    set_qc(c, qc);
    Ok(Flow::Next)
}

/// Multiplies by an element, vector and scalar.
pub fn by_element(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, size, opc) = (bit(i, 30), bit(i, 29), field(i, 23, 22), field(i, 15, 12));
    if matches!((u, opc), (false, 0b0001 | 0b0101 | 0b1001) | (true, 0b1001)) {
        return vfp::by_element(c, scalar);
    }
    if opc == 0b1110 {
        // SDOT, UDOT (by element): every byte group of Vn with the one
        // group of Vm at index H:L.
        if scalar || size != 2 {
            return Err(c.undef());
        }
        let index = (field(i, 11, 11) << 1) | field(i, 21, 21);
        let group = get(vreg(c, rm(i)), 32, index);
        dot(c, u, q, |_| group);
        return Ok(Flow::Next);
    }
    let long = matches!(opc, 0b0010 | 0b0110 | 0b1010 | 0b0011 | 0b0111 | 0b1011);
    let ok = match (u, opc) {
        (true, 0b0000 | 0b0100) | (false, 0b1000) => !scalar,
        (_, 0b0010 | 0b0110 | 0b1010) => !scalar,
        (false, 0b0011 | 0b0111 | 0b1011 | 0b1100 | 0b1101) | (true, 0b1101 | 0b1111) => true,
        _ => false,
    };
    if !ok || !(size == 1 || size == 2) {
        return Err(c.undef());
    }
    let es = 8 << size;
    let (index, m) = if size == 1 {
        (
            (field(i, 11, 11) << 2) | (field(i, 21, 21) << 1) | field(i, 20, 20),
            field(i, 19, 16),
        )
    } else {
        ((field(i, 11, 11) << 1) | field(i, 21, 21), field(i, 20, 16))
    };
    let b = get(vreg(c, m), es, index);
    let (n, d) = (vreg(c, rn(i)), vreg(c, rd(i)));
    let mut r = 0u128;
    let mut qc = false;
    if long {
        let lanes = if scalar { 1 } else { 64 / es };
        let base = if q && !scalar { lanes } else { 0 };
        // The by-element opcodes map onto the three-different ones.
        let lop = match opc {
            0b0010 => 0b1000,
            0b0110 => 0b1010,
            0b1010 => 0b1100,
            0b0011 => 0b1001,
            0b0111 => 0b1011,
            _ => 0b1101,
        };
        for e in 0..lanes {
            let (x, s) = long_op(lop, u, get(n, es, base + e), b, get(d, es * 2, e), es);
            put(&mut r, es * 2, e, x);
            qc |= s;
        }
        write(c, rd(i), r, if scalar { es * 2 } else { 128 });
    } else {
        let bits = datasize(q, scalar, es);
        for e in 0..bits / es {
            let (a, acc) = (get(n, es, e), get(d, es, e));
            let (x, s) = match (u, opc) {
                (true, 0b0000) => (acc.wrapping_add(a.wrapping_mul(b)), false),
                (true, 0b0100) => (acc.wrapping_sub(a.wrapping_mul(b)), false),
                (false, 0b1000) => (a.wrapping_mul(b), false),
                (true, _) => rdm(a, b, acc, es, opc == 0b1111), // SQRDMLAH, SQRDMLSH
                _ => doubling_mul_high(a, b, es, opc == 0b1101),
            };
            put(&mut r, es, e, x);
            qc |= s;
        }
        write(c, rd(i), r, bits);
    }
    set_qc(c, qc);
    Ok(Flow::Next)
}
