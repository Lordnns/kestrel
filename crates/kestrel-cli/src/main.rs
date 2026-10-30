//! `kestrel` command-line front end.

use kestrel_cpu::{create_backend, interp::Interpreter, BackendKind, CpuBackend};
use kestrel_hle::{Process, Stop};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

/// Release builds get the git tag (e.g. `v0.1.4`) from the Release workflow;
/// local builds report the Cargo version with a `-dev` suffix.
const VERSION: &str = match option_env!("KESTREL_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

const USAGE: &str = "\
kestrel - experimental ARM64 console emulator

USAGE:
    kestrel selftest [-v]            Run the built-in ARM64 test programs
    kestrel extract <dir>            Write the built-in test programs to <dir>
    kestrel run <program.elf|program.nro> [options] [-- args...]
    kestrel info                     Show host and backend information

RUN OPTIONS:
    --backend <auto|interp|native|jit>   CPU backend (default: auto)
    --max-steps <n>                      Instruction budget (default: 1e10)
    --trace                              Print every executed instruction
    --regs                               Dump registers when the program stops
    --stats                              Print instruction count and speed
    --sd <folder>                        The folder that is the SD card
                                         (default: user/sdmc next to kestrel)
    -- args...                           Arguments for a homebrew NRO (argv[0]
                                         is its file name)
";

struct Opts {
    path: String,
    backend: BackendKind,
    max_steps: u64,
    trace: bool,
    regs: bool,
    stats: bool,
    sd: Option<PathBuf>,
    args: Vec<String>,
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
        sd: None,
        args: Vec::new(),
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
            "--sd" => o.sd = Some(it.next().ok_or("--sd needs a folder")?.into()),
            "--" => {
                o.args = it.by_ref().cloned().collect();
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    Ok(o)
}

fn info() {
    println!("kestrel {VERSION}");
    println!("host architecture : {}", kestrel_core::host_arch());
    println!("host OS           : {}", std::env::consts::OS);
    println!("portable backend  : interp (available)");
    match kestrel_cpu::planned_fast_backend() {
        Some(k) => println!("fast backend      : {k:?} (planned, not implemented yet)"),
        None => println!("fast backend      : none planned for this host"),
    }
}

fn selftest(args: &[String]) -> Result<ExitCode, String> {
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    if let Some(bad) = args.iter().find(|a| *a != "-v" && *a != "--verbose") {
        return Err(format!("unknown selftest option '{bad}'"));
    }
    let cases = kestrel_testsuite::CASES;
    println!(
        "kestrel {} self-test on {} / {}",
        VERSION,
        std::env::consts::OS,
        kestrel_core::host_arch()
    );
    println!();
    let mut passed = 0;
    let mut total_insns = 0u64;
    let mut total_time = 0f64;
    let mut backend = "?";
    for case in cases {
        let out = kestrel_testsuite::run_case(case, BackendKind::Auto)?;
        backend = out.backend;
        total_insns += out.instructions;
        total_time += out.elapsed.as_secs_f64();
        let status = if out.passed { "PASS" } else { "FAIL" };
        println!(
            "  {status}  {:<11} {:>12} instr  {:>7.3}s  {:>6.1} MIPS   {}",
            case.name,
            out.instructions,
            out.elapsed.as_secs_f64(),
            out.mips(),
            case.description
        );
        if let Some(why) = out.failure_reason(case) {
            println!("        -> {why}");
        }
        if verbose || !out.passed {
            for line in out.output.lines() {
                println!("        | {line}");
            }
        }
        if out.passed {
            passed += 1;
        }
    }
    let mips = total_insns as f64 / total_time.max(1e-9) / 1e6;
    println!();
    println!(
        "{passed}/{} passed  ({total_insns} guest instructions, {mips:.1} MIPS on the {backend} backend)",
        cases.len()
    );
    Ok(if passed == cases.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

fn extract(args: &[String]) -> Result<ExitCode, String> {
    let dir = args.first().ok_or("extract needs a target directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
    for case in kestrel_testsuite::CASES {
        let ext = if kestrel_loader::is_nro(case.file) {
            "nro"
        } else {
            "elf"
        };
        let path = std::path::Path::new(dir).join(format!("{}.{ext}", case.name));
        std::fs::write(&path, case.file)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// The folder that is the SD card: `--sd`, or kestrel's portable folder,
/// `user/sdmc` next to the executable (the owner's choice, as Dolphin's
/// portable mode keeps its user folder). It is made when missing; if it
/// cannot be, the program runs with no card inserted.
fn sd_folder(chosen: Option<PathBuf>) -> Option<PathBuf> {
    let dir = match chosen {
        Some(dir) => dir,
        None => {
            let exe = std::env::current_exe().ok()?;
            exe.parent()?.join("user").join("sdmc")
        }
    };
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!("note: no SD card: cannot create {}: {e}", dir.display());
            None
        }
    }
}

fn run(o: Opts) -> Result<ExitCode, String> {
    let file = std::fs::read(&o.path).map_err(|e| format!("cannot read {}: {e}", o.path))?;
    let name = std::path::Path::new(&o.path)
        .file_name()
        .map_or(o.path.clone(), |n| n.to_string_lossy().into_owned());
    let mut proc = Process::load(&file, &name, &o.args).map_err(|e| format!("{}: {e}", o.path))?;
    proc.echo = true;
    proc.services.fs.sd = sd_folder(o.sd);

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
        let n = proc.instructions();
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
        Stop::Abort { pc, reason, result } => {
            let with = result.map_or(String::new(), |r| {
                format!(": result {}", kestrel_hle::results::describe(r))
            });
            let what = kestrel_hle::svc::break_reason(reason);
            eprintln!("guest aborted ({what}) at {pc:#x}{with}");
            Ok(ExitCode::from(3))
        }
        Stop::Fatal { result } => {
            let what = kestrel_hle::results::describe(result);
            eprintln!("guest fatal error {what}");
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
        Some("selftest" | "self-test" | "test") => selftest(&args[1..]),
        Some("extract") => extract(&args[1..]),
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
