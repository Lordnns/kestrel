//! `kestrel` command-line front end.

#[cfg(feature = "window")]
mod input_config;
mod perf;
mod unattended;
#[cfg(feature = "window")]
mod window;

use kestrel_cpu::{create_backend, interp::Interpreter, BackendKind, CpuBackend};
use kestrel_hle::display::Screen;
use kestrel_hle::input::Source;
use kestrel_hle::{Process, Stop};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
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
    kestrel selftest [-v] [--backend <b>]
                                     Run the built-in ARM64 test programs
    kestrel diff [--backend <b>] [case...]
                                     Run the test programs on the interpreter
                                     and backend <b> side by side and report
                                     the first divergence (default: auto)
    kestrel extract <dir>            Write the built-in test programs to <dir>
    kestrel run <program.elf|program.nro> [options] [-- args...]
    kestrel info                     Show host and backend information

RUN OPTIONS:
    --backend <auto|interp|native|jit>   CPU backend (default: auto)
    --max-steps <n>                      Instruction budget (default: 1e10,
                                         or none with a window)
    --trace                              Print every executed instruction
    --regs                               Dump registers when the program stops
    --stats                              Print instruction count and speed,
                                         and frame times (average, 1% low,
                                         worst) when the program shows frames
    --sd <folder>                        The folder that is the SD card
                                         (default: user/sdmc next to kestrel)
    --headless                           No window for a homebrew NRO; the
                                         window maps keys and gamepads as
                                         user/config/input.cfg says
    --press <button>@<seconds>           Press a button (Switchbrew's name:
                                         A, Plus, Up...) for 1 s, at that
                                         time of the controller's samples;
                                         repeatable; replaces the window's
                                         controls
    --save-frame <file.png>              Save the last frame the program
                                         showed
    -- args...                           Arguments for a homebrew NRO (argv[0]
                                         is its file name)
";

struct Opts {
    path: String,
    backend: BackendKind,
    max_steps: Option<u64>,
    trace: bool,
    regs: bool,
    stats: bool,
    sd: Option<PathBuf>,
    headless: bool,
    presses: Vec<unattended::Press>,
    save_frame: Option<PathBuf>,
    args: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut it = args.iter();
    let path = it.next().ok_or("missing program path")?.clone();
    let mut o = Opts {
        path,
        backend: BackendKind::Auto,
        max_steps: None,
        trace: false,
        regs: false,
        stats: false,
        sd: None,
        headless: false,
        presses: Vec::new(),
        save_frame: None,
        args: Vec::new(),
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--backend" => o.backend = it.next().ok_or("--backend needs a value")?.parse()?,
            "--max-steps" => {
                o.max_steps = Some(
                    it.next()
                        .ok_or("--max-steps needs a value")?
                        .replace('_', "")
                        .parse()
                        .map_err(|e| format!("bad --max-steps: {e}"))?,
                )
            }
            "--trace" => o.trace = true,
            "--regs" => o.regs = true,
            "--stats" => o.stats = true,
            "--sd" => o.sd = Some(it.next().ok_or("--sd needs a folder")?.into()),
            "--headless" => o.headless = true,
            "--press" => o
                .presses
                .push(it.next().ok_or("--press needs a value")?.parse()?),
            "--save-frame" => {
                o.save_frame = Some(it.next().ok_or("--save-frame needs a file")?.into())
            }
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
    match kestrel_cpu::create_backend(BackendKind::Jit) {
        Ok(_) => println!("fast backend      : jit (x86_64 recompiler; --backend jit)"),
        Err(e) => match kestrel_cpu::planned_fast_backend() {
            Some(BackendKind::Jit) | None => println!("fast backend      : none ({e})"),
            Some(k) => println!("fast backend      : {k:?} (planned, not implemented yet)"),
        },
    }
}

fn selftest(args: &[String]) -> Result<ExitCode, String> {
    let mut verbose = false;
    let mut kind = BackendKind::Auto;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--backend" => kind = it.next().ok_or("--backend needs a value")?.parse()?,
            bad => return Err(format!("unknown selftest option '{bad}'")),
        }
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
        let out = kestrel_testsuite::run_case(case, kind)?;
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
    // Without --backend, the other backends this build has run the same
    // programs too, so the report shows each one's speed (and a fast
    // backend that fails fails the self-test).
    let mut all_passed = passed == cases.len();
    if kind == BackendKind::Auto {
        let others: Vec<BackendKind> = [BackendKind::Interp, BackendKind::Jit, BackendKind::Native]
            .into_iter()
            .filter(|&k| create_backend(k).is_ok_and(|(b, _)| b.name() != backend))
            .collect();
        if !others.is_empty() {
            println!();
            println!("Speed by backend (the same programs):");
            println!(
                "  {backend:<7} {passed:>2}/{}  {mips:>7.1} MIPS",
                cases.len()
            );
        }
        for k in others {
            let (mut ok, mut insns, mut time, mut name) = (0, 0u64, 0f64, "?");
            for case in cases {
                let out = kestrel_testsuite::run_case(case, k)?;
                name = out.backend;
                insns += out.instructions;
                time += out.elapsed.as_secs_f64();
                if out.passed {
                    ok += 1;
                } else {
                    println!(
                        "  FAIL  {} on {name}: {}",
                        case.name,
                        out.failure_reason(case).unwrap_or_default()
                    );
                }
            }
            all_passed &= ok == cases.len();
            let mips = insns as f64 / time.max(1e-9) / 1e6;
            println!("  {name:<7} {ok:>2}/{}  {mips:>7.1} MIPS", cases.len());
        }
    }
    Ok(if all_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

/// `kestrel diff`: the differential tester on the self-test programs.
fn diff(args: &[String]) -> Result<ExitCode, String> {
    let mut kind = BackendKind::Auto;
    let mut names = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--backend" => kind = it.next().ok_or("--backend needs a value")?.parse()?,
            other if other.starts_with('-') => {
                return Err(format!("unknown diff option '{other}'"))
            }
            name => names.push(name),
        }
    }
    let cases: Vec<_> = kestrel_testsuite::CASES
        .iter()
        .filter(|c| names.is_empty() || names.contains(&c.name))
        .collect();
    if let Some(bad) = names
        .iter()
        .find(|n| !kestrel_testsuite::CASES.iter().any(|c| c.name == **n))
    {
        return Err(format!("no test program named '{bad}'"));
    }
    let (_, note) = create_backend(kind)?;
    if let Some(note) = note {
        eprintln!("note: {note}");
    }
    let reference = || -> Box<dyn CpuBackend> { Box::new(Interpreter::new()) };
    let candidate = || {
        create_backend(kind)
            .expect("the backend was created above")
            .0
    };
    let mut diverged = 0;
    for case in &cases {
        let r = kestrel_testsuite::diff::diff_case(case, &reference, &candidate)?;
        match &r.divergence {
            None => println!(
                "  SAME      {:<11} {:>12} instr  {} and {}, {:?}",
                case.name, r.instructions, r.backends.0, r.backends.1, r.stop
            ),
            Some(d) => {
                diverged += 1;
                println!("  DIVERGED  {:<11} {d}", case.name);
            }
        }
    }
    println!();
    println!(
        "{diverged} of {} programs diverged (counted clock, 52 instructions a tick)",
        cases.len()
    );
    Ok(if diverged == 0 {
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

/// kestrel's portable user folder: `user` next to the executable.
fn user_folder() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("user"))
}

/// A time as "2026-10-30 15:04:05", UTC, for the log.
fn utc(secs: u64) -> String {
    let (days, s) = ((secs / 86400) as i64, secs % 86400);
    // Days since 1970-01-01 to a date (proleptic Gregorian, eras of 400
    // years from March 1).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// Add `line` to the log of unimplemented calls at `path`, making its
/// folder when missing.
fn log_unimplemented(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

/// The folder that is the SD card: `--sd`, or kestrel's portable folder,
/// `user/sdmc` next to the executable (the owner's choice, as Dolphin's
/// portable mode keeps its user folder). It is made when missing; if it
/// cannot be, the program runs with no card inserted.
fn sd_folder(chosen: Option<PathBuf>) -> Option<PathBuf> {
    let dir = match chosen {
        Some(dir) => dir,
        None => user_folder()?.join("sdmc"),
    };
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!("note: no SD card: cannot create {}: {e}", dir.display());
            None
        }
    }
}

/// Where a homebrew program is on the SD card, as argv[0] tells it (the
/// homebrew menu passes the program's path there, and libnx starts in its
/// folder and finds the files bundled in it from it). A program inside the
/// card's folder is at its real place. One elsewhere, in the owner's game
/// library anywhere on the host, shows read-only at
/// `sdmc:/switch/<name>`, where the homebrew menu keeps programs (the
/// owner's choice); what it writes still goes to the card's folder.
fn card_place(
    program: &Path,
    sd: Option<&Path>,
    name: &str,
) -> (String, Option<kestrel_hle::service::fs::Program>) {
    let inside = sd.and_then(|sd| {
        let rel = program.canonicalize().ok()?;
        let rel = rel.strip_prefix(sd.canonicalize().ok()?).ok()?;
        let parts: Option<Vec<&str>> = rel.iter().map(|c| c.to_str()).collect();
        let path = format!("/{}", parts?.join("/"));
        let mut raw = path.clone().into_bytes();
        raw.push(0);
        kestrel_hle::service::fs::parse_path(&raw).ok()?;
        Some(path)
    });
    match inside {
        Some(path) => (format!("sdmc:{path}"), None),
        None => (
            format!("sdmc:/switch/{name}"),
            Some(kestrel_hle::service::fs::Program {
                parts: vec!["switch".into(), name.into()],
                host: program.to_path_buf(),
            }),
        ),
    }
}

/// The input settings: `user/config/input.cfg`, written with the defaults
/// when missing.
#[cfg(feature = "window")]
fn input_config() -> Result<input_config::InputConfig, String> {
    let Some(path) = user_folder().map(|u| u.join("config").join("input.cfg")) else {
        return input_config::InputConfig::parse(input_config::DEFAULT);
    };
    if !path.exists() {
        let made = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, input_config::DEFAULT));
        if let Err(e) = made {
            eprintln!("note: cannot write {}: {e}", path.display());
            return input_config::InputConfig::parse(input_config::DEFAULT);
        }
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    input_config::InputConfig::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// `kestrel run`: a homebrew NRO gets a window (the owner's choice) unless
/// `--headless`, or kestrel was built without the window feature.
fn run(o: Opts) -> Result<ExitCode, String> {
    let file = std::fs::read(&o.path).map_err(|e| format!("cannot read {}: {e}", o.path))?;
    let name = std::path::Path::new(&o.path)
        .file_name()
        .map_or(o.path.clone(), |n| n.to_string_lossy().into_owned());
    let o = Arc::new(o);
    #[cfg(feature = "window")]
    if kestrel_loader::is_nro(&file) && !o.headless {
        let config = input_config()?;
        let title = format!("kestrel - {name}");
        let (opts, f, n) = (o.clone(), file.clone(), name.clone());
        let windowed = window::run(title, config, move |source, screen, interrupt| {
            emulate(&opts, &f, &n, Some((source, screen, interrupt)))
        });
        match windowed {
            Ok(result) => return result,
            // No display here (a server, an SSH session): run without one.
            Err(e) => eprintln!("note: no window ({e}); running without one"),
        }
    }
    emulate(&o, &file, &name, None)
}

/// What a window gives the program: the controller, the screen, and the
/// flag that stops it.
type Windowed = (Box<dyn Source>, Box<dyn Screen>, Arc<AtomicBool>);

/// Load and run the program, in a window when one is given, and report how
/// it stopped.
fn emulate(
    o: &Opts,
    file: &[u8],
    name: &str,
    window: Option<Windowed>,
) -> Result<ExitCode, String> {
    let sd = sd_folder(o.sd.clone());
    let (argv0, program) = if kestrel_loader::is_nro(file) {
        card_place(Path::new(&o.path), sd.as_deref(), name)
    } else {
        (name.to_string(), None)
    };
    let mut proc = Process::load(file, &argv0, &o.args).map_err(|e| format!("{}: {e}", o.path))?;
    proc.echo = true;
    proc.services.fs.sd = sd;
    proc.services.fs.program = program;
    // With a window someone is using the program: it runs on the host's
    // clock, in real time, for as long as they like (the owner's decision
    // of 2026-10-30). Without one, time is counted, so runs repeat.
    let windowed = window.is_some();
    if windowed {
        proc.cpu.clock = kestrel_cpu::Clock::host();
    }
    if let Some((input, screen, interrupt)) = window {
        proc.input = Some(input);
        proc.screen = Some(screen);
        proc.interrupt = Some(interrupt);
    }
    if !o.presses.is_empty() {
        proc.input = Some(Box::new(unattended::Scripted(o.presses.clone())));
    }
    let last_frame = std::rc::Rc::new(std::cell::RefCell::new(None));
    if o.save_frame.is_some() {
        proc.screen = Some(Box::new(unattended::Keep {
            last: last_frame.clone(),
            inner: proc.screen.take(),
        }));
    }
    let frame_times = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    if o.stats {
        proc.screen = Some(Box::new(perf::Timed {
            times: frame_times.clone(),
            inner: proc.screen.take(),
        }));
    }
    let max_steps = o
        .max_steps
        .unwrap_or(if windowed { u64::MAX } else { 10_000_000_000 });

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
    let stop = proc.run(backend.as_mut(), max_steps);
    let elapsed = t0.elapsed();
    if let Some(path) = &o.save_frame {
        match &*last_frame.borrow() {
            Some(frame) => std::fs::write(path, unattended::png(frame))
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?,
            None => eprintln!("note: the program showed no frame to save"),
        }
    }

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
        if let Some(summary) = perf::Summary::of(&frame_times.borrow()) {
            eprintln!("[{}] {}", backend.name(), summary.describe());
        }
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
        Stop::Interrupted => {
            eprintln!("stopped: the window was closed");
            Ok(ExitCode::SUCCESS)
        }
        Stop::Unimplemented { what, pc } => {
            eprintln!("unimplemented: {what}, at pc {pc:#x}");
            // Every run that reaches something kestrel lacks adds a line,
            // so the log lists what programs need next.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let line = format!("{} UTC  kestrel {VERSION}  {name}  {what}", utc(now));
            if let Some(path) = user_folder().map(|u| u.join("logs").join("unimplemented.log")) {
                match log_unimplemented(&path, &line) {
                    Ok(()) => eprintln!("recorded in {}", path.display()),
                    Err(e) => eprintln!("note: cannot write {}: {e}", path.display()),
                }
            }
            Ok(ExitCode::from(5))
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
        Some("diff") => diff(&args[1..]),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_times_are_utc_dates() {
        // Python: datetime.fromtimestamp(t, timezone.utc).
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(1_749_081_600), "2025-06-05 00:00:00");
        assert_eq!(utc(951_825_845), "2000-02-29 12:04:05");
        assert_eq!(utc(4_102_444_799), "2099-12-31 23:59:59");
    }

    #[test]
    fn programs_are_placed_on_the_card() {
        let dir = std::env::temp_dir().join(format!("kestrel-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (sd, library) = (dir.join("sdmc"), dir.join("games"));
        std::fs::create_dir_all(sd.join("switch").join("tools")).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        let inside = sd.join("switch").join("tools").join("a.nro");
        let outside = library.join("b.nro");
        std::fs::write(&inside, b"").unwrap();
        std::fs::write(&outside, b"").unwrap();
        assert_eq!(
            card_place(&inside, Some(&sd), "a.nro"),
            ("sdmc:/switch/tools/a.nro".to_string(), None)
        );
        let (argv0, program) = card_place(&outside, Some(&sd), "b.nro");
        assert_eq!(argv0, "sdmc:/switch/b.nro");
        let program = program.unwrap();
        assert_eq!(
            (program.parts, program.host),
            (
                vec!["switch".to_string(), "b.nro".to_string()],
                outside.clone()
            )
        );
        // With no card, the program still shows where the menu keeps it.
        assert_eq!(card_place(&outside, None, "b.nro").0, "sdmc:/switch/b.nro");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_log_gains_a_line_per_entry() {
        let dir = std::env::temp_dir().join(format!("kestrel-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("unimplemented.log");
        log_unimplemented(&path, "first").unwrap();
        log_unimplemented(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
