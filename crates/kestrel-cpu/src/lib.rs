//! Guest CPU emulation.
//!
//! Every backend implements [`CpuBackend`]. The emulator picks one at start-up
//! based on the host:
//!
//! | Host       | Backend today  | Planned fast path                          |
//! |------------|----------------|--------------------------------------------|
//! | x86_64     | `jit` on Windows and Linux (`auto`), `interp` elsewhere | `jit`: ARM64 -> x86_64 dynamic recompiler |
//! | aarch64    | `interp`       | `native`: run guest code directly under    |
//! |            |                | KVM / Hypervisor.framework / WHP           |
//!
//! The interpreter is the reference implementation: it runs everywhere, and
//! the faster backends are tested against it instruction by instruction.

pub mod float;
pub mod interp;
#[cfg(jit_host)]
pub mod jit;
pub mod state;

use kestrel_core::{MemFault, Memory};
pub use state::{Clock, CpuState, Flags};

/// Why a backend stopped running guest code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// `SVC #imm`: a call into the guest OS. PC already points past the SVC.
    Svc(u16),
    /// `BRK #imm`. PC points at the BRK.
    Breakpoint(u16),
    /// Encoding the backend does not implement (yet). PC points at it.
    Undefined { pc: u64, insn: u32 },
    /// Instruction the backend recognises but deliberately does not run,
    /// because `what` is not modelled yet. PC points at it.
    Unsupported {
        pc: u64,
        insn: u32,
        what: Unmodelled,
    },
    /// Data or instruction fetch fault. PC points at the faulting instruction.
    Fault { pc: u64, fault: MemFault },
    /// The step budget passed to [`CpuBackend::run`] ran out.
    StepLimit,
}

/// Architecture features a backend recognises but does not model yet.
///
/// A one-byte code rather than a message so [`Exit`], which every
/// interpreter step returns, stays small.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmodelled {
    /// The alternative half-precision format (FPCR.AHP) in a conversion.
    AltHalfPrecision,
    /// FPCR.RMode other than round-to-nearest in host-computed arithmetic.
    FpRoundingMode,
    /// Flush-to-zero of a double-precision fused multiply-add whose result
    /// rounds to exactly the smallest normal number.
    FpFlushedFmaEdge,
}

impl Unmodelled {
    pub const fn describe(self) -> &'static str {
        match self {
            Unmodelled::AltHalfPrecision => {
                "alternative half-precision format (FPCR.AHP) in a conversion"
            }
            Unmodelled::FpRoundingMode => {
                "FPCR rounding mode other than round-to-nearest in floating-point arithmetic"
            }
            Unmodelled::FpFlushedFmaEdge => {
                "flush-to-zero of a double-precision fused multiply-add that rounds to the smallest normal"
            }
        }
    }
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
            // The recompiler is the default where it is built (the owner's
            // decision of 2026-10-31, once CI passed on Windows x86_64).
            #[cfg(jit_host)]
            return Ok((Box::new(jit::Jit::new()?), None));
            #[cfg(not(jit_host))]
            {
                let note = planned_fast_backend().map(|k| {
                    format!(
                        "{k:?} backend is not yet supported on this platform ({} {}); \
                         using the interpreter",
                        std::env::consts::OS,
                        kestrel_core::host_arch()
                    )
                });
                Ok((Box::new(interp::Interpreter::new()), note))
            }
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
            #[cfg(jit_host)]
            return Ok((Box::new(jit::Jit::new()?), None));
            #[cfg(not(jit_host))]
            Err(format!(
                "the JIT backend is not yet supported on this platform ({} {}); \
                 it runs on Windows and Linux x86_64",
                std::env::consts::OS,
                kestrel_core::host_arch()
            ))
        }
    }
}
