//! Scalar floating point: single and double precision.
//!
//! Decoding follows the "Data Processing -- Scalar Floating-Point and
//! Advanced SIMD" encoding group; the arithmetic lives in [`crate::float`],
//! which implements the Arm pseudocode. Writing a scalar result clears the
//! rest of the 128-bit register. Half precision stops with an explicit
//! unsupported error.

use super::bits::*;
use super::{Ctx, Flow, Step};
use crate::float::{self, Fmt, Fpcr, Op2, Rounding};
use crate::{Exit, Flags, Unmodelled};

/// Scalar floating-point group: bit 30 clear, bits 28..25 = 0b1111.
/// Kept out of line so the integer fast path stays compact.
#[inline(never)]
pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    if bit(i, 29) {
        return Err(c.undef());
    }
    if bit(i, 24) {
        return three_source(c);
    }
    if !bit(i, 21) {
        return convert_fixed(c);
    }
    if field(i, 15, 10) == 0 {
        return convert_int(c);
    }
    if bit(i, 31) {
        return Err(c.undef());
    }
    if field(i, 14, 10) == 0b10000 {
        return one_source(c);
    }
    if field(i, 13, 10) == 0b1000 {
        return compare(c);
    }
    if field(i, 12, 10) == 0b100 {
        return fmov_imm(c);
    }
    match field(i, 11, 10) {
        0b01 => cond_compare(c),
        0b10 => two_source(c),
        _ => cond_select(c),
    }
}

/// The ftype field (bits 23..22).
fn ftype(c: &Ctx) -> Result<Fmt, Exit> {
    match field(c.insn, 23, 22) {
        0b00 => Ok(Fmt::S),
        0b01 => Ok(Fmt::D),
        0b11 => Err(c.unsupported(Unmodelled::HalfPrecision)),
        _ => Err(c.undef()),
    }
}

fn fpcr(c: &Ctx) -> Fpcr {
    Fpcr::from_bits(c.s.fpcr)
}

fn read(c: &Ctx, n: u32, f: Fmt) -> u64 {
    c.s.v[n as usize] as u64 & f.mask()
}

/// Write a scalar result, zeroing the upper bits of the vector register.
fn write(c: &mut Ctx, n: u32, v: u64) {
    c.s.v[n as usize] = v as u128;
}

fn rn(c: &Ctx) -> u32 {
    field(c.insn, 9, 5)
}
fn rm(c: &Ctx) -> u32 {
    field(c.insn, 20, 16)
}
fn rd(c: &Ctx) -> u32 {
    field(c.insn, 4, 0)
}

fn two_source(c: &mut Ctx) -> Step {
    let f = ftype(c)?;
    let (a, b, fpcr) = (read(c, rn(c), f), read(c, rm(c), f), fpcr(c));
    let r = match field(c.insn, 15, 12) {
        0b0000 => float::arith(Op2::Mul, a, b, f, fpcr),
        0b0001 => float::arith(Op2::Div, a, b, f, fpcr),
        0b0010 => float::arith(Op2::Add, a, b, f, fpcr),
        0b0011 => float::arith(Op2::Sub, a, b, f, fpcr),
        0b0100 => Ok(float::max_min(a, b, f, fpcr, true, false)),
        0b0101 => Ok(float::max_min(a, b, f, fpcr, false, false)),
        0b0110 => Ok(float::max_min(a, b, f, fpcr, true, true)),
        0b0111 => Ok(float::max_min(a, b, f, fpcr, false, true)),
        // FNMUL negates the product, NaN results included.
        0b1000 => float::arith(Op2::Mul, a, b, f, fpcr).map(|r| r ^ f.sign()),
        _ => return Err(c.undef()),
    };
    let r = r.map_err(|what| c.unsupported(what))?;
    write(c, rd(c), r);
    Ok(Flow::Next)
}

/// FMADD, FMSUB, FNMADD, FNMSUB. The negations happen before NaN
/// processing, as in the pseudocode, so they also flip a propagated NaN.
fn three_source(c: &mut Ctx) -> Step {
    let i = c.insn;
    if bit(i, 31) {
        return Err(c.undef());
    }
    let f = ftype(c)?;
    let (n, m, a) = (
        read(c, rn(c), f),
        read(c, rm(c), f),
        read(c, field(i, 14, 10), f),
    );
    let neg = f.sign();
    let (a, n) = match (bit(i, 21), bit(i, 15)) {
        (false, false) => (a, n),            // FMADD:   a + n*m
        (false, true) => (a, n ^ neg),       // FMSUB:   a - n*m
        (true, false) => (a ^ neg, n ^ neg), // FNMADD: -a - n*m
        (true, true) => (a ^ neg, n),        // FNMSUB: -a + n*m
    };
    let r = float::mul_add(a, n, m, f, fpcr(c)).map_err(|what| c.unsupported(what))?;
    write(c, rd(c), r);
    Ok(Flow::Next)
}

fn one_source(c: &mut Ctx) -> Step {
    let f = ftype(c)?;
    let opc = field(c.insn, 20, 15);
    let (x, fpcr) = (read(c, rn(c), f), fpcr(c));
    let r = match opc {
        0b000000 => x,             // FMOV
        0b000001 => x & !f.sign(), // FABS
        0b000010 => x ^ f.sign(),  // FNEG
        0b000011 => float::sqrt(x, f, fpcr).map_err(|what| c.unsupported(what))?,
        0b000100 | 0b000101 => {
            let to = if opc == 0b000100 { Fmt::S } else { Fmt::D };
            if to == f {
                return Err(c.undef());
            }
            float::convert(x, f, to, fpcr)
        }
        0b000111 => return Err(c.unsupported(Unmodelled::HalfPrecision)),
        0b001000..=0b001111 if opc != 0b001101 => {
            let rounding = match opc & 7 {
                0 => Rounding::TieEven, // FRINTN
                1 => Rounding::PosInf,  // FRINTP
                2 => Rounding::NegInf,  // FRINTM
                3 => Rounding::Zero,    // FRINTZ
                4 => Rounding::TieAway, // FRINTA
                _ => fpcr.rmode,        // FRINTX, FRINTI
            };
            float::round_int(x, f, fpcr, rounding)
        }
        _ => return Err(c.undef()),
    };
    write(c, rd(c), r);
    Ok(Flow::Next)
}

/// FCMP, FCMPE (the signalling difference only affects exception flags).
fn compare(c: &mut Ctx) -> Step {
    let i = c.insn;
    if field(i, 15, 14) != 0 || field(i, 2, 0) != 0 {
        return Err(c.undef());
    }
    let f = ftype(c)?;
    let a = read(c, rn(c), f);
    let b = if bit(i, 3) { 0 } else { read(c, rm(c), f) }; // #0.0 form
    c.s.flags = Flags::from_nzcv(float::compare(a, b, f, fpcr(c)));
    Ok(Flow::Next)
}

/// FCCMP, FCCMPE.
fn cond_compare(c: &mut Ctx) -> Step {
    let i = c.insn;
    let f = ftype(c)?;
    c.s.flags = if c.s.flags.cond(field(i, 15, 12)) {
        let (a, b) = (read(c, rn(c), f), read(c, rm(c), f));
        Flags::from_nzcv(float::compare(a, b, f, fpcr(c)))
    } else {
        Flags::from_nzcv((field(i, 3, 0) as u64) << 28)
    };
    Ok(Flow::Next)
}

fn cond_select(c: &mut Ctx) -> Step {
    let f = ftype(c)?;
    let pick = if c.s.flags.cond(field(c.insn, 15, 12)) {
        rn(c)
    } else {
        rm(c)
    };
    let v = read(c, pick, f);
    write(c, rd(c), v);
    Ok(Flow::Next)
}

fn fmov_imm(c: &mut Ctx) -> Step {
    if field(c.insn, 9, 5) != 0 {
        return Err(c.undef());
    }
    let f = ftype(c)?;
    let v = float::expand_imm(field(c.insn, 20, 13), f);
    write(c, rd(c), v);
    Ok(Flow::Next)
}

/// Conversions between floating point and integers, and FMOV general.
fn convert_int(c: &mut Ctx) -> Step {
    let i = c.insn;
    let (rmode, opcode) = (field(i, 20, 19), field(i, 18, 16));
    if opcode >= 0b110 {
        return fmov_general(c);
    }
    let f = ftype(c)?;
    let width = if bit(i, 31) { 64 } else { 32 };
    let fpcr = fpcr(c);
    match (rmode, opcode) {
        // FCVT{N,P,M,Z}{S,U} and FCVTA{S,U}.
        (_, 0b000 | 0b001) | (0b00, 0b100 | 0b101) => {
            let rounding = if opcode >= 0b100 {
                Rounding::TieAway
            } else {
                Rounding::from_rmode(rmode)
            };
            let x = read(c, rn(c), f);
            let v = float::to_fixed(x, f, fpcr, 0, opcode & 1 == 1, width, rounding);
            c.s.set_xr(rd(c), v);
        }
        // SCVTF, UCVTF (rounded by FPCR).
        (0b00, 0b010 | 0b011) => {
            let v = float::from_fixed(c.s.xr(rn(c)), width, opcode == 0b011, 0, f, fpcr);
            write(c, rd(c), v);
        }
        _ => return Err(c.undef()),
    }
    Ok(Flow::Next)
}

/// Conversions between floating point and fixed point (bits 15..10 hold
/// 64 - fbits).
fn convert_fixed(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let scale = field(i, 15, 10);
    if !sf && scale < 32 {
        return Err(c.undef());
    }
    let f = ftype(c)?;
    let (width, fbits, fpcr) = (if sf { 64 } else { 32 }, 64 - scale, fpcr(c));
    match (field(i, 20, 19), field(i, 18, 16)) {
        (0b11, opcode @ (0b000 | 0b001)) => {
            let x = read(c, rn(c), f);
            let v = float::to_fixed(x, f, fpcr, fbits, opcode == 1, width, Rounding::Zero);
            c.s.set_xr(rd(c), v);
        }
        (0b00, opcode @ (0b010 | 0b011)) => {
            let v = float::from_fixed(c.s.xr(rn(c)), width, opcode == 0b011, fbits, f, fpcr);
            write(c, rd(c), v);
        }
        _ => return Err(c.undef()),
    }
    Ok(Flow::Next)
}

/// FMOV between general and floating-point registers.
fn fmov_general(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let ftype = field(i, 23, 22);
    let rmode = field(i, 20, 19);
    let opcode = field(i, 18, 16);
    let (rn, rd) = (rn(c), rd(c));
    match (sf, ftype, rmode, opcode) {
        (false, 0b00, 0b00, 0b110) => c.s.set_xr(rd, c.s.v[rn as usize] as u32 as u64), // FMOV Wd, Sn
        (false, 0b00, 0b00, 0b111) => c.s.v[rd as usize] = c.s.xr(rn) as u32 as u128, // FMOV Sd, Wn
        (true, 0b01, 0b00, 0b110) => c.s.set_xr(rd, c.s.v[rn as usize] as u64),       // FMOV Xd, Dn
        (true, 0b01, 0b00, 0b111) => c.s.v[rd as usize] = c.s.xr(rn) as u128,         // FMOV Dd, Xn
        (true, 0b10, 0b01, 0b110) => c.s.set_xr(rd, (c.s.v[rn as usize] >> 64) as u64), // FMOV Xd, Vn.D[1]
        (true, 0b10, 0b01, 0b111) => {
            // FMOV Vd.D[1], Xn: upper half only, lower half preserved.
            let lo = c.s.v[rd as usize] & u64::MAX as u128;
            c.s.v[rd as usize] = lo | ((c.s.xr(rn) as u128) << 64);
        }
        (_, 0b11, _, _) => return Err(c.unsupported(Unmodelled::HalfPrecision)),
        _ => return Err(c.undef()),
    }
    Ok(Flow::Next)
}

/// Scalar forms of the reciprocal helpers in the Advanced SIMD scalar
/// groups: FRECPE, FRSQRTE, FRECPX (two-register misc) and FRECPS, FRSQRTS
/// (three same). Bits 31..30 = 0b01, bits 28..24 = 0b11110.
#[inline(never)]
pub fn scalar_simd(c: &mut Ctx) -> Step {
    let i = c.insn;
    let f = if bit(i, 22) { Fmt::D } else { Fmt::S };
    let u = bit(i, 29);
    let (x, fpcr) = (read(c, rn(c), f), fpcr(c));
    let r = if field(i, 21, 17) == 0b10000 && field(i, 11, 10) == 0b10 && bit(i, 23) {
        match (u, field(i, 16, 12)) {
            (false, 0b11101) => float::recip_estimate(x, f, fpcr),
            (true, 0b11101) => float::rsqrt_estimate(x, f, fpcr),
            (false, 0b11111) => float::recpx(x, f, fpcr),
            _ => return Err(c.undef()),
        }
    } else if bit(i, 21) && bit(i, 10) && field(i, 15, 11) == 0b11111 && !u {
        let y = read(c, rm(c), f);
        float::recip_step(x, y, f, fpcr, bit(i, 23)).map_err(|what| c.unsupported(what))?
    } else {
        return Err(c.undef());
    };
    write(c, rd(c), r);
    Ok(Flow::Next)
}
