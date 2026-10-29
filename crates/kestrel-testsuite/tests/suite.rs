//! Runs the embedded suite under `cargo test`, on whatever host CI uses.

use kestrel_cpu::BackendKind;
use kestrel_testsuite::{run_case, CASES};

#[test]
fn every_guest_program_matches_expected_output() {
    let mut failures = Vec::new();
    for case in CASES {
        let out = run_case(case, BackendKind::Auto).expect("case should load");
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
