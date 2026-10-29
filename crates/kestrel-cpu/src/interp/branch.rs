//! Branches, exception generation and system instructions.

use super::bits::*;
use super::{system, Ctx, Flow, Step};
use crate::Exit;

pub fn exec(c: &mut Ctx) -> Step {
    let i = c.insn;
    // B / BL
    if field(i, 30, 26) == 0b00101 {
        let off = sext((field(i, 25, 0) as u64) << 2, 28);
        if bit(i, 31) {
            c.s.x[30] = c.pc.wrapping_add(4);
        }
        return Ok(Flow::Jump(c.pc.wrapping_add(off as u64)));
    }
    // CBZ / CBNZ
    if field(i, 30, 25) == 0b011010 {
        let sf = bit(i, 31);
        let v = trunc(c.s.xr(field(i, 4, 0)), sf);
        let take = (v == 0) != bit(i, 24);
        return Ok(cond_jump(c, take, field(i, 23, 5), 19));
    }
    // TBZ / TBNZ
    if field(i, 30, 25) == 0b011011 {
        let b = ((bit(i, 31) as u32) << 5) | field(i, 23, 19);
        let set = (c.s.xr(field(i, 4, 0)) >> b) & 1 != 0;
        let take = set == bit(i, 24);
        return Ok(cond_jump(c, take, field(i, 18, 5), 14));
    }
    // B.cond
    if field(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
        let take = c.s.flags.cond(field(i, 3, 0));
        return Ok(cond_jump(c, take, field(i, 23, 5), 19));
    }
    // Exception generation: SVC / HVC / SMC / BRK / HLT
    if field(i, 31, 24) == 0b1101_0100 {
        let imm = field(i, 20, 5) as u16;
        return match (field(i, 23, 21), field(i, 4, 0)) {
            (0b000, 0b00001) => Err(Exit::Svc(imm)),
            (0b001, 0b00000) => Err(Exit::Breakpoint(imm)),
            _ => Err(c.undef()),
        };
    }
    // System: hints, barriers, MRS/MSR
    if field(i, 31, 22) == 0b11_0101_0100 {
        return system::exec(c);
    }
    // Unconditional branch (register): BR / BLR / RET
    if field(i, 31, 25) == 0b110_1011 {
        let opc = field(i, 24, 21);
        // Only the plain forms: op2 == 0b11111, op3 == 0, op4 == 0.
        if field(i, 20, 16) != 0b11111 || field(i, 15, 10) != 0 || field(i, 4, 0) != 0 {
            return Err(c.undef());
        }
        let target = c.s.xr(field(i, 9, 5));
        return match opc {
            0b0000 | 0b0010 => Ok(Flow::Jump(target)), // BR, RET
            0b0001 => {
                c.s.x[30] = c.pc.wrapping_add(4); // BLR
                Ok(Flow::Jump(target))
            }
            _ => Err(c.undef()),
        };
    }
    Err(c.undef())
}

#[inline]
fn cond_jump(c: &Ctx, take: bool, imm: u32, bits: u32) -> Flow {
    if take {
        let off = sext((imm as u64) << 2, bits + 2);
        Flow::Jump(c.pc.wrapping_add(off as u64))
    } else {
        Flow::Next
    }
}
