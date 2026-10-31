//! The embedded suite with each guest core on its own host thread
//! (`Process::run_threaded`, `PLAN.md` section 3, "Time and cores"). The
//! programs' output does not depend on how the cores interleave, so it
//! must match the same expected output; the thread and synchronisation
//! programs run many times, to catch races between the cores. Each runs
//! on the interpreter and on the default backend (the recompiler on
//! x86_64, one per core).
//!
//! The programs in `HOST_SPEED` are left out: in real time their output
//! depends on the host keeping up, which CI's qemu job for Linux aarch64,
//! or a host busy with other tests' threads, does not. They run threaded
//! in `kestrel selftest --threaded`, and the examples' frames are checked
//! threaded by `scripts/run-examples.sh`.

use kestrel_cpu::BackendKind;
use kestrel_testsuite::{run_case_threaded, CASES, HOST_SPEED};

/// Runs of each thread and synchronisation program on each backend:
/// kestrel's choice, about 20 s on a laptop for both. `kestrel selftest
/// --threaded --repeat <n>` runs more.
const RACE_RUNS: usize = 25;

fn failures(names: &[&str], runs: usize, backend: BackendKind) -> Vec<String> {
    let mut failures = Vec::new();
    let chosen = |name: &str| {
        if names.is_empty() {
            !HOST_SPEED.contains(&name)
        } else {
            names.contains(&name)
        }
    };
    for case in CASES.iter().filter(|c| chosen(c.name)) {
        for run in 0..runs {
            let out = run_case_threaded(case, backend).expect("case should load");
            if let Some(why) = out.failure_reason(case) {
                failures.push(format!(
                    "{} (run {}, {}): {why}",
                    case.name,
                    run + 1,
                    out.backend
                ));
            }
        }
    }
    failures
}

#[test]
fn every_guest_program_matches_expected_output_on_threaded_cores() {
    let mut failures = failures(&[], 1, BackendKind::Interp);
    failures.extend(self::failures(&[], 1, BackendKind::Auto));
    assert!(
        failures.is_empty(),
        "failing cases:\n{}",
        failures.join("\n")
    );
}

#[test]
fn threads_and_synchronisation_hold_on_threaded_cores_run_after_run() {
    // The interpreter, and the default backend: the recompiler on x86_64.
    let mut failures = failures(&["hbthreads", "hbsync"], RACE_RUNS, BackendKind::Interp);
    failures.extend(self::failures(
        &["hbthreads", "hbsync"],
        RACE_RUNS,
        BackendKind::Auto,
    ));
    assert!(
        failures.is_empty(),
        "failing runs:\n{}",
        failures.join("\n")
    );
}
