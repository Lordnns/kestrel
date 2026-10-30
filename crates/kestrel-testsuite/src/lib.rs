//! The guest test programs, compiled into the binary.
//!
//! Each case is an ARM64 program from `guest/bin` (an ELF, or a homebrew NRO
//! with its arguments) plus the exact output it must print
//! (`guest/expected`, computed independently of kestrel). Embedding
//! them means a tester only needs the `kestrel` executable to run
//! `kestrel selftest` on their machine.

use kestrel_cpu::{create_backend, BackendKind};
use kestrel_hle::{Process, Stop};
use std::time::{Duration, Instant};

pub struct Case {
    pub name: &'static str,
    pub description: &'static str,
    /// The program: an ELF or an NRO.
    pub file: &'static [u8],
    /// NRO arguments after argv[0], one per line (`guest/src/nro/*.args`).
    pub args: &'static str,
    pub expected: &'static str,
}

impl Case {
    /// The arguments after argv[0].
    pub fn args(&self) -> Vec<String> {
        self.args.lines().map(str::to_string).collect()
    }
}

macro_rules! case {
    ($name:literal, $desc:literal) => {
        case!($name, $name, $desc)
    };
    // A build of `$program` under another name, sharing its expected output.
    ($name:literal, $program:literal, $desc:literal) => {
        Case {
            name: $name,
            description: $desc,
            file: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../guest/bin/",
                $name,
                ".elf"
            )),
            args: "",
            expected: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../guest/expected/",
                $program,
                ".txt"
            )),
        }
    };
}

/// A homebrew NRO, run with the arguments in `guest/src/nro/<name>.args`.
macro_rules! homebrew {
    ($name:literal, $desc:literal) => {
        Case {
            name: $name,
            description: $desc,
            file: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../guest/bin/",
                $name,
                ".nro"
            )),
            args: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../guest/src/nro/",
                $name,
                ".args"
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
    case!("asm_checks", "121 hand-written instruction checks"),
    case!("vm", "bytecode VM: jump tables and indirect calls"),
    case!("bigint", "128-bit math, Miller-Rabin primality"),
    case!("algos", "recursion, CRC32, sieve, heap, sort"),
    case!("sha256", "SHA-256 of three inputs"),
    case!("fp", "127 scalar FP results: NaNs, FZ, DN, rounding"),
    case!("intrinsics", "168 NEON intrinsics, every instruction class"),
    // The same C programs built with floating point and NEON allowed.
    case!("hello-neon", "hello", "hello, NEON build"),
    case!("vm-neon", "vm", "vm, NEON build"),
    case!("bigint-neon", "bigint", "bigint, NEON build"),
    case!("algos-neon", "algos", "algos, NEON build (vector compares)"),
    case!(
        "sha256-neon",
        "sha256",
        "sha256, NEON build (LD4, widening)"
    ),
    // Homebrew format: NRO loader, launch environment, self-relocation.
    homebrew!("hbtest", "homebrew NRO: arguments, bss, relocations"),
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
    let file_name = if kestrel_loader::is_nro(case.file) {
        format!("{}.nro", case.name)
    } else {
        format!("{}.elf", case.name)
    };
    let mut proc = Process::load(case.file, &file_name, &case.args())?;
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
