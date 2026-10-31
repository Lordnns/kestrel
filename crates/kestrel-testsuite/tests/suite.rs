//! Runs the embedded suite under `cargo test`, on whatever host CI uses.

use kestrel_cpu::BackendKind;
use kestrel_testsuite::{run_case, CASES};

#[test]
fn every_guest_program_matches_expected_output() {
    let mut failures = Vec::new();
    for case in CASES {
        let out = run_case(case, BackendKind::Interp).expect("case should load");
        if let Some(why) = out.failure_reason(case) {
            failures.push(format!("{}: {why}", case.name));
        }
    }
    assert!(
        failures.is_empty(),
        "failing cases:\n{}",
        failures.join("\n")
    );
}

/// The same on the x86_64 recompiler, where it is built.
#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "windows", target_os = "linux")
))]
#[test]
fn every_guest_program_matches_expected_output_on_the_jit() {
    let mut failures = Vec::new();
    for case in CASES {
        let out = run_case(case, BackendKind::Jit).expect("case should load");
        assert_eq!(out.backend, "jit");
        if let Some(why) = out.failure_reason(case) {
            failures.push(format!("{}: {why}", case.name));
        }
    }
    assert!(
        failures.is_empty(),
        "failing cases:\n{}",
        failures.join("\n")
    );
}
