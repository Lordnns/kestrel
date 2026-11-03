//! The recompiler on the instruction vectors: every case of
//! `interp/int_vectors.rs` and `interp/neon_vectors.rs` (expected values
//! from the Python models, not from kestrel) runs as a one-instruction
//! block, in both calling conventions. Instructions the recompiler does
//! not translate run in the interpreter from inside the block, so every
//! group must pass. The scalar floating-point tests (`interp/fp_tests.rs`)
//! run on the recompiler too.

use super::x64::Abi;
use super::Jit;
use crate::interp::{int_tests, int_vectors, neon_tests, neon_vectors};

/// Run every case on a new recompiler (each case has its own memory) and
/// report all failures together.
fn all_cases<C>(
    abi: Abi,
    cases: &[C],
    name: impl Fn(&C) -> (&'static str, u32),
    run: impl Fn(&C, &mut Jit) -> Vec<String>,
) {
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|c| {
            let mut jit = Jit::with_abi(abi).unwrap();
            let errs = run(c, &mut jit);
            let (asm, insn) = name(c);
            (!errs.is_empty()).then(|| format!("{asm} ({insn:08x}):\n    {}", errs.join("\n    ")))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases failed on {abi:?}:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

fn integer(abi: Abi) {
    all_cases(
        abi,
        int_vectors::CASES,
        |c| (c.asm, c.insn),
        |c, j| int_tests::run_on(c, j),
    );
}

fn neon(abi: Abi) {
    all_cases(
        abi,
        neon_vectors::CASES,
        |c| (c.asm, c.insn),
        |c, j| neon_tests::run_on(c, j),
    );
}

#[test]
fn every_integer_vector_in_the_system_v_convention() {
    integer(Abi::SysV);
}

#[test]
fn every_integer_vector_in_the_windows_convention() {
    integer(Abi::Win64);
}

#[test]
fn every_neon_vector_in_the_system_v_convention() {
    neon(Abi::SysV);
}

#[test]
fn every_neon_vector_in_the_windows_convention() {
    neon(Abi::Win64);
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
            (s.x, s.sp, s.pc, s.flags, s.retired, s.v),
            (s0.x, s0.sp, s0.pc, s0.flags, s0.retired, s0.v),
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

/// Bytes `from..from + 16` of the sequence 0, 1, 2, ... as a Q register.
fn bytes_from(from: u8) -> u128 {
    u128::from_le_bytes(std::array::from_fn(|k| from + k as u8))
}

#[test]
fn simd_accesses_crossing_a_page_see_the_bytes_in_order() {
    let words = [
        0xad00_0c62, // stp q2, q3, [x3]
        0x3cc0_8060, // ldur q0, [x3, #8]
        0x3cc0_3061, // ldur q1, [x3, #3]
        0x6d41_1464, // ldp d4, d5, [x3, #16]
        BRK,
    ];
    // The 32 bytes straddle a page boundary at each of these offsets.
    for at in [0xff0, 0xff8, 0xffc, 0xfe8, 0xfe3, 0x10] {
        let (exits, s) = same(
            &words,
            |s| {
                s.v[2] = bytes_from(0);
                s.v[3] = bytes_from(16);
                s.x[3] = STACK_BASE + at;
            },
            100,
        );
        assert_eq!(exits, [Exit::Breakpoint(0)], "at {at:#x}");
        assert_eq!((s.v[0], s.v[1]), (bytes_from(8), bytes_from(3)));
        let d = |from| bytes_from(from) & u128::from(u64::MAX);
        assert_eq!((s.v[4], s.v[5]), (d(16), d(24)), "at {at:#x}");
    }
}

#[test]
fn simd_and_fp_loads_and_stores_are_translated() {
    // Every vector case of the load and store class (op0 x1x0) on a SIMD
    // and FP register, except the structure loads and stores (LD1..ST4).
    let simd_ldst = |i: u32| i >> 25 & 0b101 == 0b100 && i >> 26 & 1 == 1 && i >> 27 & 7 != 0b001;
    let mut n = 0;
    for c in int_vectors::CASES.iter().filter(|c| simd_ldst(c.insn)) {
        let (_, m) = setup(&[c.insn]);
        let (block, _) = super::frontend::translate(&m, CODE, 1).unwrap();
        let interp = block
            .ops
            .iter()
            .any(|op| matches!(op, super::ir::Op::Interp { .. }));
        assert!(
            !interp,
            "{} ({:08x}) runs in the interpreter",
            c.asm, c.insn
        );
        n += 1;
    }
    assert!(n >= 30, "{n} cases");
}

#[test]
fn simd_faults_leave_what_the_interpreter_leaves() {
    // The stack region ends at `end`, with nothing mapped after it.
    let end = STACK_BASE + 0x1_0000;
    let init = |x3: u64| {
        move |s: &mut CpuState| {
            for k in 0..4 {
                s.v[k] = bytes_from(0x40 + 16 * k as u8);
            }
            s.x[3] = x3;
        }
    };
    let faults = |exits: &[Exit]| matches!(exits, [Exit::Fault { pc: CODE, .. }]);
    // A Q load whose high half is not mapped writes nothing.
    for x3 in [end - 8, end - 4, end - 12] {
        let (exits, s) = same(&[0x3dc0_0060, BRK], init(x3), 100); // ldr q0, [x3]
        assert!(faults(&exits), "{exits:?}");
        assert_eq!(s.v[0], bytes_from(0x40));
    }
    // A Q store stores the bytes before the unmapped page (`same` compares
    // the memory with the interpreter's).
    for x3 in [end - 8, end - 4, end - 12] {
        let (exits, _) = same(&[0x3d80_0061, BRK], init(x3), 100); // str q1, [x3]
        assert!(faults(&exits), "{exits:?}");
    }
    // A pair whose second register faults has written the first.
    let (exits, s) = same(&[0xad40_0460, BRK], init(end - 16), 100); // ldp q0, q1, [x3]
    assert!(faults(&exits), "{exits:?}");
    assert_eq!((s.v[0], s.v[1]), (0, bytes_from(0x50)));
    let (exits, _) = same(&[0xad00_0c62, BRK], init(end - 16), 100); // stp q2, q3, [x3]
    assert!(faults(&exits), "{exits:?}");
}

#[test]
fn dc_zva_zeroes_its_aligned_block_or_faults_writing_nothing() {
    let words = [
        0xf900_0080, // str x0, [x4]
        0xf900_1c80, // str x0, [x4, #56]
        0xf900_00a0, // str x0, [x5]
        0xf900_00c0, // str x0, [x6]
        0xd50b_7423, // dc zva, x3
        0xf940_0087, // ldr x7, [x4]
        0xf940_1c8a, // ldr x10, [x4, #56]
        0xf940_00a8, // ldr x8, [x5]
        0xf940_00c9, // ldr x9, [x6]
        BRK,
    ];
    let block = STACK_BASE + 0x1c0;
    let init = |x3: u64| {
        move |s: &mut CpuState| {
            s.x[0] = 0x1122_3344_5566_7788;
            s.x[3] = x3;
            s.x[4] = block;
            s.x[5] = block + 64;
            s.x[6] = block - 8;
        }
    };
    // Unaligned: the 64-byte block holding the address, not a byte more.
    let (exits, s) = same(&words, init(block + 0x27), 100);
    assert_eq!(exits, [Exit::Breakpoint(0)]);
    assert_eq!((s.x[7], s.x[10]), (0, 0));
    assert_eq!((s.x[8], s.x[9]), (s.x[0], s.x[0]));
    // Not writable (the code) or not mapped: a fault at the DC ZVA, after
    // the four stores.
    for x3 in [CODE + 0x10, 0x5000_0000] {
        let (exits, s) = same(&words, init(x3), 100);
        assert!(
            matches!(exits[..], [Exit::Fault { pc, .. }] if pc == CODE + 16),
            "{exits:?}"
        );
        assert_eq!(s.retired, 4);
    }
}

#[test]
fn the_counters_see_the_instructions_retired_before_them() {
    // On the counted clock a tick is 52 instructions (PLAN.md section 3):
    // retired / 52 + skipped. 1037 + 3 = 1040 = 20 x 52 at the first MRS;
    // 1040 + 1 + 51 = 1092 = 21 x 52 at the second.
    let mut words = vec![0x9100_0442; 3]; // add x2, x2, #1
    words.push(0xd53b_e020); // mrs x0, cntpct_el0
    words.extend([0x9100_0442; 51]);
    words.push(0xd53b_e041); // mrs x1, cntvct_el0
    words.push(BRK);
    let init = |s: &mut CpuState| {
        s.retired = 1037;
        s.clock = crate::Clock::Counted { skipped: 5 };
    };
    for budget in [10_000, 30] {
        let (exits, s) = same(&words, init, budget);
        assert_eq!(exits.last(), Some(&Exit::Breakpoint(0)));
        assert_eq!((s.x[0], s.x[1], s.x[2]), (25, 26, 54));
        assert_eq!(s.retired, 1037 + 56);
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

// The interpreter's tests of guest cores on several host threads
// (`interp/cores_tests.rs`), on the recompiler: its translated exclusives
// (a compare-and-swap of the value LDXR read) must hold between host
// threads as the interpreter's do.

fn jit() -> Box<dyn CpuBackend + Send> {
    Box::new(Jit::new().unwrap())
}

#[test]
fn another_cores_store_fails_the_translated_store_exclusive() {
    crate::interp::cores_tests::another_cores_store_fails_the_store_exclusive_on(jit);
}

#[test]
fn translated_guest_atomics_hold_between_host_threads() {
    crate::interp::cores_tests::guest_atomics_hold_between_host_threads_on(jit);
}
