//! System instructions visible at EL0: hints, barriers, MRS/MSR.

use super::bits::*;
use super::{Ctx, Flow, Step};
use crate::Flags;

/// Pack (op0, op1, CRn, CRm, op2) the way MRS/MSR encode it in bits 20..5.
const fn sysreg(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    ((op0 - 2) << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

const NZCV: u32 = sysreg(3, 3, 4, 2, 0);
const FPCR: u32 = sysreg(3, 3, 4, 4, 0);
const FPSR: u32 = sysreg(3, 3, 4, 4, 1);
const TPIDR_EL0: u32 = sysreg(3, 3, 13, 0, 2);
const TPIDRRO_EL0: u32 = sysreg(3, 3, 13, 0, 3);
const CTR_EL0: u32 = sysreg(3, 3, 0, 0, 1);
const DCZID_EL0: u32 = sysreg(3, 3, 0, 0, 7);
const CNTFRQ_EL0: u32 = sysreg(3, 3, 14, 0, 0);
const CNTPCT_EL0: u32 = sysreg(3, 3, 14, 0, 1);
const CNTVCT_EL0: u32 = sysreg(3, 3, 14, 0, 2);

/// Guest timer frequency reported through CNTFRQ_EL0 (19.2 MHz, a common
/// ARM SoC value; adjust once the real platform value is measured).
pub const COUNTER_HZ: u64 = 19_200_000;

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    let l = bit(i, 21);
    let op0 = field(i, 20, 19);
    let rt = field(i, 4, 0);

    let op1 = field(i, 18, 16);
    let crn = field(i, 15, 12);
    let crm = field(i, 11, 8);
    let op2 = field(i, 7, 5);

    if op0 == 0 {
        if l || op1 != 3 || rt != 31 {
            return Err(c.undef());
        }
        return match (crn, op2) {
            // Hint space (NOP, YIELD, WFE, BTI, PAC hints ...). The Arm ARM
            // defines unallocated or unimplemented hints to execute as NOP.
            (0b0010, _) => Ok(Flow::Next),
            // CLREX clears the exclusive monitor.
            (0b0011, 0b010) => {
                c.s.exclusive_addr = None;
                Ok(Flow::Next)
            }
            // DSB, DMB, ISB, SB: one guest thread, nothing to order.
            (0b0011, 0b100..=0b111) => Ok(Flow::Next),
            // MSR (immediate) PSTATE fields and anything else.
            _ => Err(c.undef()),
        };
    }
    if op0 == 1 {
        // SYS: only the cache maintenance operations EL0 may execute.
        if l || op1 != 3 || crn != 7 || op2 != 1 {
            return Err(c.undef());
        }
        return match crm {
            4 => {
                // DC ZVA: zero a 64-byte block (block size advertised in DCZID_EL0).
                let addr = c.s.xr(rt) & !63;
                c.m.write_bytes(addr, &[0u8; 64]).map_err(|f| c.fault(f))?;
                Ok(Flow::Next)
            }
            // IC IVAU: the interpreter caches no translations. Backends that
            // do must act on it (docs/ROADMAP.md, phase 3).
            5 => Ok(Flow::Next),
            // DC CVAC, CVAU, CVAP, CVADP, CIVAC: no data cache to maintain.
            10..=14 => Ok(Flow::Next),
            _ => Err(c.undef()),
        };
    }

    let reg = field(i, 19, 5);
    if l {
        let v = match reg {
            NZCV => c.s.flags.to_nzcv(),
            FPCR => c.s.fpcr,
            FPSR => c.s.fpsr,
            TPIDR_EL0 => c.s.tpidr_el0,
            TPIDRRO_EL0 => c.s.tpidrro_el0,
            // 64-byte cache lines for I and D caches.
            CTR_EL0 => 0x8444_c004,
            // DC ZVA allowed, 64-byte blocks (log2(words) = 4).
            DCZID_EL0 => 4,
            CNTFRQ_EL0 => COUNTER_HZ,
            // The virtual offset is 0, so both counters read the same clock;
            // libnx's armGetSystemTick reads CNTPCT_EL0.
            CNTPCT_EL0 | CNTVCT_EL0 => c.s.retired,
            _ => return Err(c.undef()),
        };
        c.s.set_xr(rt, v);
    } else {
        let v = c.s.xr(rt);
        match reg {
            NZCV => c.s.flags = Flags::from_nzcv(v),
            FPCR => c.s.fpcr = v,
            FPSR => c.s.fpsr = v,
            TPIDR_EL0 => c.s.tpidr_el0 = v,
            _ => return Err(c.undef()),
        }
    }
    Ok(Flow::Next)
}
