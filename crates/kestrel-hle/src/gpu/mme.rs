//! The macro method expander (MME): small programs a program uploads to the
//! 3D engine (LOAD_MME_INSTRUCTION_RAM, LOAD_MME_START_ADDRESS_RAM) and calls
//! with CALL_MME_MACRO(i), each call's parameters following as
//! CALL_MME_DATA(i). A macro reads parameters and method registers, computes,
//! and sends method writes to the 3D engine.
//!
//! The instruction set of Fermi through Maxwell is not in NVIDIA's
//! documentation. Sources, both MIT: envytools' macro disassembler
//! (`envydis/macro.c`, with a description of the unit) and Mesa's
//! `src/nouveau/mme/mme_fermi.c` and `mme_fermi_sim.c`. Where they differ,
//! kestrel follows envytools' explicit descriptions (listed in the ROADMAP's
//! "Phase 5 notes"):
//!
//! - assign mode 5 fetches a parameter into the register and uses the
//!   result as the method address (Mesa stores the result);
//! - the method increment is bits 12 to 17 of a method address operand
//!   (Mesa: 12 to 15);
//! - an exit on a branch exits only if the branch is not taken.
//!
//! Each instruction is one 32-bit word: OP (bits 2:0), the assign mode (6:4)
//! or for branches NOT_ZERO (4) and ANNUL (5), EXIT (7), DST (10:8), SRC0
//! (13:11), SRC1 (16:14), and an ALU op (21:17), a signed 18-bit immediate
//! (31:14), or bitfield positions (src bit 21:17, size 26:22, dst bit 31:27).
//! Eight registers; r0 reads 0 and ignores writes; r1 holds the first
//! parameter at the start.

/// What a macro reads and writes outside itself.
pub trait Host {
    /// A method register's value (the "read" instruction), by byte offset.
    fn read(&mut self, method: u32) -> u32;
    /// Send a method write to the 3D engine.
    fn send(&mut self, method: u32, value: u32) -> Result<(), String>;
}

/// kestrel's choice: a macro that runs this many instructions stops, named
/// (the hardware has a watchdog whose limit no source gives).
pub const STEP_LIMIT: u64 = 10_000_000;

fn field(w: u32, lo: u32, len: u32) -> u32 {
    (w >> lo) & ((1u32 << len) - 1)
}

fn imm(w: u32) -> i32 {
    (w as i32) >> 14
}

/// Bits `size` of `value` from `src`, moved to `dst`; 0 when either
/// position is past bit 31 (as Mesa's simulator).
fn bfe_shl(value: u32, src: u32, size: u32, dst: u32) -> u32 {
    if src > 31 || dst > 31 {
        return 0;
    }
    let mask = if size >= 32 {
        u32::MAX
    } else {
        (1 << size) - 1
    };
    ((value >> src) & mask) << dst
}

struct Machine<'a> {
    regs: [u32; 8],
    carry: bool,
    method: u32,
    increment: u32,
    has_method: bool,
    params: &'a [u32],
    next_param: usize,
}

impl Machine<'_> {
    fn reg(&self, r: u32) -> u32 {
        self.regs[r as usize]
    }

    fn set(&mut self, r: u32, v: u32) {
        if r != 0 {
            self.regs[r as usize] = v;
        }
    }

    fn param(&mut self) -> Result<u32, String> {
        let p = *self.params.get(self.next_param).ok_or_else(|| {
            format!(
                "a macro reading parameter {} of the {} it was given (the hardware traps: too \
                 few parameters)",
                self.next_param + 1,
                self.params.len()
            )
        })?;
        self.next_param += 1;
        Ok(p)
    }

    /// A method address operand: the method in bits 11:0 (words), the
    /// increment in bits 17:12 (words).
    fn set_method(&mut self, v: u32) {
        self.method = (v & 0xfff) * 4;
        self.increment = (v >> 12 & 0x3f) * 4;
        self.has_method = true;
    }

    fn send(&mut self, host: &mut dyn Host, value: u32) -> Result<(), String> {
        if !self.has_method {
            // Mesa's simulator does nothing; no source says what the
            // hardware does.
            return Err("a macro sending a method before setting its address".into());
        }
        host.send(self.method, value)?;
        self.method = (self.method + self.increment) & 0x3ffc;
        Ok(())
    }

    /// One non-branch instruction: its result, then the assign mode.
    fn exec(&mut self, host: &mut dyn Host, w: u32, pc: u32) -> Result<(), String> {
        let op = field(w, 0, 3);
        let assign = field(w, 4, 3);
        let dst = field(w, 8, 3);
        let (x, y) = (self.reg(field(w, 11, 3)), self.reg(field(w, 14, 3)));
        let result = match op {
            0 => {
                let (r, carry) = match field(w, 17, 5) {
                    0 => {
                        let s = x as u64 + y as u64;
                        (s as u32, s >> 32 != 0)
                    }
                    1 => {
                        let s = x as u64 + y as u64 + self.carry as u64;
                        (s as u32, s >> 32 != 0)
                    }
                    // SUB and SBB: the carry is the borrow (kestrel's
                    // choice of the exact borrow).
                    2 => (x.wrapping_sub(y), (x as u64) < y as u64),
                    3 => {
                        let b = self.carry as u64;
                        (
                            x.wrapping_sub(y).wrapping_sub(b as u32),
                            (x as u64) < y as u64 + b,
                        )
                    }
                    8 => (x ^ y, self.carry),
                    9 => (x | y, self.carry),
                    10 => (x & y, self.carry),
                    11 => (x & !y, self.carry),
                    12 => (!(x & y), self.carry),
                    alu => return Err(format!("macro ALU op {alu} at {pc:#x}")),
                };
                self.carry = carry;
                r
            }
            1 => x.wrapping_add(imm(w) as u32),
            // Merge: x with `size` bits of y from `src` put at `dst`.
            2 => {
                let (src, size, dstb) = (field(w, 17, 5), field(w, 22, 5), field(w, 27, 5));
                let mask = bfe_shl(u32::MAX, 0, size, dstb);
                (x & !mask) | bfe_shl(y, src, size, dstb)
            }
            // `size` bits of y from bit x, moved to `dst`.
            3 => bfe_shl(y, x, field(w, 22, 5), field(w, 27, 5)),
            // `size` bits of y from `src`, moved to bit x.
            4 => bfe_shl(y, field(w, 17, 5), field(w, 22, 5), x),
            // Read: the method register at (x + imm) words; envytools
            // defines it with the "mov" assign mode only.
            5 => {
                if assign != 1 {
                    return Err(format!(
                        "macro read with assign mode {assign} at {pc:#x}, which no source defines"
                    ));
                }
                host.read((x.wrapping_add(imm(w) as u32) & 0xfff) * 4)
            }
            op => return Err(format!("macro op {op} at {pc:#x}, which no source defines")),
        };
        match assign {
            // parm: fetch a parameter into dst; the result is not kept.
            0 => {
                let p = self.param()?;
                self.set(dst, p);
            }
            // mov
            1 => self.set(dst, result),
            // maddr
            2 => {
                self.set(dst, result);
                self.set_method(result);
            }
            // send the result, then fetch a parameter into dst
            3 => {
                self.send(host, result)?;
                let p = self.param()?;
                self.set(dst, p);
            }
            // send the result and keep it
            4 => {
                self.set(dst, result);
                self.send(host, result)?;
            }
            // the result as method address, then fetch a parameter into dst
            5 => {
                self.set_method(result);
                let p = self.param()?;
                self.set(dst, p);
            }
            // the result as method address and kept, then fetch a
            // parameter and send it
            6 => {
                self.set(dst, result);
                self.set_method(result);
                let p = self.param()?;
                self.send(host, p)?;
            }
            // the result as method address, send its bits 17:12, keep it
            _ => {
                self.set(dst, result);
                self.set_method(result);
                self.send(host, result >> 12 & 0x3f)?;
            }
        }
        Ok(())
    }
}

/// Run the macro at word `start` of `code` with `params` (the first in r1),
/// which must all be read.
pub fn run(code: &[u32], start: u32, params: &[u32], host: &mut dyn Host) -> Result<(), String> {
    let Some(&first) = params.first() else {
        return Err("a macro called with no parameter".into());
    };
    let mut m = Machine {
        regs: [0; 8],
        carry: false,
        method: 0,
        increment: 0,
        has_method: false,
        params,
        next_param: 1,
    };
    m.regs[1] = first;
    let fetch = |pc: u32| {
        code.get(pc as usize)
            .copied()
            .ok_or_else(|| format!("a macro running past its code, at {pc:#x}"))
    };
    let mut pc = start;
    // A taken branch's target, applied after the delay slot.
    let mut pending: Option<u32> = None;
    // The macro ends after this instruction (the exit's delay slot).
    let mut exiting = false;
    let mut steps = 0u64;
    loop {
        steps += 1;
        if steps > STEP_LIMIT {
            return Err(format!(
                "a macro still running after {STEP_LIMIT} instructions (the hardware's watchdog)"
            ));
        }
        let w = fetch(pc)?;
        let exit = field(w, 7, 1) == 1;
        let in_delay_slot = pending.is_some() || exiting;
        let mut next = pc + 1;
        if field(w, 0, 3) == 7 {
            if in_delay_slot {
                return Err(format!(
                    "a macro branch in a delay slot at {pc:#x} (an error, envytools)"
                ));
            }
            let v = m.reg(field(w, 11, 3));
            let taken = if field(w, 4, 1) == 1 { v != 0 } else { v == 0 };
            let annul = field(w, 5, 1) == 1;
            let target = (pc as i64 + imm(w) as i64) as u32;
            if taken {
                if annul {
                    next = target;
                } else {
                    pending = Some(target);
                }
            } else if exit {
                // An exit on a branch happens only if it is not taken.
                exiting = true;
            }
            pc = next;
            continue;
        }
        m.exec(host, w, pc)?;
        if let Some(t) = pending.take() {
            next = t;
        } else if exiting {
            break;
        } else if exit {
            exiting = true;
        }
        pc = next;
    }
    if m.next_param < params.len() {
        return Err(format!(
            "a macro ending with {} of its {} parameters unread (the hardware traps: too many \
             parameters)",
            params.len() - m.next_param,
            params.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host recording what a macro sends, with method registers to read.
    #[derive(Default)]
    struct Record {
        regs: std::collections::BTreeMap<u32, u32>,
        sent: Vec<(u32, u32)>,
    }

    impl Host for Record {
        fn read(&mut self, method: u32) -> u32 {
            self.regs.get(&method).copied().unwrap_or(0)
        }
        fn send(&mut self, method: u32, value: u32) -> Result<(), String> {
            self.sent.push((method, value));
            Ok(())
        }
    }

    // Encoders, from the layout in this module's documentation.
    fn alu(assign: u32, dst: u32, a: u32, b: u32, op: u32) -> u32 {
        assign << 4 | dst << 8 | a << 11 | b << 14 | op << 17
    }
    fn addi(assign: u32, dst: u32, a: u32, v: i32) -> u32 {
        1 | assign << 4 | dst << 8 | a << 11 | (v as u32) << 14
    }
    #[allow(clippy::too_many_arguments)]
    fn bf(op: u32, assign: u32, dst: u32, a: u32, b: u32, src: u32, size: u32, at: u32) -> u32 {
        op | assign << 4 | dst << 8 | a << 11 | b << 14 | src << 17 | size << 22 | at << 27
    }
    fn branch(not_zero: bool, annul: bool, a: u32, offset: i32) -> u32 {
        7 | (not_zero as u32) << 4 | (annul as u32) << 5 | a << 11 | (offset as u32) << 14
    }
    const EXIT: u32 = 1 << 7;

    fn run_on(code: &[u32], params: &[u32]) -> (Result<(), String>, Record) {
        let mut host = Record::default();
        host.regs.insert(0x2c8, 77);
        let r = run(code, 0, params, &mut host);
        (r, host)
    }

    #[test]
    fn a_macro_sends_its_parameters_to_methods() {
        // maddr r0 = r1 (the first parameter is the method, increment 1);
        // parm r2, send r2; then exit after sending r2 + 5.
        let code = [
            addi(2, 0, 1, 0),
            alu(3, 2, 0, 0, 0), // send 0 (r0 + r0), then parm -> r2
            addi(4, 0, 2, 5) | EXIT,
            alu(1, 0, 0, 0, 0), // the exit's delay slot
        ];
        let (r, host) = run_on(&code, &[0x1000 | (0x800 >> 2), 9]);
        assert_eq!(r, Ok(()));
        assert_eq!(host.sent, [(0x800, 0), (0x804, 14)]);
    }

    #[test]
    fn branches_have_delay_slots_unless_annulled() {
        // r1 counts down from 3; each pass sends r1 to method 0x100 (no
        // increment): branz r1, -1 with the subtraction in its delay slot.
        let code = [
            addi(2, 0, 0, 0x100 >> 2), // maddr 0x100, increment 0
            addi(4, 1, 1, 0),          // send r1
            branch(true, false, 1, -1),
            addi(1, 1, 1, -1), // delay slot: r1 -= 1
            alu(1, 0, 0, 0, 0) | EXIT,
            alu(1, 0, 0, 0, 0),
        ];
        let (r, host) = run_on(&code, &[3]);
        assert_eq!(r, Ok(()));
        // Sends 3; the branch is taken (r1 = 3) and its slot makes 2; back
        // to the send ... until it sends 0 and the branch is not taken.
        assert_eq!(host.sent, [(0x100, 3), (0x100, 2), (0x100, 1), (0x100, 0)]);
        // Annulled: the slot does not run when taken.
        let code = [
            addi(2, 0, 0, 0x100 >> 2),
            branch(true, true, 1, 2), // r1 != 0: skip to 3, no slot
            addi(4, 0, 0, 1),         // not run
            addi(4, 0, 0, 2) | EXIT,
            alu(1, 0, 0, 0, 0),
        ];
        assert_eq!(run_on(&code, &[1]).1.sent, [(0x100, 2)]);
        // An exit on a branch exits only if it is not taken.
        let code = [
            addi(2, 0, 0, 0x100 >> 2),
            branch(false, true, 1, 3) | EXIT, // r1 == 0? no: not taken, exit
            addi(4, 0, 0, 1),                 // the exit's delay slot
            addi(4, 0, 0, 2),
        ];
        assert_eq!(run_on(&code, &[1]).1.sent, [(0x100, 1)]);
    }

    #[test]
    fn alu_bitfields_and_reads() {
        // r2 = 0xffffffff + 1 (carry), r3 = 0 + 0 + carry; send both.
        let code = [
            addi(2, 0, 0, 0x1000 | (0x200 >> 2)),
            addi(1, 2, 0, -1),
            addi(1, 4, 0, 1),
            alu(4, 2, 2, 4, 0), // add r2 = r2 + r4 = 0, carry
            alu(4, 3, 0, 0, 1), // adc r3 = 0 + 0 + 1
            // merge: r1 (0xff00) with 4 bits of r4 (1) from bit 0 at bit 4
            bf(2, 4, 5, 1, 4, 0, 4, 4),
            // bfe: 8 bits of r1 from bit r4 (1), at bit 0
            bf(3, 4, 6, 4, 1, 0, 8, 0),
            // read method 0x2c8 (r0 + 0xb2)
            5 | 1 << 4 | 7 << 8 | (0xb2 << 14),
            alu(4, 0, 7, 0, 0) | EXIT,
            alu(1, 0, 0, 0, 0),
        ];
        let (r, host) = run_on(&code, &[0xff00]);
        assert_eq!(r, Ok(()));
        let values: Vec<u32> = host.sent.iter().map(|s| s.1).collect();
        assert_eq!(values, [0, 1, 0xff10, 0x80, 77]);
        assert_eq!(host.sent[0].0, 0x200);
        assert_eq!(host.sent[4].0, 0x210);
    }

    #[test]
    fn parameter_counts_and_unknown_instructions_stop() {
        let parm_exit = [alu(0, 2, 0, 0, 0) | EXIT, alu(1, 0, 0, 0, 0)];
        assert!(run_on(&parm_exit, &[1]).0.unwrap_err().contains("too few"));
        let just_exit = [alu(1, 0, 0, 0, 0) | EXIT, alu(1, 0, 0, 0, 0)];
        assert!(run_on(&just_exit, &[1, 2])
            .0
            .unwrap_err()
            .contains("too many"));
        assert!(run_on(&[6], &[1]).0.unwrap_err().contains("macro op 6"));
        assert!(run_on(&[alu(1, 0, 0, 0, 4)], &[1])
            .0
            .unwrap_err()
            .contains("ALU op 4"));
        assert!(run_on(&[branch(false, true, 0, 0)], &[1])
            .0
            .unwrap_err()
            .contains("watchdog"));
        assert!(run_on(&[alu(4, 0, 0, 0, 0)], &[1])
            .0
            .unwrap_err()
            .contains("before setting"));
    }
}
