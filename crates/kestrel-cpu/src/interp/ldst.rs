//! Loads and stores: single registers (all addressing modes), pairs,
//! literals, exclusives/acquire-release, compare-and-swap and LSE atomics.
//! Advanced SIMD structure loads and stores are in `neon/mem.rs`.

use super::bits::*;
use super::{Ctx, Flow, Step};

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    if field(i, 29, 24) == 0b00_1000 {
        return exclusive(c);
    }
    match field(i, 29, 27) {
        0b011 if field(i, 25, 24) == 0 => literal(c),
        0b101 if !bit(i, 25) => pair(c),
        0b111 if bit(i, 24) => {
            // Unsigned scaled 12-bit offset.
            let (bytes, _) = access_size(c)?;
            let off = (field(i, 21, 10) as u64) * bytes as u64;
            let addr = c.s.xsp(field(i, 9, 5)).wrapping_add(off);
            single(c, addr)
        }
        0b111 if !bit(i, 25) => {
            if !bit(i, 21) {
                imm9(c)
            } else if field(i, 11, 10) == 0b10 {
                reg_offset(c)
            } else if field(i, 11, 10) == 0b00 && !bit(i, 26) {
                atomic(c)
            } else {
                Err(c.undef())
            }
        }
        // Advanced SIMD structures: LD1-LD4, ST1-ST4, LD1R-LD4R.
        0b001 if bit(i, 26) && !bit(i, 25) && !bit(i, 31) => super::neon::load_store(c),
        _ => Err(c.undef()),
    }
}

/// Size in bytes and whether the access targets a SIMD/FP register.
fn access_size(c: &Ctx) -> Result<(u32, bool), crate::Exit> {
    let i = c.insn;
    let size = field(i, 31, 30);
    let simd = bit(i, 26);
    if simd && size == 0 && bit(i, 23) {
        Ok((16, true)) // Q register
    } else {
        Ok((1 << size, simd))
    }
}

/// Load/store one register at `addr`, using size/opc/V from the instruction.
fn single(c: &mut Ctx, addr: u64) -> Step {
    let i = c.insn;
    let rt = field(i, 4, 0);
    let size = field(i, 31, 30);
    let opc = field(i, 23, 22);
    if bit(i, 26) {
        let (bytes, _) = access_size(c)?;
        if bytes != 16 && opc >> 1 != 0 {
            return Err(c.undef());
        }
        return if opc & 1 == 0 {
            store_v(c, addr, rt, bytes)
        } else {
            load_v(c, addr, rt, bytes)
        };
    }
    match opc {
        0b00 => {
            let v = c.s.xr(rt);
            store(c, addr, v, 1 << size)?;
        }
        0b01 => {
            let v = load(c, addr, 1 << size)?;
            c.s.set_xr(rt, v);
        }
        0b10 if size == 3 => {} // PRFM: prefetch hint, no-op
        0b10 => {
            let v = load(c, addr, 1 << size)?;
            c.s.set_xr(rt, sext(v, 8 << size) as u64);
        }
        0b11 if size < 2 => {
            let v = load(c, addr, 1 << size)?;
            c.s.set_xr(rt, sext(v, 8 << size) as u64 & 0xffff_ffff);
        }
        _ => return Err(c.undef()),
    }
    Ok(Flow::Next)
}

/// Unscaled (LDUR/STUR), post-index and pre-index 9-bit offset forms.
fn imm9(c: &mut Ctx) -> Step {
    let i = c.insn;
    let rn = field(i, 9, 5);
    let off = sext(field(i, 20, 12) as u64, 9) as u64;
    let base = c.s.xsp(rn);
    match field(i, 11, 10) {
        0b00 | 0b10 => single(c, base.wrapping_add(off)), // LDUR / unprivileged
        0b01 => {
            single(c, base)?; // post-index
            c.s.set_xsp(rn, base.wrapping_add(off));
            Ok(Flow::Next)
        }
        _ => {
            let a = base.wrapping_add(off); // pre-index
            single(c, a)?;
            c.s.set_xsp(rn, a);
            Ok(Flow::Next)
        }
    }
}

fn reg_offset(c: &mut Ctx) -> Step {
    let i = c.insn;
    let option = field(i, 15, 13);
    if option & 0b010 == 0 {
        return Err(c.undef());
    }
    let (bytes, _) = access_size(c)?;
    let amount = if bit(i, 12) {
        bytes.trailing_zeros()
    } else {
        0
    };
    let off = extend(c.s.xr(field(i, 20, 16)), option, amount);
    let addr = c.s.xsp(field(i, 9, 5)).wrapping_add(off);
    single(c, addr)
}

fn literal(c: &mut Ctx) -> Step {
    let i = c.insn;
    let rt = field(i, 4, 0);
    let addr =
        c.pc.wrapping_add(sext((field(i, 23, 5) as u64) << 2, 21) as u64);
    let opc = field(i, 31, 30);
    if bit(i, 26) {
        let bytes = match opc {
            0 => 4,
            1 => 8,
            2 => 16,
            _ => return Err(c.undef()),
        };
        return load_v(c, addr, rt, bytes);
    }
    match opc {
        0 => {
            let v = load(c, addr, 4)?;
            c.s.set_xr(rt, v);
        }
        1 => {
            let v = load(c, addr, 8)?;
            c.s.set_xr(rt, v);
        }
        2 => {
            let v = load(c, addr, 4)?;
            c.s.set_xr(rt, sext(v, 32) as u64);
        }
        _ => {} // PRFM literal
    }
    Ok(Flow::Next)
}

fn pair(c: &mut Ctx) -> Step {
    let i = c.insn;
    let opc = field(i, 31, 30);
    let simd = bit(i, 26);
    let load_op = bit(i, 22);
    let rt = field(i, 4, 0);
    let rt2 = field(i, 14, 10);
    let rn = field(i, 9, 5);
    let bytes: u32 = if simd {
        match opc {
            0 => 4,
            1 => 8,
            2 => 16,
            _ => return Err(c.undef()),
        }
    } else {
        match opc {
            0 | 1 => 4, // opc 01 with L=1 is LDPSW
            2 => 8,
            _ => return Err(c.undef()),
        }
    };
    if !simd && opc == 1 && !load_op {
        return Err(c.undef());
    }
    let off = (sext(field(i, 21, 15) as u64, 7) * bytes as i64) as u64;
    let base = c.s.xsp(rn);
    let mode = field(i, 24, 23);
    let addr = if mode == 0b01 {
        base
    } else {
        base.wrapping_add(off)
    };
    let addr2 = addr.wrapping_add(bytes as u64);

    if simd {
        if load_op {
            load_v(c, addr, rt, bytes)?;
            load_v(c, addr2, rt2, bytes)?;
        } else {
            store_v(c, addr, rt, bytes)?;
            store_v(c, addr2, rt2, bytes)?;
        }
    } else if load_op {
        let a = load(c, addr, bytes)?;
        let b = load(c, addr2, bytes)?;
        let fix = |v: u64| if opc == 1 { sext(v, 32) as u64 } else { v };
        c.s.set_xr(rt, fix(a));
        c.s.set_xr(rt2, fix(b));
    } else {
        let (a, b) = (c.s.xr(rt), c.s.xr(rt2));
        store(c, addr, a, bytes)?;
        store(c, addr2, b, bytes)?;
    }
    match mode {
        0b01 => c.s.set_xsp(rn, base.wrapping_add(off)),
        0b11 => c.s.set_xsp(rn, addr),
        _ => {}
    }
    Ok(Flow::Next)
}

/// LDXR/STXR, LDAXR/STLXR, LDAR/STLR, LDXP/STXP and CAS.
///
/// The interpreter runs one guest thread at a time, so the exclusive monitor
/// only needs to remember the last LDXR address; a store-exclusive succeeds
/// if it targets that address. Multi-core scheduling will tighten this.
fn exclusive(c: &mut Ctx) -> Step {
    let i = c.insn;
    let size = field(i, 31, 30);
    let bytes = 1u32 << size;
    let o2 = bit(i, 23);
    let l = bit(i, 22);
    let o1 = bit(i, 21);
    let rs = field(i, 20, 16);
    let rt2 = field(i, 14, 10);
    let rn = field(i, 9, 5);
    let rt = field(i, 4, 0);
    let addr = c.s.xsp(rn);

    match (o2, o1) {
        // Load-acquire / store-release (non-exclusive).
        (true, false) => {
            if l {
                let v = load(c, addr, bytes)?;
                c.s.set_xr(rt, v);
            } else {
                let v = c.s.xr(rt);
                store(c, addr, v, bytes)?;
            }
        }
        // Single-register exclusive.
        (false, false) => {
            if l {
                let v = load(c, addr, bytes)?;
                c.s.set_xr(rt, v);
                c.s.exclusive_addr = Some(addr);
            } else {
                let ok = c.s.exclusive_addr == Some(addr);
                if ok {
                    let v = c.s.xr(rt);
                    store(c, addr, v, bytes)?;
                }
                c.s.exclusive_addr = None;
                c.s.set_xr(rs, (!ok) as u64);
            }
        }
        // Pair exclusive (sizes 32 and 64 only).
        (false, true) => {
            if size < 2 {
                return Err(c.undef());
            }
            if l {
                let a = load(c, addr, bytes)?;
                let b = load(c, addr + bytes as u64, bytes)?;
                c.s.set_xr(rt, a);
                c.s.set_xr(rt2, b);
                c.s.exclusive_addr = Some(addr);
            } else {
                let ok = c.s.exclusive_addr == Some(addr);
                if ok {
                    let (a, b) = (c.s.xr(rt), c.s.xr(rt2));
                    store(c, addr, a, bytes)?;
                    store(c, addr + bytes as u64, b, bytes)?;
                }
                c.s.exclusive_addr = None;
                c.s.set_xr(rs, (!ok) as u64);
            }
        }
        // CAS / CASA / CASL / CASAL (single register; CASP not yet).
        (true, true) => {
            if rt2 != 0b11111 {
                return Err(c.undef());
            }
            let old = load(c, addr, bytes)?;
            let mask = ones(bytes * 8);
            if old == c.s.xr(rs) & mask {
                let v = c.s.xr(rt);
                store(c, addr, v, bytes)?;
            }
            c.s.set_xr(rs, old);
        }
    }
    Ok(Flow::Next)
}

/// ARMv8.1 LSE atomics: LDADD, LDCLR, LDEOR, LDSET, LD{S,U}{MAX,MIN}, SWP.
fn atomic(c: &mut Ctx) -> Step {
    let i = c.insn;
    let bytes = 1u32 << field(i, 31, 30);
    let bits = bytes * 8;
    let rs = field(i, 20, 16);
    let o3 = bit(i, 15);
    let opc = field(i, 14, 12);
    let rt = field(i, 4, 0);
    let addr = c.s.xsp(field(i, 9, 5));
    let mask = ones(bits);
    let old = load(c, addr, bytes)?;
    let val = c.s.xr(rs) & mask;
    let s = |v: u64| sext(v, bits);
    let new = match (o3, opc) {
        (false, 0b000) => old.wrapping_add(val),
        (false, 0b001) => old & !val,
        (false, 0b010) => old ^ val,
        (false, 0b011) => old | val,
        (false, 0b100) => {
            if s(old) >= s(val) {
                old
            } else {
                val
            }
        }
        (false, 0b101) => {
            if s(old) <= s(val) {
                old
            } else {
                val
            }
        }
        (false, 0b110) => old.max(val),
        (false, 0b111) => old.min(val),
        (true, 0b000) => val,
        _ => return Err(c.undef()),
    } & mask;
    store(c, addr, new, bytes)?;
    c.s.set_xr(rt, old);
    Ok(Flow::Next)
}

// ---- memory helpers -------------------------------------------------------

fn load(c: &mut Ctx, addr: u64, bytes: u32) -> Result<u64, crate::Exit> {
    let r = match bytes {
        1 => c.m.read8(addr).map(u64::from),
        2 => c.m.read16(addr).map(u64::from),
        4 => c.m.read32(addr).map(u64::from),
        _ => c.m.read64(addr),
    };
    r.map_err(|f| c.fault(f))
}

fn store(c: &mut Ctx, addr: u64, v: u64, bytes: u32) -> Result<(), crate::Exit> {
    let r = match bytes {
        1 => c.m.write8(addr, v as u8),
        2 => c.m.write16(addr, v as u16),
        4 => c.m.write32(addr, v as u32),
        _ => c.m.write64(addr, v),
    };
    r.map_err(|f| c.fault(f))
}

fn load_v(c: &mut Ctx, addr: u64, rt: u32, bytes: u32) -> Step {
    let v = if bytes == 16 {
        c.m.read128(addr).map_err(|f| c.fault(f))?
    } else {
        load(c, addr, bytes)? as u128
    };
    // Writing a scalar FP/SIMD register zeroes the upper bits.
    c.s.v[rt as usize] = v;
    Ok(Flow::Next)
}

fn store_v(c: &mut Ctx, addr: u64, rt: u32, bytes: u32) -> Step {
    let v = c.s.v[rt as usize];
    if bytes == 16 {
        c.m.write128(addr, v).map_err(|f| c.fault(f))?;
    } else {
        store(c, addr, v as u64, bytes)?;
    }
    Ok(Flow::Next)
}
