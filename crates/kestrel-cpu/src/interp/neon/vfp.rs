//! Floating-point Advanced SIMD lanes: three same, two-register misc,
//! across lanes, scalar pairwise, by element and fixed-point conversion.
//! Every lane is computed by [`crate::float`]; half precision stops with an
//! explicit unsupported error.

use super::super::bits::*;
use super::super::{Ctx, Flow, Step};
use super::*;
use crate::float::{self as fpu, Cmp, Fmt, Fpcr, Op2, Rounding};
use crate::{Exit, Unmodelled};

type Lane = Result<u64, Unmodelled>;

fn fmt(sz: bool) -> Fmt {
    if sz {
        Fmt::D
    } else {
        Fmt::S
    }
}

fn fpcr(c: &Ctx) -> Fpcr {
    Fpcr::from_bits(c.s.fpcr)
}

/// Run `op` on every lane and write the result.
fn lanes(c: &mut Ctx, bits: u32, es: u32, mut op: impl FnMut(u32) -> Lane) -> Result<(), Exit> {
    let mut r = 0u128;
    for e in 0..bits / es {
        let x = op(e).map_err(|what| c.unsupported(what))?;
        put(&mut r, es, e, x);
    }
    write(c, rd(c.insn), r, bits);
    Ok(())
}

/// One lane of a floating-point three-same operation, keyed by
/// (U, size<1>, opcode). `d` is the destination element for FMLA and FMLS.
fn same_op(key: (bool, bool, u32), a: u64, b: u64, d: u64, f: Fmt, fpcr: Fpcr) -> Option<Lane> {
    let cmp = |x: u64, y: u64, cmp| Ok(mask_if(fpu::compare_cond(x, y, f, fpcr, cmp), f.width()));
    let abs = |x: u64| x & !f.sign();
    Some(match key {
        (false, false, 0b11000) | (true, false, 0b11000) => {
            Ok(fpu::max_min(a, b, f, fpcr, true, true))
        }
        (false, true, 0b11000) | (true, true, 0b11000) => {
            Ok(fpu::max_min(a, b, f, fpcr, false, true))
        }
        (false, false, 0b11110) | (true, false, 0b11110) => {
            Ok(fpu::max_min(a, b, f, fpcr, true, false))
        }
        (false, true, 0b11110) | (true, true, 0b11110) => {
            Ok(fpu::max_min(a, b, f, fpcr, false, false))
        }
        (false, false, 0b11001) => fpu::mul_add(d, a, b, f, fpcr), // FMLA
        (false, true, 0b11001) => fpu::mul_add(d, a ^ f.sign(), b, f, fpcr), // FMLS
        (false, false, 0b11010) | (true, false, 0b11010) => fpu::arith(Op2::Add, a, b, f, fpcr),
        (false, true, 0b11010) => fpu::arith(Op2::Sub, a, b, f, fpcr),
        (true, true, 0b11010) => fpu::arith(Op2::Sub, a, b, f, fpcr).map(abs), // FABD
        (false, false, 0b11011) => fpu::mulx(a, b, f, fpcr),
        (true, false, 0b11011) => fpu::arith(Op2::Mul, a, b, f, fpcr),
        (true, false, 0b11111) => fpu::arith(Op2::Div, a, b, f, fpcr),
        (false, false, 0b11100) => cmp(a, b, Cmp::Eq),
        (true, false, 0b11100) => cmp(a, b, Cmp::Ge),
        (true, true, 0b11100) => cmp(a, b, Cmp::Gt),
        (true, false, 0b11101) => cmp(abs(a), abs(b), Cmp::Ge), // FACGE
        (true, true, 0b11101) => cmp(abs(a), abs(b), Cmp::Gt),  // FACGT
        (false, false, 0b11111) => fpu::recip_step(a, b, f, fpcr, false),
        (false, true, 0b11111) => fpu::recip_step(a, b, f, fpcr, true),
        _ => return None,
    })
}

/// Floating-point three same (opcodes 11000..11111).
pub fn three_same(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, hi, sz, opc) = (
        bit(i, 30),
        bit(i, 29),
        bit(i, 23),
        bit(i, 22),
        field(i, 15, 11),
    );
    let key = (u, hi, opc);
    let pairwise = u && matches!((hi, opc), (_, 0b11000) | (false, 0b11010) | (_, 0b11110));
    let scalar_ok = matches!(
        key,
        (false, false, 0b11011 | 0b11100 | 0b11111)
            | (false, true, 0b11111)
            | (true, false, 0b11100 | 0b11101)
            | (true, true, 0b11010 | 0b11100 | 0b11101)
    );
    let f = fmt(sz);
    let fpcr = fpcr(c);
    if (scalar && !scalar_ok) || (sz && !q && !scalar) || same_op(key, 0, 0, 0, f, fpcr).is_none() {
        return Err(c.undef());
    }
    let es = f.width();
    let bits = datasize(q, scalar, es);
    let count = bits / es;
    let (n, m, d) = (vreg(c, rn(i)), vreg(c, rm(i)), vreg(c, rd(i)));
    lanes(c, bits, es, |e| {
        let (a, b) = if pairwise {
            let src = if 2 * e < count { n } else { m };
            let k = (2 * e) % count;
            (get(src, es, k), get(src, es, k + 1))
        } else {
            (get(n, es, e), get(m, es, e))
        };
        same_op(key, a, b, get(d, es, e), f, fpcr).expect("checked above")
    })?;
    Ok(Flow::Next)
}

/// Floating-point two-register misc, plus URECPE and URSQRTE.
pub fn misc(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, opc) = (bit(i, 30), bit(i, 29), field(i, 16, 12));
    let (hi, sz) = (bit(i, 23), bit(i, 22));
    let f = fmt(sz);
    let fpcr = fpcr(c);
    let n = vreg(c, rn(i));
    match (u, hi, opc) {
        (false, false, 0b10110 | 0b10111) | (true, false, 0b10110) => {
            return convert_precision(c, scalar, u, opc)
        }
        _ => {}
    }
    if sz && !q && !scalar {
        return Err(c.undef());
    }
    let vector_only = matches!(
        (u, hi, opc),
        (_, _, 0b11000 | 0b11001) | (_, true, 0b01111) | (_, true, 0b11100) | (true, true, 0b11111)
    );
    let key = (u, hi, opc);
    let invalid = (scalar && vector_only)
        || (!scalar && key == (false, true, 0b11111))
        || (sz && opc == 0b11100 && hi)
        || misc_op(key, 0, f, fpcr).is_none();
    if invalid {
        return Err(c.undef());
    }
    let es = f.width();
    lanes(c, datasize(q, scalar, es), es, |e| {
        misc_op(key, get(n, es, e), f, fpcr).expect("checked above")
    })?;
    Ok(Flow::Next)
}

/// One lane of a floating-point two-register misc operation, keyed by
/// (U, size<1>, opcode).
fn misc_op(key: (bool, bool, u32), x: u64, f: Fmt, fpcr: Fpcr) -> Option<Lane> {
    let es = f.width();
    let cmp = |a, b, cmp| Ok(mask_if(fpu::compare_cond(a, b, f, fpcr, cmp), es));
    let rint = |r| Ok(fpu::round_int(x, f, fpcr, r));
    let (u, hi, opc) = key;
    Some(match key {
        (false, true, 0b01100) => cmp(x, 0, Cmp::Gt), // FCMGT #0
        (true, true, 0b01100) => cmp(x, 0, Cmp::Ge),  // FCMGE #0
        (false, true, 0b01101) => cmp(x, 0, Cmp::Eq), // FCMEQ #0
        (true, true, 0b01101) => cmp(0, x, Cmp::Ge),  // FCMLE #0
        (false, true, 0b01110) => cmp(0, x, Cmp::Gt), // FCMLT #0
        (false, true, 0b01111) => Ok(x & !f.sign()),  // FABS
        (true, true, 0b01111) => Ok(x ^ f.sign()),    // FNEG
        (false, false, 0b11000) => rint(Rounding::TieEven), // FRINTN
        (false, true, 0b11000) => rint(Rounding::PosInf), // FRINTP
        (false, false, 0b11001) => rint(Rounding::NegInf), // FRINTM
        (false, true, 0b11001) => rint(Rounding::Zero), // FRINTZ
        (true, false, 0b11000) => rint(Rounding::TieAway), // FRINTA
        (true, _, 0b11001) => rint(fpcr.rmode),       // FRINTX, FRINTI
        // FCVTNS, FCVTPS, FCVTMS, FCVTZS, FCVTAS and the unsigned forms.
        (_, _, 0b11010 | 0b11011) | (_, false, 0b11100) => {
            let r = match (hi, opc) {
                (false, 0b11010) => Rounding::TieEven,
                (true, 0b11010) => Rounding::PosInf,
                (false, 0b11011) => Rounding::NegInf,
                (true, 0b11011) => Rounding::Zero,
                _ => Rounding::TieAway,
            };
            Ok(fpu::to_fixed(x, f, fpcr, 0, u, es, r))
        }
        (_, false, 0b11101) => Ok(fpu::from_fixed(x, es, u, 0, f, fpcr)), // SCVTF, UCVTF
        // URECPE, URSQRTE: 32-bit unsigned fixed-point lanes.
        (false, true, 0b11100) => Ok(fpu::unsigned_recip_estimate(x as u32) as u64),
        (true, true, 0b11100) => Ok(fpu::unsigned_rsqrt_estimate(x as u32) as u64),
        (false, true, 0b11101) => Ok(fpu::recip_estimate(x, f, fpcr)),
        (true, true, 0b11101) => Ok(fpu::rsqrt_estimate(x, f, fpcr)),
        (false, true, 0b11111) => Ok(fpu::recpx(x, f, fpcr)), // scalar only
        (true, true, 0b11111) => fpu::sqrt(x, f, fpcr),
        _ => return None,
    })
}

/// FCVTN, FCVTN2, FCVTL, FCVTL2 and FCVTXN, FCVTXN2.
fn convert_precision(c: &mut Ctx, scalar: bool, u: bool, opc: u32) -> Step {
    let i = c.insn;
    let (q, sz) = (bit(i, 30), bit(i, 22));
    let widen = opc == 0b10111;
    // Only FCVTXN has a scalar form, and it only narrows double precision.
    if (scalar && !u) || (u && !sz) {
        return Err(c.undef());
    }
    if !sz {
        return Err(c.unsupported(Unmodelled::HalfPrecision));
    }
    let fpcr = fpcr(c);
    let n = vreg(c, rn(i));
    if widen {
        let base = if q { 2 } else { 0 };
        let mut r = 0u128;
        for e in 0..2 {
            put(
                &mut r,
                64,
                e,
                fpu::convert(get(n, 32, base + e), Fmt::S, Fmt::D, fpcr),
            );
        }
        write(c, rd(i), r, 128);
        return Ok(Flow::Next);
    }
    let rounding = if u { Rounding::ToOdd } else { fpcr.rmode };
    let narrow = |e| fpu::convert_rounded(get(n, 64, e), Fmt::D, Fmt::S, fpcr, rounding);
    if scalar {
        write(c, rd(i), narrow(0) as u128, 32);
    } else {
        write_narrow(c, rd(i), narrow(0) | (narrow(1) << 32), q);
    }
    Ok(Flow::Next)
}

/// The reduction or pairwise operation for (opcode, size<1>).
fn reduce_op(opc: u32, hi: bool, a: u64, b: u64, f: Fmt, fpcr: Fpcr) -> Option<Lane> {
    Some(match (opc, hi) {
        (0b01100, false) => Ok(fpu::max_min(a, b, f, fpcr, true, true)),
        (0b01100, true) => Ok(fpu::max_min(a, b, f, fpcr, false, true)),
        (0b01111, false) => Ok(fpu::max_min(a, b, f, fpcr, true, false)),
        (0b01111, true) => Ok(fpu::max_min(a, b, f, fpcr, false, false)),
        (0b01101, false) => fpu::arith(Op2::Add, a, b, f, fpcr),
        _ => return None,
    })
}

/// FMAXNMV, FMINNMV, FMAXV, FMINV (4S only).
pub fn across(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (q, u, hi, sz, opc) = (
        bit(i, 30),
        bit(i, 29),
        bit(i, 23),
        bit(i, 22),
        field(i, 16, 12),
    );
    if !u {
        return Err(c.unsupported(Unmodelled::HalfPrecision));
    }
    if !q || sz {
        return Err(c.undef());
    }
    let (f, fpcr) = (Fmt::S, fpcr(c));
    let n = vreg(c, rn(i));
    let e = |k| get(n, 32, k);
    // Reduce() pairs the halves recursively: op(op(e0, e1), op(e2, e3)).
    let step = |a, b| reduce_op(opc, hi, a, b, f, fpcr).expect("FMAX/FMIN forms");
    let r =
        (|| step(step(e(0), e(1))?, step(e(2), e(3))?))().map_err(|what| c.unsupported(what))?;
    write(c, rd(i), r as u128, 32);
    Ok(Flow::Next)
}

/// Scalar FADDP, FMAXP, FMINP, FMAXNMP, FMINNMP over two elements.
pub fn pairwise_scalar(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (hi, sz, opc) = (bit(i, 23), bit(i, 22), field(i, 16, 12));
    let f = fmt(sz);
    let es = f.width();
    let n = vreg(c, rn(i));
    let Some(r) = reduce_op(opc, hi, get(n, es, 0), get(n, es, 1), f, fpcr(c)) else {
        return Err(c.undef());
    };
    let r = r.map_err(|what| c.unsupported(what))?;
    write(c, rd(i), r as u128, es);
    Ok(Flow::Next)
}

/// FMLA, FMLS, FMUL, FMULX by element.
pub fn by_element(c: &mut Ctx, scalar: bool) -> Step {
    let i = c.insn;
    let (q, u, opc) = (bit(i, 30), bit(i, 29), field(i, 15, 12));
    let (hi, sz, l) = (bit(i, 23), bit(i, 22), bit(i, 21));
    if !hi {
        return Err(if sz {
            c.undef()
        } else {
            c.unsupported(Unmodelled::HalfPrecision)
        });
    }
    if sz && (l || (!q && !scalar)) {
        return Err(c.undef());
    }
    let f = fmt(sz);
    let es = f.width();
    let index = if sz {
        field(i, 11, 11)
    } else {
        (field(i, 11, 11) << 1) | field(i, 21, 21)
    };
    let fpcr = fpcr(c);
    let b = get(vreg(c, rm(i)), es, index);
    let (n, d) = (vreg(c, rn(i)), vreg(c, rd(i)));
    let bits = datasize(q, scalar, es);
    lanes(c, bits, es, |e| {
        let a = get(n, es, e);
        match (u, opc) {
            (false, 0b0001) => fpu::mul_add(get(d, es, e), a, b, f, fpcr),
            (false, 0b0101) => fpu::mul_add(get(d, es, e), a ^ f.sign(), b, f, fpcr),
            (false, _) => fpu::arith(Op2::Mul, a, b, f, fpcr),
            (true, _) => fpu::mulx(a, b, f, fpcr),
        }
    })?;
    Ok(Flow::Next)
}

/// SCVTF, UCVTF, FCVTZS, FCVTZU (vector and scalar, fixed point).
pub fn fixed_convert(c: &mut Ctx, scalar: bool, es: u32, fbits: u32) -> Step {
    let i = c.insn;
    let (q, u, to_int) = (bit(i, 30), bit(i, 29), field(i, 15, 11) == 0b11111);
    let f = match es {
        16 => return Err(c.unsupported(Unmodelled::HalfPrecision)),
        32 => Fmt::S,
        64 if q || scalar => Fmt::D,
        _ => return Err(c.undef()),
    };
    let fpcr = fpcr(c);
    let n = vreg(c, rn(i));
    lanes(c, datasize(q, scalar, es), es, |e| {
        let x = get(n, es, e);
        Ok(if to_int {
            fpu::to_fixed(x, f, fpcr, fbits, u, es, Rounding::Zero)
        } else {
            fpu::from_fixed(x, es, u, fbits, f, fpcr)
        })
    })?;
    Ok(Flow::Next)
}
