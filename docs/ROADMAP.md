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

### Phase 0: Foundation: **partial** (code done; platform coverage short)

| Task | Status | Evidence |
|------|--------|----------|
| Workspace split | done | crates `kestrel-core`, `-cpu`, `-loader`, `-hle`, `-testsuite`, `-cli` |
| Portable interpreter (integer, branches, all load/store forms, exclusives, atomics, user sysregs, SIMD moves) | done | `interp/{dp_imm,dp_reg,branch,ldst,system,simd}.rs`; CASP, LDAPR and PAC branches stop as undefined |
| ELF loader, process layout, first SVCs | done | `load_elf` (static ET_EXEC); fixed `hle::layout`; SVCs 0x01 0x07 0x0b 0x1e 0x26 0x27 |
| Embedded suite with independent expected output | done | 6 programs embedded, all pass; `scripts/gen-expected.py --check` re-derives every expected file in Python and runs in CI |
| Self-test command | done | `kestrel selftest [-v]`, exit code 2 on failure |
| CI on all platforms | done, awaiting first run | `ci.yml` native jobs on `ubuntu-24.04`, `ubuntu-24.04-arm`, `windows-2022`, `windows-11-arm`, `macos-14`, `macos-15-intel`, plus the qemu fallback; release tags and manual dispatch only |
| Release packaging | done, awaiting first run | every package built and self-tested on its own OS and CPU; the macOS universal binary is also self-tested on `macos-15-intel` |

### Phase 1: Complete CPU: **in progress**

| Task | Status | Evidence |
|------|--------|----------|
| 1A Scalar floating point | done, with the gaps below | `float.rs` (Arm pseudocode) + `interp/fp.rs`; 13 test groups in `interp/fp_tests.rs`; a compiler-generated FP smoke program matches x86 host results bit for bit |
| 1B NEON | partial | only FMOV general, DUP/INS/UMOV, vector logic, MOVI/MVNI (`simd.rs`); first gaps `cmgt`, `ld4` (see probe above) |
| 1C Phase 1 tests | not started | guests built with `-mgeneral-regs-only`; no FP or NEON test program |
| 1D Real-hardware oracle | not started | no host build mode for `guest/src` |

### Phase 2: Homebrew: **not started**

| Task | Status | Evidence |
|------|--------|----------|
| Homebrew executable loader | not started | only ELF |
| Process layout reporting | not started | fixed constants in `hle::layout`; no svcGetInfo |
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
estimates) or IEEE results from Python; a check against real ARM hardware
is task 1D.

## Next task

Phase 1B, NEON. First gaps hit by compiled code: `cmgt v3.2d` (`4ee63463`),
`ld4 {v0.16b-v3.16b}` (`4c400100`), vector `fmov v1.2d, #imm` (`6f03f601`).
