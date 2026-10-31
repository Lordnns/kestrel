//! The recompiler on the instruction vectors: every case of
//! `interp/int_vectors.rs` (expected values from the Python models, not
//! from kestrel) runs as a one-instruction block, in both calling
//! conventions. Instructions the recompiler does not translate run in the
//! interpreter from inside the block, so every group must pass.

use super::x64::Abi;
use super::Jit;
use crate::interp::int_tests::run_on;
use crate::interp::int_vectors::CASES;

fn all_cases(abi: Abi) {
    let failures: Vec<String> = CASES
        .iter()
        .filter_map(|c| {
            // A new recompiler per case: each case has its own memory.
            let mut jit = Jit::with_abi(abi).unwrap();
            let errs = run_on(c, &mut jit);
            (!errs.is_empty())
                .then(|| format!("{} ({:08x}):\n    {}", c.asm, c.insn, errs.join("\n    ")))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases failed on {abi:?}:\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n")
    );
}

#[test]
fn every_integer_vector_in_the_system_v_convention() {
    all_cases(Abi::SysV);
}

#[test]
fn every_integer_vector_in_the_windows_convention() {
    all_cases(Abi::Win64);
}
