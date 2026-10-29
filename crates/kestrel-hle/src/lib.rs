//! High-level emulation (HLE) of the guest operating system.
//!
//! Instead of running the console's kernel, supervisor calls (`SVC #n`) from
//! guest code are answered here in Rust. This module owns one process: its
//! address space, its main thread and its kernel-visible state.
//!
//! The SVC numbering follows the publicly documented Horizon ABI (see
//! Switchbrew's "SVC" page). Behaviour is implemented from that public
//! documentation and from observing homebrew, never from console firmware.

pub mod svc;

use kestrel_core::{Memory, Perm};
use kestrel_cpu::{CpuBackend, CpuState, Exit};
use kestrel_loader::Loaded;

/// Fixed process layout for now. Real titles use ASLR'd regions reported
/// through svcGetInfo; these constants stand in until that is implemented.
pub mod layout {
    pub const STACK_TOP: u64 = 0x7f_0000_0000;
    pub const STACK_SIZE: u64 = 1 << 20;
    pub const HEAP_BASE: u64 = 0x60_0000_0000;
    pub const TLS_BASE: u64 = 0x7e_0000_0000;
    pub const TLS_SIZE: u64 = 0x1000;
}

/// Why a process stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Guest called ExitProcess.
    Exited,
    /// Guest hit a BRK or svcBreak.
    Break { pc: u64, reason: u64 },
    /// Something the emulator does not support yet, or a guest crash.
    Error(String),
    /// The step budget ran out.
    StepLimit,
}

/// One emulated process with a single thread.
pub struct Process {
    pub mem: Memory,
    pub cpu: CpuState,
    pub heap_size: u64,
    /// Bytes the guest wrote through svcOutputDebugString.
    pub debug_output: Vec<u8>,
    /// Echo debug output to stdout as it arrives.
    pub echo: bool,
    pub svc_count: u64,
}

impl Process {
    /// Create a process around an already-loaded image.
    pub fn new(mut mem: Memory, image: &Loaded) -> Result<Self, String> {
        use layout::*;
        mem.map(STACK_TOP - STACK_SIZE, STACK_SIZE, Perm::RW, "stack")
            .map_err(|e| e.to_string())?;
        mem.map(TLS_BASE, TLS_SIZE, Perm::RW, "tls")
            .map_err(|e| e.to_string())?;
        let cpu = CpuState {
            pc: image.entry,
            sp: STACK_TOP,
            tpidrro_el0: TLS_BASE,
            ..Default::default()
        };
        Ok(Process {
            mem,
            cpu,
            heap_size: 0,
            debug_output: Vec::new(),
            echo: false,
            svc_count: 0,
        })
    }

    /// Run until the process stops or `max_steps` instructions retire.
    pub fn run(&mut self, backend: &mut dyn CpuBackend, max_steps: u64) -> Stop {
        let start = self.cpu.retired;
        loop {
            let used = self.cpu.retired - start;
            if used >= max_steps {
                return Stop::StepLimit;
            }
            match backend.run(&mut self.cpu, &mut self.mem, max_steps - used) {
                Exit::Svc(n) => {
                    self.svc_count += 1;
                    if let Some(stop) = svc::dispatch(self, n) {
                        return stop;
                    }
                }
                Exit::Breakpoint(imm) => {
                    return Stop::Break {
                        pc: self.cpu.pc,
                        reason: imm as u64,
                    }
                }
                Exit::Undefined { pc, insn } => {
                    return Stop::Error(format!(
                        "unimplemented instruction {insn:08x} at {pc:#x} (backend: {})",
                        backend.name()
                    ))
                }
                Exit::Unsupported { pc, insn, what } => {
                    return Stop::Error(format!(
                        "unsupported: {} (instruction {insn:08x} at {pc:#x}, backend: {})",
                        what.describe(),
                        backend.name()
                    ))
                }
                Exit::Fault { pc, fault } => {
                    return Stop::Error(format!("guest fault at pc {pc:#x}: {fault}"))
                }
                Exit::StepLimit => return Stop::StepLimit,
            }
        }
    }

    pub fn debug_output_str(&self) -> String {
        String::from_utf8_lossy(&self.debug_output).into_owned()
    }
}
