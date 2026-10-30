//! Advanced SIMD structure loads and stores: LD1-LD4 and ST1-ST4 (multiple
//! structures and single lane), LD1R-LD4R, with immediate and register
//! post-increment.
//!
//! Loads gather into a copy of the register file and commit only when every
//! access succeeded, so a fault leaves the registers unchanged.

use super::super::bits::*;
use super::super::{Ctx, Flow, Step};
use super::*;

/// Bits 31 = 0, 29..25 = 0b00110 (called from the load/store decoder).
pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    let post = bit(i, 23);
    if !post && field(i, 20, 16) != 0 {
        return Err(c.undef());
    }
    let n = rn(i);
    let base = c.s.xsp(n);
    let bytes = if bit(i, 24) {
        single(c, base)?
    } else {
        multiple(c, base)?
    };
    if post {
        let m = rm(i);
        let off = if m == 31 { bytes } else { c.s.xr(m) };
        c.s.set_xsp(n, base.wrapping_add(off));
    }
    Ok(Flow::Next)
}

/// LD1-LD4 / ST1-ST4 (multiple structures). Returns the bytes transferred.
fn multiple(c: &mut Ctx, mut addr: u64) -> Result<u64, crate::Exit> {
    let i = c.insn;
    if bit(i, 21) {
        return Err(c.undef());
    }
    let (q, load, size) = (bit(i, 30), bit(i, 22), field(i, 11, 10));
    // (registers repeated, elements per structure)
    let (rpt, selem) = match field(i, 15, 12) {
        0b0000 => (1, 4),
        0b0010 => (4, 1),
        0b0100 => (1, 3),
        0b0110 => (3, 1),
        0b0111 => (1, 1),
        0b1000 => (1, 2),
        0b1010 => (2, 1),
        _ => return Err(c.undef()),
    };
    if size == 3 && !q && selem > 1 {
        return Err(c.undef());
    }
    let es = 8 << size;
    let bits = datasize(q, false, es);
    let t = rd(i);
    let start = addr;
    let mut regs = c.s.v;
    if load {
        // The whole register is written: a 64-bit form clears the top half.
        for k in 0..rpt * selem {
            regs[((t + k) % 32) as usize] = 0;
        }
    }
    for r in 0..rpt {
        for e in 0..bits / es {
            for s in 0..selem {
                let reg = &mut regs[((t + r + s) % 32) as usize];
                transfer(c, addr, reg, es, e, load)?;
                addr = addr.wrapping_add(es as u64 / 8);
            }
        }
    }
    if load {
        c.s.v = regs;
    }
    Ok(addr.wrapping_sub(start))
}

/// Single-lane LD1-LD4 / ST1-ST4 and the replicating LD1R-LD4R.
fn single(c: &mut Ctx, mut addr: u64) -> Result<u64, crate::Exit> {
    let i = c.insn;
    let (q, load, s, size) = (bit(i, 30), bit(i, 22), field(i, 12, 12), field(i, 11, 10));
    let opc = field(i, 15, 13);
    let selem = ((opc & 1) << 1 | field(i, 21, 21)) + 1;
    let qv = q as u32;
    let (es, index, replicate) = match opc >> 1 {
        0b00 => (8, qv << 3 | s << 2 | size, false),
        0b01 if size & 1 == 0 => (16, qv << 2 | s << 1 | size >> 1, false),
        0b10 if size == 0 => (32, qv << 1 | s, false),
        0b10 if size == 1 && s == 0 => (64, qv, false),
        0b11 if load && s == 0 => (8 << size, 0, true),
        _ => return Err(c.undef()),
    };
    let t = rd(i);
    let start = addr;
    let mut regs = c.s.v;
    for k in 0..selem {
        let reg = &mut regs[((t + k) % 32) as usize];
        if replicate {
            let mut one = 0u128;
            transfer(c, addr, &mut one, es, 0, true)?;
            let bits = datasize(q, false, es);
            *reg = 0;
            for e in 0..bits / es {
                put(reg, es, e, one as u64);
            }
        } else {
            transfer(c, addr, reg, es, index, load)?;
        }
        addr = addr.wrapping_add(es as u64 / 8);
    }
    if load {
        c.s.v = regs;
    }
    Ok(addr.wrapping_sub(start))
}

/// Load or store element `e` of `es` bits of `reg` at `addr`.
fn transfer(c: &mut Ctx, addr: u64, reg: &mut u128, es: u32, e: u32, load: bool) -> Step {
    let r = if load {
        let v = match es {
            8 => c.m.read8(addr).map(u64::from),
            16 => c.m.read16(addr).map(u64::from),
            32 => c.m.read32(addr).map(u64::from),
            _ => c.m.read64(addr),
        };
        v.map(|v| put(reg, es, e, v))
    } else {
        let v = get(*reg, es, e);
        match es {
            8 => c.m.write8(addr, v as u8),
            16 => c.m.write16(addr, v as u16),
            32 => c.m.write32(addr, v as u32),
            _ => c.m.write64(addr, v),
        }
    };
    r.map_err(|f| c.fault(f))?;
    Ok(Flow::Next)
}
