//! Data processing (register): logical/arith shifted and extended,
//! ADC/SBC, conditional compare/select, 1- and 2-source ops, multiply.

use super::bits::*;
use super::{Ctx, Flow, Step};
use crate::Flags;

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    if !bit(i, 28) {
        return match (bit(i, 24), bit(i, 21)) {
            (false, _) => logical_shifted(c),
            (true, false) => add_sub_shifted(c),
            (true, true) => add_sub_extended(c),
        };
    }
    match field(i, 24, 21) {
        0b0000 => adc_sbc(c),
        0b0010 => cond_compare(c),
        0b0100 => cond_select(c),
        0b0110 if bit(i, 30) => one_source(c),
        0b0110 => two_source(c),
        0b1000..=0b1111 => three_source(c),
        _ => Err(c.undef()),
    }
}

fn logical_shifted(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let amount = field(i, 15, 10);
    if !sf && amount >= 32 {
        return Err(c.undef());
    }
    let mut b = shift(c.s.xr(field(i, 20, 16)), field(i, 23, 22), amount, sf);
    if bit(i, 21) {
        b = trunc(!b, sf); // BIC, ORN, EON, BICS
    }
    let a = trunc(c.s.xr(field(i, 9, 5)), sf);
    let opc = field(i, 30, 29);
    let r = match opc {
        0b00 | 0b11 => a & b,
        0b01 => a | b,
        _ => a ^ b,
    };
    if opc == 0b11 {
        c.s.flags = logic_flags(r, sf);
    }
    c.s.set_xr(field(i, 4, 0), r);
    Ok(Flow::Next)
}

fn add_sub_shifted(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let kind = field(i, 23, 22);
    let amount = field(i, 15, 10);
    if kind == 0b11 || (!sf && amount >= 32) {
        return Err(c.undef());
    }
    let b = shift(c.s.xr(field(i, 20, 16)), kind, amount, sf);
    let a = trunc(c.s.xr(field(i, 9, 5)), sf);
    arith(c, sf, a, b, field(i, 4, 0), false);
    Ok(Flow::Next)
}

fn add_sub_extended(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let amount = field(i, 12, 10);
    if amount > 4 || field(i, 23, 22) != 0 {
        return Err(c.undef());
    }
    let b = trunc(
        extend(c.s.xr(field(i, 20, 16)), field(i, 15, 13), amount),
        sf,
    );
    let a = trunc(c.s.xsp(field(i, 9, 5)), sf);
    arith(c, sf, a, b, field(i, 4, 0), true);
    Ok(Flow::Next)
}

/// Shared ADD/ADDS/SUB/SUBS tail. `rd_is_sp`: destination 31 means SP when S=0.
fn arith(c: &mut Ctx, sf: bool, a: u64, b: u64, rd: u32, rd_is_sp: bool) {
    let sub = bit(c.insn, 30);
    let set_flags = bit(c.insn, 29);
    let b = if sub { trunc(!b, sf) } else { b };
    let (r, flags) = add_with_carry(a, b, sub, sf);
    if set_flags {
        c.s.flags = flags;
        c.s.set_xr(rd, r);
    } else if rd_is_sp {
        c.s.set_xsp(rd, r);
    } else {
        c.s.set_xr(rd, r);
    }
}

fn adc_sbc(c: &mut Ctx) -> Step {
    let i = c.insn;
    if field(i, 15, 10) != 0 {
        return Err(c.undef());
    }
    let sf = bit(i, 31);
    let a = trunc(c.s.xr(field(i, 9, 5)), sf);
    let mut b = trunc(c.s.xr(field(i, 20, 16)), sf);
    if bit(i, 30) {
        b = trunc(!b, sf);
    }
    let (r, flags) = add_with_carry(a, b, c.s.flags.c, sf);
    if bit(i, 29) {
        c.s.flags = flags;
    }
    c.s.set_xr(field(i, 4, 0), r);
    Ok(Flow::Next)
}

fn cond_compare(c: &mut Ctx) -> Step {
    let i = c.insn;
    if !bit(i, 29) || bit(i, 10) || bit(i, 4) {
        return Err(c.undef());
    }
    let sf = bit(i, 31);
    if c.s.flags.cond(field(i, 15, 12)) {
        let a = trunc(c.s.xr(field(i, 9, 5)), sf);
        let b = if bit(i, 11) {
            field(i, 20, 16) as u64
        } else {
            trunc(c.s.xr(field(i, 20, 16)), sf)
        };
        let sub = bit(i, 30); // CCMP subtracts, CCMN adds
        let b = if sub { trunc(!b, sf) } else { b };
        c.s.flags = add_with_carry(a, b, sub, sf).1;
    } else {
        c.s.flags = Flags::from_nzcv((field(i, 3, 0) as u64) << 28);
    }
    Ok(Flow::Next)
}

fn cond_select(c: &mut Ctx) -> Step {
    let i = c.insn;
    if bit(i, 29) || bit(i, 11) {
        return Err(c.undef());
    }
    let sf = bit(i, 31);
    let a = c.s.xr(field(i, 9, 5));
    let b = c.s.xr(field(i, 20, 16));
    let r = if c.s.flags.cond(field(i, 15, 12)) {
        a
    } else {
        match (bit(i, 30), bit(i, 10)) {
            (false, false) => b,                // CSEL
            (false, true) => b.wrapping_add(1), // CSINC
            (true, false) => !b,                // CSINV
            (true, true) => b.wrapping_neg(),   // CSNEG
        }
    };
    c.s.set_xr(field(i, 4, 0), trunc(r, sf));
    Ok(Flow::Next)
}

fn two_source(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    let width = if sf { 64 } else { 32 };
    let a = trunc(c.s.xr(field(i, 9, 5)), sf);
    let b = trunc(c.s.xr(field(i, 20, 16)), sf);
    let r = match field(i, 15, 10) {
        0b000010 => a.checked_div(b).unwrap_or(0), // UDIV (x/0 = 0 on ARM)
        0b000011 => {
            // SDIV, with INT_MIN / -1 = INT_MIN as the architecture defines.
            let (sa, sb) = (sext(a, width), sext(b, width));
            if sb == 0 {
                0
            } else {
                sa.wrapping_div(sb) as u64
            }
        }
        0b001000 => shift(a, 0, (b % width as u64) as u32, sf),
        0b001001 => shift(a, 1, (b % width as u64) as u32, sf),
        0b001010 => shift(a, 2, (b % width as u64) as u32, sf),
        0b001011 => shift(a, 3, (b % width as u64) as u32, sf),
        _ => return Err(c.undef()),
    };
    c.s.set_xr(field(i, 4, 0), trunc(r, sf));
    Ok(Flow::Next)
}

fn one_source(c: &mut Ctx) -> Step {
    let i = c.insn;
    if field(i, 20, 16) != 0 || bit(i, 29) {
        return Err(c.undef());
    }
    let sf = bit(i, 31);
    let a = c.s.xr(field(i, 9, 5));
    let r = match (sf, field(i, 15, 10)) {
        (false, 0) => (a as u32).reverse_bits() as u64,
        (true, 0) => a.reverse_bits(),
        // REV16: swap bytes within each halfword.
        (_, 1) => ((a & 0x00ff_00ff_00ff_00ff) << 8) | ((a >> 8) & 0x00ff_00ff_00ff_00ff),
        (false, 2) => (a as u32).swap_bytes() as u64,
        (true, 2) => {
            // REV32: swap bytes within each word.
            let lo = (a as u32).swap_bytes() as u64;
            let hi = ((a >> 32) as u32).swap_bytes() as u64;
            (hi << 32) | lo
        }
        (true, 3) => a.swap_bytes(),
        (false, 4) => (a as u32).leading_zeros() as u64,
        (true, 4) => a.leading_zeros() as u64,
        (false, 5) => {
            let v = a as u32;
            ((v ^ ((v as i32 >> 1) as u32)).leading_zeros() - 1) as u64
        }
        (true, 5) => (a ^ ((a as i64 >> 1) as u64)).leading_zeros() as u64 - 1,
        _ => return Err(c.undef()),
    };
    c.s.set_xr(field(i, 4, 0), trunc(r, sf));
    Ok(Flow::Next)
}

fn three_source(c: &mut Ctx) -> Step {
    let i = c.insn;
    let sf = bit(i, 31);
    if field(i, 30, 29) != 0 {
        return Err(c.undef());
    }
    let n = c.s.xr(field(i, 9, 5));
    let m = c.s.xr(field(i, 20, 16));
    let a = c.s.xr(field(i, 14, 10));
    let sub = bit(i, 15);
    let r = match field(i, 23, 21) {
        0b000 => {
            let p = trunc(n, sf).wrapping_mul(trunc(m, sf));
            if sub {
                a.wrapping_sub(p)
            } else {
                a.wrapping_add(p)
            }
        }
        0b001 if sf => {
            let p = (sext(n, 32) as u64).wrapping_mul(sext(m, 32) as u64);
            if sub {
                a.wrapping_sub(p)
            } else {
                a.wrapping_add(p)
            }
        }
        0b101 if sf => {
            let p = (n & 0xffff_ffff).wrapping_mul(m & 0xffff_ffff);
            if sub {
                a.wrapping_sub(p)
            } else {
                a.wrapping_add(p)
            }
        }
        0b010 if sf && !sub => ((n as i64 as i128 * m as i64 as i128) >> 64) as u64,
        0b110 if sf && !sub => ((n as u128 * m as u128) >> 64) as u64,
        _ => return Err(c.undef()),
    };
    c.s.set_xr(field(i, 4, 0), trunc(r, sf));
    Ok(Flow::Next)
}
