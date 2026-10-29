//! The guest test programs, compiled into the binary.
//!
//! Each case is an ARM64 ELF from `guest/bin` plus the exact output it must
//! print (`guest/expected`, computed independently of kestrel). Embedding
//! them means a tester only needs the `kestrel` executable to run
//! `kestrel selftest` on their machine.

use kestrel_core::Memory;
use kestrel_cpu::{create_backend, BackendKind};
use kestrel_hle::{Process, Stop};
use std::time::{Duration, Instant};

pub struct Case {
    pub name: &'static str,
    pub description: &'static str,
    pub elf: &'static [u8],
    pub expected: &'static str,
}

macro_rules! case {
    ($name:literal, $desc:literal) => {
        Case {
            name: $name,
            description: $desc,
            elf: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../guest/bin/",
                $name,
                ".elf"
            )),
            expected: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../guest/expected/",
                $name,
                ".txt"
            )),
        }
    };
}

/// Every program in the suite, cheapest first.
pub const CASES: &[Case] = &[
    case!("hello", "smallest program: one debug print"),
    case!("asm_checks", "91 hand-written instruction checks"),
    case!("vm", "bytecode VM: jump tables and indirect calls"),
    case!("bigint", "128-bit math, Miller-Rabin primality"),
    case!("algos", "recursion, CRC32, sieve, heap, sort"),
    case!("sha256", "SHA-256 of three inputs"),
];

/// Instruction budget per case; generous, so only real hangs trip it.
pub const MAX_STEPS: u64 = 500_000_000;

pub struct Outcome {
    pub passed: bool,
    pub stop: Stop,
    pub output: String,
    pub instructions: u64,
    pub elapsed: Duration,
    pub backend: &'static str,
}

impl Outcome {
    /// Millions of guest instructions per second.
    pub fn mips(&self) -> f64 {
        self.instructions as f64 / self.elapsed.as_secs_f64().max(1e-9) / 1e6
    }

    /// Short reason for a failure, for reports.
    pub fn failure_reason(&self, case: &Case) -> Option<String> {
        if self.passed {
            return None;
        }
        if self.stop != Stop::Exited {
            return Some(format!("stopped with {:?}", self.stop));
        }
        let got: Vec<&str> = self.output.lines().collect();
        for (i, want) in case.expected.lines().enumerate() {
            let have = got.get(i).copied().unwrap_or("<missing>");
            if have != want {
                return Some(format!("line {}: expected {want:?}, got {have:?}", i + 1));
            }
        }
        Some("output has extra lines".into())
    }
}

/// Load and run one case on the requested backend.
pub fn run_case(case: &Case, backend: BackendKind) -> Result<Outcome, String> {
    let mut mem = Memory::new();
    let image = kestrel_loader::load_elf(case.elf, &mut mem).map_err(|e| e.to_string())?;
    let mut proc = Process::new(mem, &image)?;
    let (mut cpu, _) = create_backend(backend)?;
    let t0 = Instant::now();
    let stop = proc.run(cpu.as_mut(), MAX_STEPS);
    let elapsed = t0.elapsed();
    let output = proc.debug_output_str();
    Ok(Outcome {
        passed: stop == Stop::Exited && output == case.expected,
        stop,
        output,
        instructions: proc.cpu.retired,
        elapsed,
        backend: cpu.name(),
    })
}
