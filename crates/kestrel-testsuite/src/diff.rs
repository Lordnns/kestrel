//! The differential tester (`PLAN.md` section 2: every fast backend is
//! tested against the interpreter).
//!
//! Two copies of a case's process run side by side, one on the reference
//! backend (the interpreter) and one on the candidate, on the counted
//! clock (52 instructions a tick), so both see exactly the same time and
//! anything that differs is the candidate's doing. Every so many
//! instructions the two are compared: the running thread's registers,
//! flags and system registers, the instructions retired, the contents of
//! every writable page, the system calls made since the last comparison
//! (number and arguments), the program's output and how it stopped.
//!
//! The first comparison that differs bounds the divergence to a stretch
//! of instructions. Both processes are then loaded again, run to the
//! start of that stretch (the counted clock makes the replay exact), and
//! compared in smaller steps, down to one instruction, which is reported
//! with its address and encoding. Nothing here knows how a backend works:
//! it only calls [`CpuBackend::run`] with smaller budgets.
//!
//! What it cannot see: a wrong value that is overwritten before the next
//! comparison (a dead flag, a register reloaded at once) leaves no trace
//! in the state, so the program is not affected and nothing is reported;
//! another program that uses the value will show it. The narrowing passes
//! find the first instruction whose effect is still visible at the end
//! of each smaller stretch.

use crate::{Case, Prepared};
use kestrel_cpu::{CpuBackend, CpuState, Exit};
use kestrel_hle::Stop;
use std::fmt;

/// Builds a fresh backend; called again for every replay.
pub type MakeBackend<'a> = &'a dyn Fn() -> Box<dyn CpuBackend>;

/// Instructions between comparisons on the first pass; each replay
/// compares `STEP_FACTOR` times as often, down to every instruction.
pub const FIRST_CHUNK: u64 = 100_000;
pub const STEP_FACTOR: u64 = 100;

/// Where the two backends first differed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Instructions both had retired before the stretch that differed.
    pub at: u64,
    /// Length of that stretch: 1 when the divergence was found to one
    /// instruction.
    pub span: u64,
    /// The reference's PC and the instruction there at the start of the
    /// stretch (`None` if it could not be fetched).
    pub pc: u64,
    pub insn: Option<u32>,
    /// What differed, reference value first.
    pub differences: Vec<String>,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let insn = self
            .insn
            .map_or_else(|| "????????".to_string(), |i| format!("{i:08x}"));
        if self.span == 1 {
            write!(
                f,
                "after {} instructions, at pc {:#x} (instruction {insn}):",
                self.at, self.pc
            )?;
        } else {
            write!(
                f,
                "within instructions {} to {}, from pc {:#x} (instruction {insn}); \
                 not reproduced in smaller steps:",
                self.at,
                self.at + self.span,
                self.pc
            )?;
        }
        for d in &self.differences {
            write!(f, "\n  {d}")?;
        }
        Ok(())
    }
}

/// The result of comparing one case.
#[derive(Debug)]
pub struct Report {
    /// The reference's backend name and the candidate's.
    pub backends: (&'static str, &'static str),
    /// Instructions the reference ran (to the divergence, if any).
    pub instructions: u64,
    /// How the reference stopped.
    pub stop: Stop,
    pub divergence: Option<Divergence>,
}

/// A backend that records the system calls it returns, so the two sides'
/// calls can be compared even when the OS layer answers them the same.
struct Recorder {
    inner: Box<dyn CpuBackend>,
    /// (instructions retired, SVC number, X0..X7) since the last look.
    calls: Vec<(u64, u16, [u64; 8])>,
}

impl CpuBackend for Recorder {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn run(&mut self, state: &mut CpuState, mem: &kestrel_core::Memory, max: u64) -> Exit {
        let exit = self.inner.run(state, mem, max);
        if let Exit::Svc(n) = exit {
            let mut args = [0; 8];
            args.copy_from_slice(&state.x[..8]);
            self.calls.push((state.retired, n, args));
        }
        exit
    }

    fn invalidate(&mut self, addr: u64, len: u64) {
        self.inner.invalidate(addr, len)
    }
}

/// One of the two processes.
struct Side {
    p: Prepared,
    cpu: Recorder,
    /// How the last run stopped.
    stop: Stop,
}

impl Side {
    fn new(case: &Case, tag: &str, make: MakeBackend) -> Result<Self, String> {
        Ok(Side {
            p: Prepared::new(case, tag)?,
            cpu: Recorder {
                inner: make(),
                calls: Vec::new(),
            },
            stop: Stop::StepLimit,
        })
    }

    fn running(&self) -> bool {
        self.stop == Stop::StepLimit
    }

    fn run(&mut self, steps: u64) {
        if self.running() && steps > 0 {
            self.stop = self.p.proc.run(&mut self.cpu, steps);
        }
    }

    fn retired(&self) -> u64 {
        self.p.proc.instructions()
    }
}

/// Everything that differs between the two sides, reference first.
fn compare(a: &mut Side, b: &mut Side) -> Vec<String> {
    let mut d = Vec::new();
    let (na, nb) = (a.cpu.name(), b.cpu.name());
    let mut diff = |what: String, x: String, y: String| {
        if x != y {
            d.push(format!("{what}: {na} {x}, {nb} {y}"));
        }
    };
    // System calls first: a different call explains what follows.
    let (ca, cb) = (
        std::mem::take(&mut a.cpu.calls),
        std::mem::take(&mut b.cpu.calls),
    );
    for i in 0..ca.len().max(cb.len()) {
        let show = |c: Option<&(u64, u16, [u64; 8])>| match c {
            Some((at, n, x)) => format!("svc {n:#x} after {at} instructions, x0..x7 {x:x?}"),
            None => "no call".into(),
        };
        diff(
            format!("system call {}", i + 1),
            show(ca.get(i)),
            show(cb.get(i)),
        );
    }
    let (sa, sb) = (&a.p.proc.cpu, &b.p.proc.cpu);
    diff("pc".into(), hex(sa.pc), hex(sb.pc));
    for i in 0..31 {
        diff(format!("x{i}"), hex(sa.x[i]), hex(sb.x[i]));
    }
    diff("sp".into(), hex(sa.sp), hex(sb.sp));
    diff(
        "nzcv".into(),
        format!("{:?}", sa.flags),
        format!("{:?}", sb.flags),
    );
    for i in 0..32 {
        diff(
            format!("v{i}"),
            format!("{:#034x}", sa.v[i]),
            format!("{:#034x}", sb.v[i]),
        );
    }
    diff("fpcr".into(), hex(sa.fpcr), hex(sb.fpcr));
    diff("fpsr".into(), hex(sa.fpsr), hex(sb.fpsr));
    diff("tpidr_el0".into(), hex(sa.tpidr_el0), hex(sb.tpidr_el0));
    diff(
        "tpidrro_el0".into(),
        hex(sa.tpidrro_el0),
        hex(sb.tpidrro_el0),
    );
    diff(
        "exclusive monitor".into(),
        format!("{:x?}", sa.exclusive_addr),
        format!("{:x?}", sb.exclusive_addr),
    );
    diff(
        "instructions retired".into(),
        sa.retired.to_string(),
        sb.retired.to_string(),
    );
    diff(
        "clock".into(),
        format!("{:?}", sa.clock),
        format!("{:?}", sb.clock),
    );
    diff(
        "running thread".into(),
        a.p.proc.sched.current.to_string(),
        b.p.proc.sched.current.to_string(),
    );
    d.extend(compare_memory(a, b));
    let (oa, ob) = (a.p.output(), b.p.output());
    if oa != ob {
        let at = oa
            .bytes()
            .zip(ob.bytes())
            .take_while(|(x, y)| x == y)
            .count();
        d.push(format!(
            "output from byte {at}: {na} {:?}, {nb} {:?}",
            &oa[at.min(oa.len())..],
            &ob[at.min(ob.len())..]
        ));
    }
    let (ta, tb) = (format!("{:?}", a.stop), format!("{:?}", b.stop));
    if ta != tb {
        d.push(format!("stop: {na} {ta}, {nb} {tb}"));
    }
    d
}

fn hex(v: u64) -> String {
    format!("{v:#x}")
}

/// The mappings must match, and so must every byte of every page the
/// guest can write; read-only pages change only through the OS layer,
/// whose calls are compared already.
fn compare_memory(a: &Side, b: &Side) -> Vec<String> {
    let (ma, mb) = (&a.p.proc.mem, &b.p.proc.mem);
    let ra: Vec<_> = ma.regions().collect();
    let rb: Vec<_> = mb.regions().collect();
    if ra != rb {
        let i = ra.iter().zip(&rb).take_while(|(x, y)| x == y).count();
        return vec![format!(
            "memory map, region {}: {} {:x?}, {} {:x?}",
            i + 1,
            a.cpu.name(),
            ra.get(i),
            b.cpu.name(),
            rb.get(i)
        )];
    }
    let mut out = Vec::new();
    let (mut pa, mut pb) = (vec![0u8; 4096], vec![0u8; 4096]);
    for (base, size, perm, name) in ra {
        if !perm.contains(kestrel_core::Perm::RW) {
            continue;
        }
        for page in (base..base + size).step_by(4096) {
            if ma.read_bytes(page, &mut pa).is_err() || mb.read_bytes(page, &mut pb).is_err() {
                continue;
            }
            if pa != pb {
                let i = pa.iter().zip(&pb).take_while(|(x, y)| x == y).count();
                let word = |p: &[u8]| {
                    let s = i & !7;
                    format!(
                        "{:#018x}",
                        u64::from_le_bytes(p[s..s + 8].try_into().unwrap())
                    )
                };
                out.push(format!(
                    "memory at {:#x} ({name}), the 8 bytes from {:#x}: {} {}, {} {}",
                    page + i as u64,
                    page + (i & !7) as u64,
                    a.cpu.name(),
                    word(&pa),
                    b.cpu.name(),
                    word(&pb)
                ));
                // The first differing page says enough.
                return out;
            }
        }
    }
    out
}

/// What one pass found.
enum Pass {
    /// Both stopped the same way, with nothing different.
    Same {
        instructions: u64,
        stop: Stop,
    },
    Diverged(Divergence),
}

/// Load both sides, run them to `start` without comparing, then compare
/// every `chunk` instructions, giving up after `limit` instructions.
fn pass(
    case: &Case,
    reference: MakeBackend,
    candidate: MakeBackend,
    start: u64,
    chunk: u64,
    limit: u64,
) -> Result<(Pass, (&'static str, &'static str)), String> {
    let mut a = Side::new(case, "-reference", reference)?;
    let mut b = Side::new(case, "-candidate", candidate)?;
    let names = (a.cpu.name(), b.cpu.name());
    a.run(start);
    b.run(start);
    // Calls before the start were compared on the pass that got here.
    a.cpu.calls.clear();
    b.cpu.calls.clear();
    let end = start.saturating_add(limit);
    loop {
        let at = a.retired();
        let pc = a.p.proc.cpu.pc;
        let insn = a.p.proc.mem.fetch32(pc).ok();
        a.run(chunk);
        b.run(chunk);
        let differences = compare(&mut a, &mut b);
        if !differences.is_empty() {
            let d = Divergence {
                at,
                span: chunk,
                pc,
                insn,
                differences,
            };
            return Ok((Pass::Diverged(d), names));
        }
        if !a.running() || a.retired() >= end {
            let instructions = a.retired();
            return Ok((
                Pass::Same {
                    instructions,
                    stop: a.stop.clone(),
                },
                names,
            ));
        }
    }
}

/// Run `case` on both backends and report the first divergence, if any.
pub fn diff_case(
    case: &Case,
    reference: MakeBackend,
    candidate: MakeBackend,
) -> Result<Report, String> {
    let max = crate::MAX_STEPS;
    let (first, backends) = pass(case, reference, candidate, 0, FIRST_CHUNK, max)?;
    let mut found = match first {
        Pass::Same { instructions, stop } => {
            return Ok(Report {
                backends,
                instructions,
                stop,
                divergence: None,
            })
        }
        Pass::Diverged(d) => d,
    };
    // Narrow it down. A stretch that does not diverge again in smaller
    // steps (the candidate behaves differently with smaller budgets) is
    // reported as it is.
    while found.span > 1 {
        let chunk = (found.span / STEP_FACTOR).max(1);
        match pass(case, reference, candidate, found.at, chunk, found.span)?.0 {
            Pass::Diverged(d) if d.at < found.at + found.span => found = d,
            _ => break,
        }
    }
    Ok(Report {
        backends,
        instructions: found.at,
        stop: Stop::StepLimit,
        divergence: Some(found),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_cpu::interp::Interpreter;
    use std::sync::{Arc, Mutex};

    fn interp() -> Box<dyn CpuBackend> {
        Box::new(Interpreter::new())
    }

    fn case(name: &str) -> &'static Case {
        crate::CASES.iter().find(|c| c.name == name).unwrap()
    }

    /// What a faulty interpreter gets wrong, from the `nth` time the
    /// instruction runs on, as a backend's bug would.
    #[derive(Clone, Copy)]
    enum Bug {
        /// ADD (immediate, 64-bit) to a register other than SP leaves
        /// bit 0 of its result flipped.
        AddResult,
        /// SUBS (immediate) sets C wrongly.
        SubsCarry,
        /// STR (64-bit, unsigned offset) stores bit 0 flipped.
        StoreValue,
        /// System call 0x27 (svcOutputDebugString) is returned as 0x26
        /// (svcBreak).
        WrongSvc,
    }

    /// The interpreter with one deliberate bug, one instruction at a time.
    /// Records where the bug first struck: (instructions retired before,
    /// pc).
    struct Faulty {
        inner: Interpreter,
        bug: Bug,
        nth: u64,
        seen: u64,
        struck: Arc<Mutex<Option<(u64, u64)>>>,
    }

    impl CpuBackend for Faulty {
        fn name(&self) -> &'static str {
            "faulty"
        }

        fn run(&mut self, s: &mut CpuState, m: &kestrel_core::Memory, max: u64) -> Exit {
            for _ in 0..max {
                let (pc, at) = (s.pc, s.retired);
                let insn = m.fetch32(pc).unwrap_or(0);
                let hit = match self.bug {
                    Bug::AddResult => insn & 0xff80_0000 == 0x9100_0000 && insn & 31 != 31,
                    Bug::SubsCarry => insn & 0x7f80_0000 == 0x7100_0000,
                    Bug::StoreValue => insn & 0xffc0_0000 == 0xf900_0000,
                    Bug::WrongSvc => insn == 0xd400_04e1,
                };
                let strike = hit && {
                    self.seen += 1;
                    self.seen >= self.nth
                };
                // The address the STR writes, before it runs.
                let addr = s
                    .xsp(insn >> 5 & 31)
                    .wrapping_add((insn as u64 >> 10 & 0xfff) * 8);
                let exit = self.inner.run(s, m, 1);
                if strike {
                    let mut struck = self.struck.lock().unwrap();
                    if struck.is_none() {
                        *struck = Some((at, pc));
                    }
                    match self.bug {
                        Bug::AddResult => s.x[(insn & 31) as usize] ^= 1,
                        Bug::SubsCarry => s.flags.c = !s.flags.c,
                        Bug::StoreValue => {
                            let v = m.read64(addr).unwrap();
                            m.write64(addr, v ^ 1).unwrap();
                        }
                        Bug::WrongSvc => return Exit::Svc(0x26),
                    }
                }
                if exit != Exit::StepLimit {
                    return exit;
                }
            }
            Exit::StepLimit
        }
    }

    /// Diff `name` against an interpreter with `bug` at its `nth`
    /// occurrence; returns the divergence and where the bug struck.
    fn find(name: &str, bug: Bug, nth: u64) -> (Divergence, (u64, u64)) {
        let struck = Arc::new(Mutex::new(None));
        let s = struck.clone();
        let faulty = move || -> Box<dyn CpuBackend> {
            Box::new(Faulty {
                inner: Interpreter::new(),
                bug,
                nth,
                seen: 0,
                struck: s.clone(),
            })
        };
        let report = diff_case(case(name), &interp, &faulty).unwrap();
        let d = report.divergence.expect("the bug should be found");
        let at = *struck.lock().unwrap();
        (d, at.expect("the bug should strike"))
    }

    #[test]
    fn the_interpreter_agrees_with_itself_on_every_case() {
        for c in crate::CASES {
            let r = diff_case(c, &interp, &interp).unwrap();
            assert_eq!(r.divergence, None, "{}", c.name);
            assert_eq!(r.stop, Stop::Exited, "{}", c.name);
        }
    }

    /// The recompiler, in both calling conventions, with blocks as long
    /// as they come and with one instruction each.
    #[cfg(all(
        target_arch = "x86_64",
        any(target_os = "windows", target_os = "linux")
    ))]
    #[test]
    fn the_recompiler_agrees_with_the_interpreter_on_every_case() {
        use kestrel_cpu::jit::{x64::Abi, Jit};
        for abi in [Abi::SysV, Abi::Win64] {
            for max in [64, 1] {
                let jit = move || -> Box<dyn CpuBackend> {
                    let mut j = Jit::with_abi(abi).unwrap();
                    j.set_max_block(max);
                    Box::new(j)
                };
                for c in crate::CASES {
                    let r = diff_case(c, &interp, &jit).unwrap();
                    if let Some(d) = r.divergence {
                        panic!("{} ({abi:?}, blocks of {max}): {d}", c.name);
                    }
                    assert_eq!(r.stop, Stop::Exited, "{}", c.name);
                }
            }
        }
    }

    #[test]
    fn a_wrong_register_is_found_at_its_instruction() {
        // Deep enough that the first pass's chunks of 100,000 must be
        // narrowed down twice.
        let (d, (at, pc)) = find("algos", Bug::AddResult, 5_000);
        assert_eq!((d.at, d.pc, d.span), (at, pc, 1), "{d}");
        let rd = d.insn.unwrap() & 31;
        assert!(
            d.differences[0].starts_with(&format!("x{rd}: interp ")),
            "{d}"
        );
    }

    #[test]
    fn wrong_flags_are_found_at_their_instruction() {
        let (d, (at, pc)) = find("sha256", Bug::SubsCarry, 300);
        assert_eq!((d.at, d.pc, d.span), (at, pc, 1), "{d}");
        assert!(d.differences[0].starts_with("nzcv: interp "), "{d}");
    }

    #[test]
    fn a_wrong_memory_write_is_found_at_its_instruction() {
        let (d, (at, pc)) = find("vm", Bug::StoreValue, 200);
        assert_eq!((d.at, d.pc, d.span), (at, pc, 1), "{d}");
        assert_eq!(d.differences.len(), 1, "{d}");
        assert!(d.differences[0].starts_with("memory at "), "{d}");
    }

    #[test]
    fn a_wrong_system_call_is_found_at_its_instruction() {
        let (d, (at, pc)) = find("hello", Bug::WrongSvc, 1);
        assert_eq!((d.at, d.pc, d.span), (at, pc, 1), "{d}");
        assert!(
            d.differences[0].starts_with("system call 1: interp svc 0x27"),
            "{d}"
        );
        assert!(d.differences[0].contains("faulty svc 0x26"), "{d}");
    }
}
