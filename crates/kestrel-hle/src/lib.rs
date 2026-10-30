//! High-level emulation (HLE) of the guest operating system.
//!
//! Instead of running the console's kernel, supervisor calls (`SVC #n`) from
//! guest code are answered here in Rust. This module owns one process: its
//! address space, its main thread and its kernel-visible state.
//!
//! The SVC numbering follows the publicly documented Horizon ABI (see
//! Switchbrew's "SVC" page). Behaviour is implemented from that public
//! documentation and from observing homebrew, never from console firmware.

pub mod homebrew;
pub mod input;
pub mod ipc;
pub mod kernel;
pub mod memory;
pub mod results;
pub mod service;
pub mod svc;
pub mod svc_names;
pub mod sync;
#[cfg(test)]
mod testutil;
pub mod thread;

use kestrel_core::{Memory, Perm};
use kestrel_cpu::{CpuBackend, CpuState, Exit};
use kestrel_loader::Loaded;

/// The process layout: one fixed 39-bit address space ("AddressSpace64Bit"
/// on Switchbrew's "SVC" page), reported through svcGetInfo. The heap and
/// alias region sizes are the 39-bit defaults from Switchbrew's "16.0.0"
/// page; where the regions sit is kestrel's choice, without randomisation.
/// The ELF test programs are linked at 0x200000, below the address space.
pub mod layout {
    /// A region as (base, size).
    pub type Region = (u64, u64);

    pub const ADDRESS_SPACE_BASE: u64 = 0x800_0000;
    pub const ADDRESS_SPACE_END: u64 = 1 << 39;
    /// Code may go anywhere in the address space.
    pub const ASLR_REGION: Region = (ADDRESS_SPACE_BASE, ADDRESS_SPACE_END - ADDRESS_SPACE_BASE);
    /// 64 GiB.
    pub const ALIAS_REGION: Region = (0x10_0000_0000, 0x10_0000_0000);
    /// 8 GiB; svcSetHeapSize maps the heap at its start.
    pub const HEAP_REGION: Region = (HEAP_BASE, 0x2_0000_0000);
    /// 2 GiB, ending at the top of the main thread's stack.
    pub const STACK_REGION: Region = (STACK_TOP - 0x8000_0000, 0x8000_0000);

    pub const STACK_TOP: u64 = 0x7f_0000_0000;
    pub const STACK_SIZE: u64 = 1 << 20;
    pub const HEAP_BASE: u64 = 0x60_0000_0000;
    /// Pages of thread local regions, mapped as threads need them.
    pub const TLS_BASE: u64 = 0x7e_0000_0000;
    pub const TLS_SIZE: u64 = 0x1000;
    /// Where homebrew NROs are loaded: the start of the ASLR region.
    pub const NRO_BASE: u64 = ADDRESS_SPACE_BASE;
    /// The homebrew loader config entries and their strings.
    pub const HOMEBREW_CONFIG: u64 = 0x7d_0000_0000;

    /// Memory the process may use (svcGetInfo TotalMemorySize). Kept
    /// small while guest memory is backed by ordinary host allocations.
    pub const TOTAL_MEMORY: u64 = 0x4000_0000;
    /// The program ID reported for every program: "kestrel" in ASCII, so it
    /// cannot be mistaken for a real title.
    pub const PROGRAM_ID: u64 = u64::from_be_bytes(*b"kestrel\0");
    /// svcGetInfo RandomEntropy, subtypes 0 to 3: fixed, so runs repeat.
    pub const RANDOM_ENTROPY: [u64; 4] = [
        0x243f_6a88_85a3_08d3,
        0x1319_8a2e_0370_7344,
        0xa409_3822_299f_31d0,
        0x082e_fa98_ec4e_6c89,
    ];
}

/// The system version kestrel reports: 20.0.0, the owner's choice (the
/// first major version of the Switch 2 era). Every service answers as
/// that version documents it on Switchbrew.
pub mod firmware {
    pub const MAJOR: u8 = 20;
    pub const MINOR: u8 = 0;
    pub const MICRO: u8 = 0;
    /// The version as libnx's MAKEHOSVERSION encodes it, for the homebrew
    /// HosVersion loader config entry.
    pub const HOS_VERSION: u64 = (MAJOR as u64) << 16 | (MINOR as u64) << 8 | MICRO as u64;
    /// The time the system clocks show when a process starts: 2025-06-05
    /// 00:00:00 UTC, the Switch 2's launch day, the owner's choice. The
    /// clocks then run with the process clock (guest instructions as
    /// 19.2 MHz ticks), so every run sees the same times.
    pub const BOOT_TIME: i64 = 1_749_081_600;
    /// The device's time zone, also the owner's choice: UTC, offset 0,
    /// no daylight saving time.
    pub const LOCATION_NAME: &str = "UTC";
}

/// Why a process stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Guest called ExitProcess.
    Exited,
    /// Guest hit a BRK instruction.
    Break { pc: u64, reason: u64 },
    /// svcBreak without the notification-only flag: the console's crash
    /// reporting (Switchbrew, "SVC"). The reason, and the result libnx's
    /// diagAbortWithResult passes with it.
    Abort {
        pc: u64,
        reason: u32,
        result: Option<u32>,
    },
    /// fatal:u with an error screen: the program ends with this result.
    Fatal { result: u32 },
    /// The program used something kestrel does not implement: an SVC, a
    /// named port, a service, a command or a CMIF control command, named
    /// with its identifiers. The frontend records it in its log.
    Unimplemented { what: String, pc: u64 },
    /// Something the emulator does not support yet, or a guest crash.
    Error(String),
    /// The step budget ran out.
    StepLimit,
    /// The frontend asked the program to stop (its window was closed).
    Interrupted,
}

/// One emulated process.
pub struct Process {
    pub mem: Memory,
    /// The running thread's registers.
    pub cpu: CpuState,
    pub handles: kernel::HandleTable,
    pub sched: thread::Scheduler,
    /// Kernel objects the services keep.
    pub services: service::State,
    /// The main thread's handle, passed to homebrew as MainThreadHandle.
    pub main_thread: u32,
    pub heap_size: u64,
    /// Size of the loaded image, for svcGetInfo UsedMemorySize.
    pub image_size: u64,
    /// Bytes mapped with svcMapPhysicalMemory.
    pub physical_size: u64,
    /// Live svcMapMemory mappings as (destination, source, size).
    pub mapped_memory: Vec<(u64, u64, u64)>,
    /// Bytes the guest wrote through svcOutputDebugString.
    pub debug_output: Vec<u8>,
    /// Echo debug output to stdout as it arrives, and kestrel's notes to
    /// stderr.
    pub echo: bool,
    /// Kestrel's notes on what the program did that it should hear of.
    pub log: Vec<String>,
    /// Where controller input comes from; None: no controller.
    pub input: Option<Box<dyn input::Source>>,
    /// Set by the frontend to stop the program (its window closed).
    pub interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub svc_count: u64,
}

impl Process {
    /// Create a process around an already-loaded image.
    pub fn new(mut mem: Memory, image: &Loaded) -> Result<Self, String> {
        use layout::*;
        use memory::{state, tag};
        // The image the loader mapped: code read-only, data writable.
        let image_regions: Vec<_> = mem
            .regions()
            .filter(|&(b, _, _, _)| b >= image.image_start && b < image.image_end)
            .map(|(b, s, _, _)| (b, s))
            .collect();
        for (base, size) in image_regions {
            mem.update_range(base, size, |p, t| {
                t.state = if p.contains(Perm::W) {
                    state::CODE_DATA
                } else {
                    state::CODE
                };
            })
            .map_err(|e| e.to_string())?;
        }
        let stack = tag(state::STACK);
        mem.map_tagged(STACK_TOP - STACK_SIZE, STACK_SIZE, Perm::RW, stack, "stack")
            .map_err(|e| e.to_string())?;
        let mut p = Process {
            mem,
            cpu: CpuState::default(),
            handles: kernel::HandleTable::default(),
            sched: thread::Scheduler::default(),
            services: service::State::default(),
            main_thread: 0,
            heap_size: 0,
            image_size: image.image_end - image.image_start,
            physical_size: 0,
            mapped_memory: Vec::new(),
            debug_output: Vec::new(),
            echo: false,
            log: Vec::new(),
            input: None,
            interrupt: None,
            svc_count: 0,
        };
        p.main_thread = p
            .new_thread(
                image.entry,
                0,
                STACK_TOP,
                thread::MAIN_PRIORITY,
                thread::DEFAULT_CORE,
            )
            .map_err(|code| format!("cannot create the main thread: {code:#x}"))?;
        let main = &mut p.sched.threads[0];
        main.state = thread::State::Ready;
        p.cpu = std::mem::take(&mut main.cpu);
        p.sched.slice_left = thread::SLICE;
        Ok(p)
    }

    /// Load `file`, an ELF test program or a homebrew NRO, into a new
    /// process. An NRO is loaded at `layout::NRO_BASE` and started with
    /// argv[0] = `name` followed by `args`; an ELF takes no arguments.
    pub fn load(file: &[u8], name: &str, args: &[String]) -> Result<Self, String> {
        let mut mem = Memory::new();
        if kestrel_loader::is_nro(file) {
            let nro = kestrel_loader::load_nro(file, layout::NRO_BASE, &mut mem)
                .map_err(|e| e.to_string())?;
            let mut p = Process::new(mem, &nro.image)?;
            let argv: Vec<String> = std::iter::once(name.to_string())
                .chain(args.iter().cloned())
                .collect();
            p.start_homebrew(&argv)?;
            Ok(p)
        } else {
            if !args.is_empty() {
                return Err("arguments are only passed to homebrew NROs".into());
            }
            let image = kestrel_loader::load_elf(file, &mut mem).map_err(|e| e.to_string())?;
            Process::new(mem, &image)
        }
    }

    /// Run until the process stops or `max_steps` instructions retire.
    pub fn run(&mut self, backend: &mut dyn CpuBackend, max_steps: u64) -> Stop {
        let start = self.instructions();
        loop {
            let used = self.instructions() - start;
            if used >= max_steps {
                return Stop::StepLimit;
            }
            if let Some(flag) = &self.interrupt {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return Stop::Interrupted;
                }
            }
            self.hid_tick();
            let budget = (max_steps - used).min(self.sched.slice_left);
            let before = self.cpu.retired;
            let exit = backend.run(&mut self.cpu, &mut self.mem, budget);
            let ran = self.cpu.retired - before;
            self.sched.slice_left = self.sched.slice_left.saturating_sub(ran);
            match exit {
                Exit::Svc(n) => {
                    self.svc_count += 1;
                    if let Some(stop) = svc::dispatch(self, n) {
                        return stop;
                    }
                    let rotate = std::mem::take(&mut self.sched.rotate);
                    if let Some(stop) = self.schedule(rotate) {
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
                Exit::StepLimit => {}
            }
            // The core used up its slice: the next core gets a turn.
            if self.sched.slice_left == 0 {
                if let Some(stop) = self.schedule(true) {
                    return stop;
                }
            }
        }
    }

    /// Add a note to the log, echoed to stderr when output is echoed.
    pub fn note(&mut self, what: String) {
        if self.echo {
            eprintln!("kestrel: {what}");
        }
        self.log.push(what);
    }

    pub fn debug_output_str(&self) -> String {
        String::from_utf8_lossy(&self.debug_output).into_owned()
    }
}
