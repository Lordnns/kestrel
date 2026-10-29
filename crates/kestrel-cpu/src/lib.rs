//! Guest CPU emulation.
//!
//! Every backend implements [`CpuBackend`]. The emulator picks one at start-up
//! based on the host:
//!
//! | Host       | Backend today  | Planned fast path                          |
//! |------------|----------------|--------------------------------------------|
//! | x86_64     | `interp`       | `jit`: ARM64 -> x86_64 dynamic recompiler  |
//! | aarch64    | `interp`       | `native`: run guest code directly under    |
//! |            |                | KVM / Hypervisor.framework / WHP           |
//!
//! The interpreter is the reference implementation: it runs everywhere, and
//! the faster backends are tested against it instruction by instruction.

pub mod interp;
pub mod state;

use kestrel_core::{MemFault, Memory};
pub use state::{CpuState, Flags};

/// Why a backend stopped running guest code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// `SVC #imm`: a call into the guest OS. PC already points past the SVC.
    Svc(u16),
    /// `BRK #imm`. PC points at the BRK.
    Breakpoint(u16),
    /// Encoding the backend does not implement (yet). PC points at it.
    Undefined { pc: u64, insn: u32 },
    /// Data or instruction fetch fault. PC points at the faulting instruction.
    Fault { pc: u64, fault: MemFault },
    /// The step budget passed to [`CpuBackend::run`] ran out.
    StepLimit,
}

/// One way of executing guest ARM64 code.
pub trait CpuBackend {
    /// Short identifier shown to the user, e.g. `"interp"`.
    fn name(&self) -> &'static str;

    /// Run until an [`Exit`] or until `max_steps` instructions have retired.
    fn run(&mut self, state: &mut CpuState, mem: &mut Memory, max_steps: u64) -> Exit;

    /// Called when guest code memory was modified, so cached translations drop.
    fn invalidate(&mut self, _addr: u64, _len: u64) {}
}

/// Which backend the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Fastest backend available on this host.
    Auto,
    /// Portable interpreter.
    Interp,
    /// Hardware-virtualised native execution (aarch64 hosts only).
    Native,
    /// Dynamic recompiler (x86_64 hosts only).
    Jit,
}

impl std::str::FromStr for BackendKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "interp" | "interpreter" => Ok(Self::Interp),
            "native" => Ok(Self::Native),
            "jit" => Ok(Self::Jit),
            _ => Err(format!(
                "unknown backend '{s}' (expected auto, interp, native or jit)"
            )),
        }
    }
}

/// Fast backends this build could offer on this host, whether or not they
/// are implemented yet.
pub fn planned_fast_backend() -> Option<BackendKind> {
    if cfg!(target_arch = "aarch64") {
        Some(BackendKind::Native)
    } else if cfg!(target_arch = "x86_64") {
        Some(BackendKind::Jit)
    } else {
        None
    }
}

/// Construct a backend. Returns the backend plus an optional note explaining
/// any fallback, so the CLI can tell the user what actually runs.
pub fn create_backend(kind: BackendKind) -> Result<(Box<dyn CpuBackend>, Option<String>), String> {
    match kind {
        BackendKind::Interp => Ok((Box::new(interp::Interpreter::new()), None)),
        BackendKind::Auto => {
            let note = planned_fast_backend().map(|k| {
                format!(
                    "{k:?} backend for {} is not implemented yet; using the interpreter",
                    kestrel_core::host_arch()
                )
            });
            Ok((Box::new(interp::Interpreter::new()), note))
        }
        BackendKind::Native => {
            if cfg!(target_arch = "aarch64") {
                Err("native backend is planned for aarch64 hosts but not implemented yet".into())
            } else {
                Err(format!(
                    "native backend needs an aarch64 host; this is {}",
                    kestrel_core::host_arch()
                ))
            }
        }
        BackendKind::Jit => {
            if cfg!(target_arch = "x86_64") {
                Err("JIT backend is planned for x86_64 hosts but not implemented yet".into())
            } else {
                Err(format!(
                    "JIT backend targets x86_64 hosts; this is {}",
                    kestrel_core::host_arch()
                ))
            }
        }
    }
}
