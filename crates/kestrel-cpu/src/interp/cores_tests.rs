//! Guest cores on several host threads sharing one memory (`PLAN.md`
//! section 3, "Time and cores"): exclusives and atomics must hold between
//! them. Encodings from clang (`--target=aarch64-none-elf
//! -march=armv8.1-a`) and llvm-objdump, from the assembly in the comments.

use super::tests::setup;
use super::Interpreter;
use crate::{CpuBackend, CpuState, Exit};
use kestrel_core::{Memory, Perm};

const DATA: u64 = 0x30_0000;
const BRK: u32 = 0xd420_0000;

/// Makes the backend the tests run on; the recompiler's tests run them too.
pub(crate) type Make = fn() -> Box<dyn CpuBackend + Send>;

fn interp() -> Box<dyn CpuBackend + Send> {
    Box::new(Interpreter::new())
}

#[test]
fn another_cores_store_fails_the_store_exclusive() {
    another_cores_store_fails_the_store_exclusive_on(interp);
}

#[test]
fn guest_atomics_hold_between_host_threads() {
    guest_atomics_hold_between_host_threads_on(interp);
}

/// A store-exclusive fails when another core wrote a new value since the
/// load-exclusive, and succeeds when nothing did.
pub(crate) fn another_cores_store_fails_the_store_exclusive_on(make: Make) {
    // ldxr x2, [x0]; stxr w3, x2, [x0]; brk #0
    let (mut a, m) = setup(&[0xc85f7c02, 0xc8037c02, BRK]);
    m.map(DATA, 0x1000, Perm::RW, "data").unwrap();
    m.write64(DATA, 5).unwrap();
    let mut b = a.clone();
    a.x[0] = DATA;
    let mut core = make();
    assert_eq!(core.run(&mut a, &m, 1), Exit::StepLimit);
    assert_eq!(a.x[2], 5);
    // Core B: str x5, [x0]; brk #0, between A's two instructions.
    m.map(0x40_0000, 0x1000, Perm::RX, "b").unwrap();
    m.poke_bytes(
        0x40_0000,
        &[0xf9000005u32, BRK].map(u32::to_le_bytes).concat(),
    )
    .unwrap();
    (b.pc, b.x[0], b.x[5]) = (0x40_0000, DATA, 6);
    assert_eq!(make().run(&mut b, &m, 10), Exit::Breakpoint(0));
    assert_eq!(core.run(&mut a, &m, 10), Exit::Breakpoint(0));
    assert_eq!(a.x[3], 1, "the store-exclusive failed");
    assert_eq!(m.read64(DATA), Ok(6), "and did not write");
    // Unhindered, the same pair succeeds.
    a.pc = 0x1_0000;
    assert_eq!(core.run(&mut a, &m, 10), Exit::Breakpoint(0));
    assert_eq!((a.x[3], a.x[2]), (0, 6));
}

/// Four host threads, each running a guest core on the interpreter, all
/// increment the same counters with every kind of guest atomic. No
/// increment is lost, and 128-bit counters carry into their high half.
pub(crate) fn guest_atomics_hold_between_host_threads_on(make: Make) {
    const THREADS: u64 = 4;
    const ROUNDS: u64 = 20_000;
    // loop: 1: ldaxr x2,[x0]; add x2,x2,#1; stlxr w3,x2,[x0]; cbnz w3,1b
    //       mov x4,#1; ldaddal x4,x5,[x6]
    //       2: ldr x9,[x7]; add x10,x9,#1; mov x11,x9; casal x11,x10,[x7];
    //          cmp x11,x9; b.ne 2b
    //       3: ldaxp x12,x13,[x8]; adds x12,x12,#1; adc x13,x13,xzr;
    //          stlxp w3,x12,x13,[x8]; cbnz w3,3b
    //       4: ldp x14,x15,[x20]; adds x16,x14,#1; adc x17,x15,xzr;
    //          mov x21,x14; mov x22,x15; caspal x14,x15,x16,x17,[x20];
    //          cmp x14,x21; ccmp x15,x22,#0,eq; b.ne 4b
    //       subs x1,x1,#1; b.ne loop; brk #0
    let code = [
        0xc85ffc02, 0x91000442, 0xc803fc02, 0x35ffffa3, 0xd2800024, 0xf8e400c5, 0xf94000e9,
        0x9100052a, 0xaa0903eb, 0xc8ebfcea, 0xeb09017f, 0x54ffff61, 0xc87fb50c, 0xb100058c,
        0x9a1f01ad, 0xc823b50c, 0x35ffff83, 0xa9403e8e, 0xb10005d0, 0x9a1f01f1, 0xaa0e03f5,
        0xaa0f03f6, 0x486efe90, 0xeb1501df, 0xfa5601e0, 0x54ffff01, 0xf1000421, 0x54fffca1, BRK,
    ];
    let (start, m): (CpuState, Memory) = setup(&code);
    m.map(DATA, 0x1000, Perm::RW, "data").unwrap();
    let (exclusive, ldadd, cas, pair, casp) = (DATA, DATA + 8, DATA + 16, DATA + 32, DATA + 48);
    // The 128-bit counters start just below the carry into the high half.
    let low = u64::MAX - THREADS * ROUNDS / 2;
    m.write128(pair, u128::from(low)).unwrap();
    m.write128(casp, u128::from(low)).unwrap();
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            let (m, mut cpu) = (&m, start.clone());
            cpu.x[0] = exclusive;
            cpu.x[1] = ROUNDS;
            (cpu.x[6], cpu.x[7], cpu.x[8], cpu.x[20]) = (ldadd, cas, pair, casp);
            s.spawn(move || {
                let mut core = make();
                assert_eq!(core.run(&mut cpu, m, u64::MAX), Exit::Breakpoint(0));
            });
        }
    });
    let n = THREADS * ROUNDS;
    assert_eq!(m.read64(exclusive), Ok(n), "LDAXR/STLXR");
    assert_eq!(m.read64(ldadd), Ok(n), "LDADDAL");
    assert_eq!(m.read64(cas), Ok(n), "CASAL");
    let want = u128::from(low) + u128::from(n);
    assert_eq!(m.read128(pair), Ok(want), "LDAXP/STLXP");
    assert_eq!(m.read128(casp), Ok(want), "CASPAL");
}
