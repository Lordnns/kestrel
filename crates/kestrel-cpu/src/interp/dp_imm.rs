//! Data processing (immediate): ADR/ADRP, ADD/SUB, logical, MOVx, bitfield, EXTR.

use super::bits::*;
use super::{Ctx, Flow, Step};

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    match field(i, 25, 23) {
        0b000 | 0b001 => pc_rel(c),
        0b010 => add_sub_imm(c),
        0b100 => logical_imm(c),
        0b101 => move_wide(c),
        0b110 => bitfield(c),
        0b111 => extract(c),
        _ => Err(c.undef()),
    }
}

fn pc_rel(c: &mut Ctx) -> Step {
    let i = c.insn;
    let rd = field(i, 4, 0);
    let imm = ((field(i, 23, 5) << 2) | field(i, 30, 29)) as u64;
    let off = sext(imm, 21);
    let v = if bit(i, 31) {
        // ADRP: page of PC plus imm * 4 KiB
        (c.pc & !0xfff).wrapping_add((off << 12) as u64)
    } else {
        c.pc.wrapping_add(off as u64)
    };
    c.s.set_xr(rd, v);
    Ok(Flow::Next)
}

fn add_sub_imm(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let sub = bit(i, 30);
    let set_flags = bit(i, 29);
    if bit(i, 22) && field(i, 23, 22) != 0b01 {
        return Err(c.undef());
    }
    let imm = (field(i, 21, 10) as u64) << if bit(i, 22) { 12 } else { 0 };
    let rn = field(i, 9, 5);
    let rd = field(i, 4, 0);
    let a = trunc(c.s.xsp(rn), sf);
    let b = if sub { trunc(!imm, sf) } else { imm };
    let (r, flags) = add_with_carry(a, b, sub, sf);
    if set_flags {
        c.s.flags = flags;
        c.s.set_xr(rd, r);
    } else {
        c.s.set_xsp(rd, r);
    }
    Ok(Flow::Next)
}

fn logical_imm(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let n = bit(i, 22);
    if !sf && n {
        return Err(c.undef());
    }
    let width = if sf { 64 } else { 32 };
    let Some((imm, _)) = decode_bit_masks(n, field(i, 15, 10), field(i, 21, 16), true, width)
    else {
        return Err(c.undef());
    };
    let rn = field(i, 9, 5);
    let rd = field(i, 4, 0);
    let a = trunc(c.s.xr(rn), sf);
    let opc = field(i, 30, 29);
    let r = match opc {
        0b00 | 0b11 => a & imm,
        0b01 => a | imm,
        _ => a ^ imm,
    };
    if opc == 0b11 {
        c.s.flags = logic_flags(r, sf);
        c.s.set_xr(rd, r);
    } else {
        c.s.set_xsp(rd, r);
    }
    Ok(Flow::Next)
}

fn move_wide(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let hw = field(i, 22, 21);
    if !sf && hw > 1 {
        return Err(c.undef());
    }
    let shift = hw * 16;
    let imm = (field(i, 20, 5) as u64) << shift;
    let rd = field(i, 4, 0);
    let v = match field(i, 30, 29) {
        0b00 => trunc(!imm, sf),                            // MOVN
        0b10 => imm,                                        // MOVZ
        0b11 => (c.s.xr(rd) & !(0xffffu64 << shift)) | imm, // MOVK
        _ => return Err(c.undef()),
    };
    c.s.set_xr(rd, trunc(v, sf));
    Ok(Flow::Next)
}

fn bitfield(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let n = bit(i, 22);
    if sf != n {
        return Err(c.undef());
    }
    let width = if sf { 64 } else { 32 };
    let immr = field(i, 21, 16);
    let imms = field(i, 15, 10);
    let Some((wmask, tmask)) = decode_bit_masks(n, imms, immr, false, width) else {
        return Err(c.undef());
    };
    let rn = field(i, 9, 5);
    let rd = field(i, 4, 0);
    let src = trunc(c.s.xr(rn), sf);
    let dst = trunc(c.s.xr(rd), sf);
    let opc = field(i, 30, 29);
    let bot_dst = if opc == 0b01 { dst } else { 0 };
    let bot = (bot_dst & !wmask) | (ror(src, immr, width) & wmask);
    let r = match opc {
        0b00 => {
            // SBFM: fill above the field with the sign bit (bit `imms` of src).
            let top = if (src >> imms) & 1 != 0 {
                ones(width)
            } else {
                0
            };
            (top & !tmask) | (bot & tmask)
        }
        0b01 => (dst & !tmask) | (bot & tmask), // BFM
        0b10 => bot & tmask,                    // UBFM
        _ => return Err(c.undef()),
    };
    c.s.set_xr(rd, trunc(r, sf));
    Ok(Flow::Next)
}

fn extract(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    if sf != bit(i, 22) || field(i, 30, 29) != 0 {
        return Err(c.undef());
    }
    let width = if sf { 64 } else { 32 };
    let lsb = field(i, 15, 10);
    if lsb >= width {
        return Err(c.undef());
    }
    let hi = trunc(c.s.xr(field(i, 9, 5)), sf);
    let lo = trunc(c.s.xr(field(i, 20, 16)), sf);
    let r = if lsb == 0 {
        lo
    } else {
        (lo >> lsb) | (hi << (width - lsb))
    };
    c.s.set_xr(field(i, 4, 0), trunc(r, sf));
    Ok(Flow::Next)
}
