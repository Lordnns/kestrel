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

// Small programs run on the interpreter (the reference) and on the
// recompiler with the same budgets; exits and state must match. Encodings
// from `llvm-mc --triple=aarch64`, with the assembly in each comment.

use crate::interp::tests::setup;
use crate::interp::Interpreter;
use crate::{CpuBackend, CpuState, Exit};
use kestrel_core::{Memory, Perm};

/// Where `setup` maps the code and the read-write stack region.
const CODE: u64 = 0x1_0000;
const STACK_BASE: u64 = 0x20_0000;

/// Run on `backend` with `budget` a call until it stops; the exits.
fn run_all(b: &mut dyn CpuBackend, s: &mut CpuState, m: &mut Memory, budget: u64) -> Vec<Exit> {
    let mut exits = Vec::new();
    loop {
        let e = b.run(s, m, budget);
        exits.push(e.clone());
        if e != Exit::StepLimit || exits.len() > 100_000 {
            return exits;
        }
    }
}

/// The interpreter's exits and final state, after checking the recompiler
/// in both conventions gives the same.
fn same(words: &[u32], init: impl Fn(&mut CpuState), budget: u64) -> (Vec<Exit>, CpuState) {
    let (mut s0, mut m0) = setup(words);
    init(&mut s0);
    let exits0 = run_all(&mut Interpreter::new(), &mut s0, &mut m0, budget);
    for abi in [Abi::SysV, Abi::Win64] {
        let (mut s, mut m) = setup(words);
        init(&mut s);
        let exits = run_all(&mut Jit::with_abi(abi).unwrap(), &mut s, &mut m, budget);
        assert_eq!(exits, exits0, "{abi:?}, budget {budget}");
        assert_eq!(
            (s.x, s.sp, s.pc, s.flags, s.retired),
            (s0.x, s0.sp, s0.pc, s0.flags, s0.retired),
            "{abi:?}, budget {budget}"
        );
        let stack = |m: &Memory| m.read_vec(STACK_BASE, 0x1_0000).unwrap();
        assert!(stack(&m) == stack(&m0), "{abi:?}: memory differs");
    }
    (exits0, s0)
}

const BRK: u32 = 0xd420_0000; // brk #0

#[test]
fn a_fault_part_way_through_a_block_stops_at_its_instruction() {
    let words = [
        0xd280_00a0, // mov x0, #5
        0x9100_0401, // add x1, x0, #1
        0xf940_0062, // ldr x2, [x3]
        0x9100_0804, // add x4, x0, #2
        BRK,
    ];
    let (exits, s) = same(&words, |s| s.x[3] = 0x5000_0000, 100);
    assert!(
        matches!(exits[..], [Exit::Fault { pc, .. }] if pc == CODE + 8),
        "{exits:?}"
    );
    assert_eq!((s.retired, s.x[1], s.x[4]), (2, 6, 0));
}

#[test]
fn accesses_crossing_a_page_and_halfwords_take_the_right_path() {
    let words = [
        0xf900_0060, // str x0, [x3]
        0xf940_0065, // ldr x5, [x3]
        0x7940_0467, // ldrh w7, [x3, #2]
        0x7900_0c60, // strh w0, [x3, #6]
        BRK,
    ];
    for at in [0xffc, 0xff8, 0x10] {
        let (exits, s) = same(
            &words,
            |s| {
                s.x[0] = 0x1122_3344_5566_7788;
                s.x[3] = STACK_BASE + at;
            },
            100,
        );
        assert_eq!(exits, [Exit::Breakpoint(0)]);
        assert_eq!((s.x[5], s.x[7]), (0x1122_3344_5566_7788, 0x5566));
    }
}

#[test]
fn a_store_to_code_faults() {
    let words = [
        0xf900_00c0, // str x0, [x6]
        BRK,
    ];
    let (exits, _) = same(&words, |s| s.x[6] = CODE, 100);
    assert!(
        matches!(exits[..], [Exit::Fault { pc: CODE, .. }]),
        "{exits:?}"
    );
}

#[test]
fn budgets_are_exact_with_chained_blocks() {
    let words = [
        0xd280_0000, // mov x0, #0
        0x9100_0400, // add x0, x0, #1
        0xf10f_a01f, // cmp x0, #1000
        0x54ff_ffc1, // b.ne #-8
        0xd280_00e1, // mov x1, #7
        BRK,
    ];
    for budget in [1, 2, 3, 7, 63, 64, 65, 129, 1000, 19_200] {
        let (exits, s) = same(&words, |_| {}, budget);
        assert_eq!(exits.last(), Some(&Exit::Breakpoint(0)));
        assert_eq!((s.x[0], s.x[1], s.retired), (1000, 7, 3002));
    }
}

#[test]
fn code_written_again_is_translated_again() {
    let (mut s, m) = setup(&[0xd280_0020, BRK]); // mov x0, #1
    let mut jit = Jit::new().unwrap();
    assert_eq!(jit.run(&mut s, &m, 10), Exit::Breakpoint(0));
    assert_eq!(s.x[0], 1);
    // Through a permission change, which moves the memory's change count.
    assert!(m.protect(CODE, Perm::RW));
    m.write32(CODE, 0xd280_0040).unwrap(); // mov x0, #2
    assert!(m.protect(CODE, Perm::RX));
    s.pc = CODE;
    assert_eq!(jit.run(&mut s, &m, 10), Exit::Breakpoint(0));
    assert_eq!(s.x[0], 2);
    // Written behind the page table's back: the OS layer says so.
    m.poke_bytes(CODE, &0xd280_0020u32.to_le_bytes()).unwrap();
    jit.invalidate(CODE, 4);
    s.pc = CODE;
    assert_eq!(jit.run(&mut s, &m, 10), Exit::Breakpoint(0));
    assert_eq!(s.x[0], 1);
}

#[test]
fn an_access_crossing_into_a_page_elsewhere_in_the_arena_is_whole() {
    let words = [
        0xf900_0060, // str x0, [x3]
        0xf940_0065, // ldr x5, [x3]
        BRK,
    ];
    // Two neighbouring guest pages whose frames are not neighbours: a
    // page mapped in between takes the frame after the first.
    let (a, b) = (0x40_0000, 0x40_1000);
    for abi in [Abi::SysV, Abi::Win64] {
        let (mut s, m) = setup(&words);
        m.map(a, 0x1000, Perm::RW, "a").unwrap();
        m.map(0x50_0000, 0x1000, Perm::RW, "between").unwrap();
        m.map(b, 0x1000, Perm::RW, "b").unwrap();
        m.write64(0x50_0000, 0x5555_5555_5555_5555).unwrap();
        s.x[0] = 0x1122_3344_5566_7788;
        s.x[3] = b - 4;
        let exit = Jit::with_abi(abi).unwrap().run(&mut s, &m, 10);
        assert_eq!(exit, Exit::Breakpoint(0));
        assert_eq!(s.x[5], 0x1122_3344_5566_7788, "{abi:?}");
        assert_eq!(m.read32(b).unwrap(), 0x1122_3344, "{abi:?}");
        assert_eq!(m.read64(0x50_0000).unwrap(), 0x5555_5555_5555_5555);
    }
}

#[test]
fn a_chained_loop_sees_code_another_core_rewrote() {
    // One core spins in a block chained to itself; another rewrites the
    // block's code and then maps a page elsewhere, which moves the
    // memory's change count, as a system call on another core does. The
    // spinning core must leave the chain, check the code again and stop
    // at the BRK, well before its budget runs out.
    let (mut s, m) = setup(&[0x1400_0000]); // b .
    let m = std::sync::Arc::new(m);
    let other = {
        let m = m.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            m.poke_bytes(CODE, &BRK.to_le_bytes()).unwrap();
            m.map(0x60_0000, 0x1000, Perm::RW, "elsewhere").unwrap();
        })
    };
    let exit = Jit::new().unwrap().run(&mut s, &m, 5_000_000_000);
    other.join().unwrap();
    assert_eq!((exit, s.pc), (Exit::Breakpoint(0), CODE));
    // Soon after the change, not when the budget ran low (a chain that
    // ignores the change count stops only then, after 5 billion).
    assert!(s.retired < 1_000_000_000, "{} instructions", s.retired);
}
