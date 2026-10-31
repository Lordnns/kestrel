//! The guest test programs, compiled into the binary.
//!
//! Each case is an ARM64 program from `guest/bin` (an ELF, or a homebrew NRO
//! with its arguments) plus the exact output it must print
//! (`guest/expected`, computed independently of kestrel). Embedding
//! them means a tester only needs the `kestrel` executable to run
//! `kestrel selftest` on their machine.

use kestrel_cpu::{create_backend, BackendKind};
use kestrel_hle::display::{Frame, Screen};
use kestrel_hle::input::{button, Controller, Pad, Source, STICK_MAX};
use kestrel_hle::{Process, Stop};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Case {
    pub name: &'static str,
    pub description: &'static str,
    /// The program: an ELF or an NRO.
    pub file: &'static [u8],
    /// NRO arguments after argv[0], one per line (`guest/src/nro/*.args`).
    pub args: &'static str,
    pub expected: &'static str,
    /// The controller the program sees, if any.
    pub input: Option<fn() -> Box<dyn Source>>,
}

/// The controller `hbinput` sees, modelled in scripts/gen-expected.py: a
/// Pro Controller holding A and ZR with the left stick fully right and the
/// right stick up and to the left for its first 8 samples, then B and
/// Minus with both sticks centred.
pub struct ScriptedPad;

impl Source for ScriptedPad {
    fn controller(&self) -> Controller {
        Controller::ProController
    }

    fn sample(&mut self, n: u64) -> Pad {
        if n < 8 {
            Pad {
                buttons: button::A | button::ZR,
                left: (STICK_MAX, 0),
                right: (-0x6000, STICK_MAX),
            }
        } else {
            Pad {
                buttons: button::B | button::MINUS,
                ..Pad::default()
            }
        }
    }
}

fn scripted_pad() -> Box<dyn Source> {
    Box::new(ScriptedPad)
}

/// CRC-32 as zlib computes it (reflected, polynomial 0xedb88320).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                crc >> 1 ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The frames a program shows, as lines the self-test adds to its output:
/// each frame's size and the CRC-32 of its pixels as little-endian
/// 0x00RRGGBB words (modelled in scripts/gen-expected.py).
struct FrameLog(Arc<Mutex<Vec<String>>>);

impl Screen for FrameLog {
    fn present(&mut self, frame: &Frame) {
        let bytes: Vec<u8> = frame.pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
        let mut log = self.0.lock().unwrap();
        let line = format!(
            "frame {}: {}x{}, crc32 {:#010x}",
            log.len() + 1,
            frame.width,
            frame.height,
            crc32(&bytes)
        );
        log.push(line);
    }
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
            input: None,
        }
    };
}

/// A homebrew NRO, run with the arguments in `guest/src/nro/<name>.args`,
/// and optionally a controller.
macro_rules! homebrew {
    ($name:literal, $desc:literal) => {
        homebrew!($name, $desc, None)
    };
    ($name:literal, $desc:literal, $input:expr) => {
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
            input: $input,
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
    homebrew!("hblayout", "homebrew NRO: svcGetInfo process layout"),
    homebrew!("hbmemory", "homebrew NRO: memory system calls"),
    homebrew!(
        "hbhandles",
        "homebrew NRO: handles, events, transfer memory"
    ),
    homebrew!("hbthreads", "homebrew NRO: threads, priorities, cores"),
    homebrew!(
        "hbsync",
        "homebrew NRO: waits, mutexes, condition variables"
    ),
    homebrew!("hbipc", "homebrew NRO: named ports, sessions, sm:"),
    homebrew!("hbapplet", "homebrew NRO: set:sys, apm, appletOE startup"),
    homebrew!("hbhid", "homebrew NRO: hid startup, shared memory"),
    homebrew!("hbtime", "homebrew NRO: time startup, clocks, UTC calendar"),
    homebrew!("hbfs", "homebrew NRO: fsp-srv, the SD card folder"),
    homebrew!("hbpower", "homebrew NRO: psm power state, error reports"),
    homebrew!(
        "hbinput",
        "homebrew NRO: a scripted Pro Controller through hid",
        Some(scripted_pad)
    ),
    homebrew!("hbdisplay", "homebrew NRO: vi, nvdrv, frames at vsync"),
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

/// Load and run one case on the requested backend. A homebrew case gets
/// an empty SD card, a new folder in the system's temporary directory that
/// is removed afterwards, so every run starts the same.
pub fn run_case(case: &Case, backend: BackendKind) -> Result<Outcome, String> {
    let nro = kestrel_loader::is_nro(case.file);
    let file_name = format!("{}.{}", case.name, if nro { "nro" } else { "elf" });
    let mut proc = Process::load(case.file, &file_name, &case.args())?;
    let sd = std::env::temp_dir().join(format!(
        "kestrel-selftest-{}-{}",
        std::process::id(),
        case.name
    ));
    if nro {
        let _ = std::fs::remove_dir_all(&sd);
        std::fs::create_dir_all(&sd).map_err(|e| format!("cannot create {}: {e}", sd.display()))?;
        proc.services.fs.sd = Some(sd.clone());
    }
    proc.input = case.input.map(|make| make());
    let frames = Arc::new(Mutex::new(Vec::new()));
    proc.screen = Some(Box::new(FrameLog(frames.clone())));
    let (mut cpu, _) = create_backend(backend)?;
    let t0 = Instant::now();
    let stop = proc.run(cpu.as_mut(), MAX_STEPS);
    let elapsed = t0.elapsed();
    if nro {
        let _ = std::fs::remove_dir_all(&sd);
    }
    let mut output = proc.debug_output_str();
    for line in frames.lock().unwrap().iter() {
        output.push_str(line);
        output.push('\n');
    }
    Ok(Outcome {
        passed: stop == Stop::Exited && output == case.expected,
        stop,
        output,
        instructions: proc.instructions(),
        elapsed,
        backend: cpu.name(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        // The CRC-32 check value (zlib.crc32(b"123456789")).
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
