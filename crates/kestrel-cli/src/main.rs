//! `kestrel` command-line front end.

use kestrel_core::Memory;
use kestrel_cpu::{create_backend, interp::Interpreter, BackendKind, CpuBackend};
use kestrel_hle::{Process, Stop};
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "\
kestrel - experimental ARM64 console emulator

USAGE:
    kestrel run <program.elf> [options]
    kestrel info

OPTIONS:
    --backend <auto|interp|native|jit>   CPU backend (default: auto)
    --max-steps <n>                      Instruction budget (default: 1e10)
    --trace                              Print every executed instruction
    --regs                               Dump registers when the program stops
    --stats                              Print instruction count and speed
";

struct Opts {
    path: String,
    backend: BackendKind,
    max_steps: u64,
    trace: bool,
    regs: bool,
    stats: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut it = args.iter();
    let path = it.next().ok_or("missing program path")?.clone();
    let mut o = Opts {
        path,
        backend: BackendKind::Auto,
        max_steps: 10_000_000_000,
        trace: false,
        regs: false,
        stats: false,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--backend" => o.backend = it.next().ok_or("--backend needs a value")?.parse()?,
            "--max-steps" => {
                o.max_steps = it
                    .next()
                    .ok_or("--max-steps needs a value")?
                    .replace('_', "")
                    .parse()
                    .map_err(|e| format!("bad --max-steps: {e}"))?
            }
            "--trace" => o.trace = true,
            "--regs" => o.regs = true,
            "--stats" => o.stats = true,
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    Ok(o)
}

fn info() {
    println!("kestrel {}", env!("CARGO_PKG_VERSION"));
    println!("host architecture : {}", kestrel_core::host_arch());
    println!("host OS           : {}", std::env::consts::OS);
    println!("portable backend  : interp (available)");
    match kestrel_cpu::planned_fast_backend() {
        Some(k) => println!("fast backend      : {k:?} (planned, not implemented yet)"),
        None => println!("fast backend      : none planned for this host"),
    }
}

fn run(o: Opts) -> Result<ExitCode, String> {
    let file = std::fs::read(&o.path).map_err(|e| format!("cannot read {}: {e}", o.path))?;
    let mut mem = Memory::new();
    let image =
        kestrel_loader::load_elf(&file, &mut mem).map_err(|e| format!("{}: {e}", o.path))?;
    let mut proc = Process::new(mem, &image)?;
    proc.echo = true;

    let mut backend: Box<dyn CpuBackend> = if o.trace {
        if !matches!(o.backend, BackendKind::Auto | BackendKind::Interp) {
            return Err("--trace is only supported by the interpreter".into());
        }
        Box::new(Interpreter { trace: true })
    } else {
        let (b, note) = create_backend(o.backend)?;
        if let Some(n) = note {
            eprintln!("note: {n}");
        }
        b
    };

    let t0 = Instant::now();
    let stop = proc.run(backend.as_mut(), o.max_steps);
    let elapsed = t0.elapsed();

    if o.regs {
        eprintln!("{:?}", proc.cpu);
    }
    if o.stats {
        let n = proc.cpu.retired;
        let mips = n as f64 / elapsed.as_secs_f64().max(1e-9) / 1e6;
        eprintln!(
            "[{}] {n} instructions, {} SVCs in {:.3}s ({mips:.1} MIPS)",
            backend.name(),
            proc.svc_count,
            elapsed.as_secs_f64()
        );
    }
    match stop {
        Stop::Exited => Ok(ExitCode::SUCCESS),
        Stop::Break { pc, reason } => {
            eprintln!("guest break at {pc:#x} (reason {reason:#x})");
            Ok(ExitCode::from(3))
        }
        Stop::StepLimit => {
            eprintln!("stopped: instruction budget exhausted");
            Ok(ExitCode::from(4))
        }
        Stop::Error(e) => Err(e),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => parse(&args[1..]).and_then(run),
        Some("info") => {
            info();
            Ok(ExitCode::SUCCESS)
        }
        Some("-h" | "--help") | None => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some(other) => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
