# Roadmap

The plan of record is [PLAN.md](../PLAN.md). This file tracks where the code
stands against it.

## Status (audit of 2026-10-29, commit 3e2bfd7)

### Verified in this audit

Host: Windows 10 x86_64, rustc 1.93.1.

| Check | Result |
|-------|--------|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo test --workspace` | pass: 23 tests (core 4, cpu 17, loader 1, embedded suite 1) |
| `kestrel selftest` (source build) | 6/6, ~142 MIPS on `interp` |
| `kestrel selftest` (downloaded `v0.1.4` Windows x86_64 package) | 6/6 |
| C test programs rebuilt with NEON allowed (`-O2`/`-O3`, no `-mgeneral-regs-only`) | hello, vm, bigint pass; `algos` stops at `cmgt v3.2d` (`4ee63463`); `sha256 -O2` stops at `ld4 {v0.16b-v3.16b}` (`4c400100`) |

Release `v0.1.4` (GitHub Actions) built and published all five packages;
CI ran natively on Linux x86_64, macOS ARM and Windows x86_64, and under
qemu for Linux ARM64.

### Phase 0: Foundation: **done**

| Task | Status | Evidence |
|------|--------|----------|
| Workspace split | done | crates `kestrel-core`, `-cpu`, `-loader`, `-hle`, `-testsuite`, `-cli` |
| Portable interpreter (integer, branches, all load/store forms, exclusives, atomics, user sysregs, SIMD moves) | done | `interp/{dp_imm,dp_reg,branch,ldst,system,simd}.rs`; CASP, LDAPR and PAC branches stop as undefined |
| ELF loader, process layout, first SVCs | done | `load_elf` (static ET_EXEC); fixed `hle::layout`; SVCs 0x01 0x07 0x0b 0x1e 0x26 0x27 |
| Embedded suite with independent expected output | done | 6 programs embedded, all pass; `scripts/gen-expected.py --check` re-derives every expected file in Python and runs in CI |
| Self-test command | done | `kestrel selftest [-v]`, exit code 2 on failure |
| CI on all platforms | done | `ci.yml` native jobs on `ubuntu-24.04`, `ubuntu-24.04-arm`, `windows-2022`, `windows-11-arm`, `macos-14`, `macos-15-intel`, plus the qemu fallback; release tags and manual dispatch only; all green on releases `v0.1.6` to `v0.1.8` |
| Release packaging | done | every package built and self-tested on its own OS and CPU; the macOS universal binary is also self-tested on `macos-15-intel`; published for `v0.1.6` to `v0.1.8` |

### Phase 1: Complete CPU: **done**

| Task | Status | Evidence |
|------|--------|----------|
| 1A Scalar floating point | done, with the gaps below | `float.rs` (Arm pseudocode) + `interp/fp.rs`; 13 test groups in `interp/fp_tests.rs`; a compiler-generated FP smoke program matches x86 host results bit for bit |
| 1B NEON | done, with the gaps below | `interp/neon/` (integer, floating point, permutes, structure loads and stores); 430 generated cases in `neon_vectors.rs`; every C test program built with NEON at `-O2` and `-O3` matches the expected output |
| 1C Phase 1 tests | done | NEON unit vectors (430 cases); every integer C program also built with NEON (`name-neon.elf`, same expected output); `fp.c` (127 scalar FP results as bit patterns, modelled by `scripts/armfp.py`); `intrinsics.c` (168 NEON intrinsics, modelled by the `gen-neon-vectors.py` lane models); 30 NaN, subnormal, flush-to-zero and saturation checks in `asm_checks.S`; `kestrel selftest` runs 13 programs |
| 1D Real-hardware oracle | done | `rt.h` host mode (`KESTREL_HOST`) sends the three console calls to the C library; `scripts/check-host.sh` builds every C program natively (integer-only and NEON, 12 builds) and compares its output with `guest/expected`; CI job `hardware-oracle` runs it on `ubuntu-24.04-arm` and `macos-14`, both passing on releases `v0.1.8` and `v0.1.9` (12/12 on each; Apple clang 15 and clang 18). Not covered: `asm_checks.S`, which is console-only assembly |
| Phase 1 acceptance | done | every C program with and without NEON passes the self-test on all five targets (CI, `v0.1.8`); the hardware oracle passes on both ARM runners; `scripts/check-optimised.sh` rebuilds every program at `-O3 -march=armv8.2-a` and runs it in kestrel: 13/13 reach the expected output with no unsupported instruction (CI, clang 18, `v0.1.9`) |

### Phase 2: Homebrew: **in progress**

| Task | Status | Evidence |
|------|--------|----------|
| Homebrew executable loader | done, with the gaps below | `kestrel-loader/src/nro.rs` (segments, zeroed BSS, ASET icon/NACP/RomFS ranges) and `kestrel-hle/src/homebrew.rs` (loader config entries, argument string, X0/X1); `kestrel run x.nro -- args`; self-test program `hbtest` (own startup code, self-relocation) |
| Process layout reporting | done, with the gaps below | `hle::layout` is one documented 39-bit layout; svcGetInfo (`svc.rs`) answers types 2-8, 11-18, 21-23; self-test program `hblayout` checks the layout's documented properties |
| Memory SVCs | not started | only SetHeapSize |
| Handle table and kernel objects | not started | none |
| Threads | not started | one thread; SleepThread returns at once |
| Synchronisation | not started | none |
| IPC | not started | none |
| First services | not started | none |
| SD folder option, unimplemented-call log | not started | unknown SVCs stop with their number; no service layer yet |
| `docs/SWITCH2_NOTES.md` | not started | file missing |

### Phase 3: Native execution on ARM hosts: **not started**

| Task | Status | Evidence |
|------|--------|----------|
| Memory arena refactor | not started | `Memory` is a `BTreeMap` of `Vec<u8>` regions |
| Native backend | not started | `create_backend(Native)` returns "not implemented yet" |
| Differential tester | not started | none |
| Self-modifying code, cache maintenance | not started | `IC IVAU` is a no-op; `CpuBackend::invalidate` is never called |
| Per-backend speed in self-test | partial | self-test prints MIPS for the one backend it runs |

### Phases 4 to 7: **not started**

- Phase 4 x86_64 recompiler: `create_backend(Jit)` returns "not implemented yet".
- Phase 5 graphics, phase 6 commercial software, phase 7 polish: no code.

### Deviations from the hard rules found in the audit

- "Every instruction has a unit test": `interp/tests.rs` plus the 91 checks in
  `asm_checks.S` cover most implemented instructions, but not all (for
  example BIT/BIF, REV32, CCMN, SMADDL/UMADDL, LDSMIN/LDUMAX).
- Fixed: `system.rs` treated every MSR (immediate) and every SYS/SYSL other
  than `DC ZVA` as a no-op, and `svcOutputDebugString` truncated at 1 MiB.
  Now hints and barriers run as NOP (as the architecture defines), the EL0
  cache maintenance operations are listed explicitly, everything else stops
  as undefined, and debug strings are read in full.

### Owner decisions from the audit

- CI stays release-only (tags and manual runs); `PLAN.md` section 3 updated.
- Fix the platform coverage gap, the missing expected-output generator and
  the silently ignored system instructions, then start phase 1A.

### Phase 1A notes

Implemented for single and double precision: FADD, FSUB, FMUL, FDIV,
FNMUL, FMAX, FMIN, FMAXNM, FMINNM, FMADD, FMSUB, FNMADD, FNMSUB, FMOV
(register, immediate, general), FABS, FNEG, FSQRT, FCVT between single and
double, FRINTN/P/M/Z/A/X/I, FCMP, FCMPE, FCCMP, FCCMPE, FCSEL, FCVT{N,P,M,Z,A}{S,U}
(integer and fixed point), SCVTF, UCVTF (integer and fixed point), FRECPE,
FRSQRTE, FRECPX, FRECPS, FRSQRTS. NaN propagation, default NaN (FPCR.DN) and
flush-to-zero (FPCR.FZ, judged on the exact value as Arm does) follow the
pseudocode; conversions and FRINT honour every rounding mode.

Remaining gaps, each reported as `Exit::Unsupported` rather than guessed:

- Half precision (per the plan's provision).
- FPCR.RMode other than round-to-nearest for host-computed arithmetic
  (add, sub, mul, div, sqrt, FMA, FRECPS, FRSQRTS). Conversions and FRINT
  already honour it.
- Under FZ, a double-precision FMA whose result rounds to exactly the
  smallest normal (the exact value is not recoverable on the host).

Not modelled: the FPSR cumulative exception flags (IOC, DZC, OFC, UFC, IXC,
IDC) are never set. FRINT32*/FRINT64* (Armv8.5) decode as undefined, as on
the Armv8.2 target CPU.

Expected values: hand-worked from the Arm pseudocode (NaN rules, FZ, the
estimates) or IEEE results from Python. `fp.c` is also run natively on real
ARM CPUs by `scripts/check-host.sh` (task 1D).

### Phase 1B notes

Implemented, vector and scalar forms: three same (integer, logic, floating
point, pairwise), three same extra (SQRDMLAH, SQRDMLSH), two-register misc
(integer and floating point, including FCVTN/FCVTL/FCVTXN, URECPE,
URSQRTE), three different (long, wide, narrowing high half, doubling,
PMULL 8-bit), across lanes, scalar pairwise, shift by immediate (including
accumulate, rounding, insert, narrowing, widening and fixed-point
conversion), by element (integer and floating point), copies (DUP, INS,
UMOV, SMOV), permutes (UZP, TRN, ZIP), EXT, TBL/TBX, the whole
modified-immediate group (MOVI, MVNI, ORR, BIC, FMOV), and LD1-LD4 /
ST1-ST4 (multiple structures, single lane, replicate, both post-index
forms). Saturation sets FPSR.QC. Floating-point lanes go through the same
`float.rs` functions as the scalar instructions.

Tests: `scripts/gen-neon-vectors.py` assembles each case with clang and
computes the expected registers, QC and memory with its own Python models of
the Arm ARM descriptions (exact fractions for floating point); CI checks the
generated file is current. Independently of those models, the five C test
programs built with NEON allowed (`-O2` and `-O3`, clang 21) give the
expected output, using among others `cmgt`, `ld4`, `ushll`, `shll`, `xtn`,
`uzp1`, `umaxv`, `dup` and `movi`. `intrinsics.c` and the NEON builds are
also run natively on real ARM CPUs by `scripts/check-host.sh` (task 1D).

Remaining gaps:

- Half precision stops as `Exit::Unsupported` (per the plan's provision).
- Not implemented, so they stop as undefined: the crypto extension (AES,
  SHA, 64-bit PMULL; the plan adds them only when a program needs them),
  DotProd (SDOT, UDOT; optional in Armv8.2), and the Armv8.3+ complex,
  BFloat16 and Int8 matrix forms.
- As for scalar FP: directed FPCR rounding for host-computed lanes is
  reported as unsupported, and FPSR exception flags other than QC are not
  set.

### Phase 2 notes

Homebrew executable loader, from Switchbrew's public "NRO" and "Homebrew
ABI" pages. An NRO is mapped at the fixed address `layout::NRO_BASE`
(0x800_0000): `.text` read-execute, `.rodata` read-only, `.data` and `.bss`
read-write, `.bss` zeroed. The asset section is parsed into file ranges; the
filesystem service will serve the RomFS later. The entry point gets X0 = the
loader config list and X1 = all ones. The list holds:

| Key | Entry | Value |
|-----|-------|-------|
| 1 | MainThreadHandle | the current-thread pseudo-handle 0xffff8000, until a handle table exists |
| 7 | AppletType | Application (0), no flags |
| 5 | Argv | argv[0] is the file name, then the arguments; empty arguments and arguments with spaces are double-quoted |
| 14 | RandomSeed | a fixed value, so runs are reproducible |
| 0 | EndOfList | the loader string `kestrel <version>` |

Gaps:

- Not provided yet: HosVersion (homebrew falls back to asking the settings
  service), SyscallAvailableHint, OverrideHeap and the other optional keys.
- argv[0] becomes an `sdmc:/` path once the SD folder option exists.
- An argument containing a double quote is refused, because the argument
  string cannot carry it.
- Compressed NROs (a newer system feature) stop as unsupported.
- `hbtest` is kestrel's own program built by `scripts/make-nro.py`; real
  homebrew built with the public toolchain also needs the process layout,
  memory, thread and service work that follows.

Process layout, from Switchbrew's "SVC" (svcGetInfo, AddressSpace64Bit)
and "16.0.0" (39-bit region sizes) pages. Placement is kestrel's own and
fixed; the kernel randomises it.

| Region | Base | Size |
|--------|------|------|
| ASLR (the whole 39-bit space; code goes here) | 0x800_0000 | up to 2^39 |
| Alias | 0x10_0000_0000 | 64 GiB |
| Heap (svcSetHeapSize maps here) | 0x60_0000_0000 | 8 GiB |
| Stack (holds the 1 MiB main stack) | 0x7e_8000_0000 | 2 GiB |

svcGetInfo also reports TotalMemorySize 1 GiB (kept small while guest
memory is ordinary host allocations), UsedMemorySize as image plus main
stack plus heap, the same for the non-system variants, system resource
sizes 0, no debugger, IsApplication 1, a fixed RandomEntropy, and the
program ID "kestrel" in ASCII so it cannot pass for a real title. A wrong
handle gives 0xe401 and a wrong subtype 0xf601.

Gaps:

- Types 0, 1, 9, 10, 19, 20, 24-26 and unknown types stop with an explicit
  "unimplemented svcGetInfo type" message.
- svcSetHeapSize does not yet check the heap region or total memory.
- The ELF test programs stay at 0x200000, below the address space.

## Next task

Phase 2, memory system calls: svcQueryMemory with memory types and
attributes for every mapping, svcSetMemoryPermission, svcSetMemoryAttribute,
svcMapMemory/svcUnmapMemory into the alias region, and svcMapPhysicalMemory,
with the heap checked against its region and the total memory size.
