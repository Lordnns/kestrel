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
| CI on all platforms | done | `ci.yml` native jobs on `ubuntu-24.04`, `ubuntu-24.04-arm`, `windows-2022`, `windows-11-arm`, `macos-15` (`macos-14` until v0.1.14, when GitHub retired it), `macos-15-intel`, plus the qemu fallback; release tags and manual dispatch only; all green on releases `v0.1.6` to `v0.1.8` |
| Release packaging | done | every package built and self-tested on its own OS and CPU; the macOS universal binary is also self-tested on `macos-15-intel`; published for `v0.1.6` to `v0.1.8` |

### Phase 1: Complete CPU: **done**

| Task | Status | Evidence |
|------|--------|----------|
| 1A Scalar floating point | done, with the gaps below | `float.rs` (Arm pseudocode) + `interp/fp.rs`; 13 test groups in `interp/fp_tests.rs`; a compiler-generated FP smoke program matches x86 host results bit for bit |
| 1B NEON | done, with the gaps below | `interp/neon/` (integer, floating point, permutes, structure loads and stores); 430 generated cases in `neon_vectors.rs`; every C test program built with NEON at `-O2` and `-O3` matches the expected output |
| 1C Phase 1 tests | done | NEON unit vectors (430 cases); every integer C program also built with NEON (`name-neon.elf`, same expected output); `fp.c` (127 scalar FP results as bit patterns, modelled by `scripts/armfp.py`); `intrinsics.c` (168 NEON intrinsics, modelled by the `gen-neon-vectors.py` lane models); 30 NaN, subnormal, flush-to-zero and saturation checks in `asm_checks.S`; `kestrel selftest` runs 13 programs |
| 1D Real-hardware oracle | done | `rt.h` host mode (`KESTREL_HOST`) sends the three console calls to the C library; `scripts/check-host.sh` builds every C program natively (integer-only and NEON, 12 builds) and compares its output with `guest/expected`; CI job `hardware-oracle` runs it on `ubuntu-24.04-arm` and `macos-14`, both passing on releases `v0.1.8` and `v0.1.9` (12/12 on each; Apple clang 15 and clang 18). Not covered: `asm_checks.S`, which is console-only assembly |
| Phase 1 acceptance | done | every C program with and without NEON passes the self-test on all five targets (CI, `v0.1.8`); the hardware oracle passes on both ARM runners; `scripts/check-optimised.sh` rebuilds every program at `-O3 -march=armv8.2-a` and runs it in kestrel: 13/13 reach the expected output with no unsupported instruction (CI, clang 18, `v0.1.9`) |

### Phase 2: Homebrew: **done** (accepted on `v0.1.17`)

| Task | Status | Evidence |
|------|--------|----------|
| Homebrew executable loader | done, with the gaps below | `kestrel-loader/src/nro.rs` (segments, zeroed BSS, ASET icon/NACP/RomFS ranges) and `kestrel-hle/src/homebrew.rs` (loader config entries, argument string, X0/X1); `kestrel run x.nro -- args`; self-test program `hbtest` (own startup code, self-relocation) |
| Process layout reporting | done, with the gaps below | `hle::layout` is one documented 39-bit layout; svcGetInfo (`svc.rs`) answers types 2-8, 11-18, 21-23; self-test program `hblayout` checks the layout's documented properties |
| Memory SVCs | done, with the gaps below | `hle/memory.rs`: memory types per mapping, svcQueryMemory, svcSetHeapSize in place with its checks, svcSetMemoryPermission, svcSetMemoryAttribute, svcMapMemory/svcUnmapMemory, svcMapPhysicalMemory/svcUnmapPhysicalMemory; `Memory` range operations in `kestrel-core`; self-test program `hbmemory` |
| Handle table and kernel objects | done, with the gaps below | `hle/kernel.rs`: per-process handle table, objects alive while a handle names them; svcCloseHandle, svcGetThreadId, svcGetProcessId, svcCreateEvent, svcSignalEvent, svcClearEvent, svcResetSignal; svcCreateTransferMemory in `hle/memory.rs`; the main thread has a real handle; self-test program `hbhandles` |
| Threads | done, with the gaps below | `hle/thread.rs`: thread records, a deterministic scheduler over four virtual cores with slices of 19,200 instructions, a process-wide 19.2 MHz clock, thread local regions; svcCreateThread, svcStartThread, svcExitThread, svcSleepThread (sleep and yield), svcGet/SetThreadPriority, svcGet/SetThreadCoreMask, svcGetCurrentProcessorNumber, svcGetInfo CoreMask and PriorityMask; self-test program `hbthreads` |
| Synchronisation | done, with the gaps below | `hle/sync.rs`: a waiting state with deadlines on the process clock; svcWaitSynchronization, svcCancelSynchronization, svcArbitrateLock/Unlock, svcWaitProcessWideKeyAtomic, svcSignalProcessWideKey, svcWaitForAddress, svcSignalToAddress; deadlock report; self-test program `hbsync` |
| IPC | done, with the gaps below | `hle/ipc.rs`: the message format (header, special header, handles, X/A/B/W descriptors, raw data, receive list) and CMIF and TIPC replies; `hle/service.rs`: client sessions as kernel objects, svcConnectToNamedPort, svcSendSyncRequest, the service manager sm: (RegisterClient, GetServiceHandle over CMIF and TIPC); unknown ports, services and commands stop by name and number; self-test program `hbipc` |
| First services | done, with the gaps below | system version 20.0.0 (`hle::firmware`) in the HosVersion loader entry; `hle/service/`: CMIF domains, copied handles, receive (C) buffers; set:sys GetFirmwareVersion2; apm; appletOE with the commands libnx's applet startup sends; shared memory objects with svcMapSharedMemory/svcUnmapSharedMemory; hid with its shared memory and npad setup; time:u with its clocks, shared memory and UTC calendar times, and CNTPCT_EL0; CloneCurrentObject with shared domains, send (A) and receive (B) buffers; fsp-srv with the SD card's files and directories; psm; fatal:u, svcBreak and result names; controller input in hid's shared memory, from a window (winit, softbuffer, gilrs) mapping keyboard and gamepads; self-test programs `hbapplet`, `hbhid`, `hbtime`, `hbfs`, `hbpower`, `hbinput` |
| SD folder option, unimplemented-call log | done | `kestrel run --sd <folder>`, else the portable `user/sdmc` next to the executable; unknown SVCs, svcGetInfo types, named ports, services, commands and CMIF control commands stop as `Stop::Unimplemented` with their identifiers, and `kestrel run` appends each to `user/logs/unimplemented.log` |
| `docs/SWITCH2_NOTES.md` | done | purpose, entry format, the first public-source entries (Switchbrew [S2] marks); no observations yet |
| Phase 2 acceptance | done | the four standard examples (hello-world, read-controls, sdmc, barrier), built from source in the pinned devkitPro container (libnx 4.12.0), run in kestrel with scripted presses, and their last frames read back with libnx's font and checked against their sources (`scripts/build-examples.sh`, `run-examples.sh`, `check-examples.py`): 4/4 on every native CI job (Linux, Windows and macOS, x86_64 and aarch64) on `v0.1.17`, which was all green; every unimplemented call stops with its name and number and is logged |

### Phase 3: Native execution on ARM hosts: **in progress** (memory arena done; the rest waits for the Windows milestone)

The owner had the memory arena refactor done first, on Linux (2026-10-30;
"Phase 3 notes"). Since 2026-10-31 the rest waits for the Windows x86_64
milestone, and ARM hosts start with the in-process fallback ("Plan
changes (2026-10-31, the Windows milestone)"). The differential tester
moved to the CPU track, for the recompiler first.

| Task | Status | Evidence |
|------|--------|----------|
| Memory arena refactor | done (2026-10-30; CI green on every platform, manual run 5 on the branch) | `kestrel_core::arena` (one host range, 2 MiB aligned, committed as used), `Memory` as a lock-free two-level page table over it, `Send + Sync`; compare-and-swap, `translate` and `epoch` for the fast backends; the self-test's output unchanged; the hypervisor and recompiler constraints in `docs/ARCHITECTURE.md`, "Memory" |
| Native backend | not started | `create_backend(Native)` returns "not implemented yet" |
| Differential tester | done (2026-10-31, CPU track) | `kestrel_testsuite::diff`, `kestrel diff --backend <b>`; "Phase 4 notes" |
| Self-modifying code, cache maintenance | not started | `IC IVAU` is a no-op; `CpuBackend::invalidate` is never called |
| Per-backend speed in self-test | done (2026-10-31) | `kestrel selftest` runs every backend the build has and prints each one's MIPS; `--backend` picks one |
| Guest counter reads from kestrel's clock | not started | `PLAN.md` section 3, "Time and cores": trap EL0 counter reads (CNTKCTL_EL1), measure the cost |
| One host thread per guest core (six) | not started | `PLAN.md` section 3, "Time and cores"; the interpreter's scheduler runs every core on one host thread |

### Phase 4: x86_64 recompiler: **in progress** (Windows and Linux x86_64)

| Task | Status | Evidence |
|------|--------|----------|
| Differential tester | done (2026-10-31) | `kestrel_testsuite::diff`, `kestrel diff`; "Phase 4 notes" |
| IR between the ARM64 decoder and x86_64 code generation | done | `jit/ir.rs`, `jit/frontend.rs` |
| kestrel's own x86_64 encoder, checked against llvm-mc | done | `jit/x64/asm.rs`; 5,359 cases from `scripts/gen-x64-vectors.py` |
| Executable memory per OS, never writable and executable at once | done (Linux run; Windows built and run under Wine, not yet on Windows itself) | `jit/code.rs` |
| Windows x64 and System V calling conventions | done | both run in the Linux tests |
| Blocks cached by guest address, checked through the memory's change count | done | `Jit::block`, `Jit::valid` |
| Interpreter fallback per instruction | done | `Op::Interp` |
| Integer, branches, loads and stores translated | done | 99.3% of the self-test's instructions run translated |
| Floating point and NEON translated | not started | they run through `Op::Interp` |
| Speed: block chaining, inline memory accesses | done | "Phase 4 notes" |
| Speed: computed-jump lookup in generated code, guest registers in host registers, lazy flags | not started | "Next task" |
| One host thread per guest core | not started | `PLAN.md` section 3, "Time and cores" |
| Performance measurement for runs | done (2026-10-31) | `kestrel run --stats`: instructions per second, and frame times (average, 1% low, worst) |
| Phase 4 acceptance | self-test 27/27 and zero divergences on Linux x86_64 in both conventions; CI on Windows x86_64 pending | "Phase 4 notes" |

### Phases 5 to 7: **not started**

- Phase 5 graphics (5A display, 5B Maxwell GPU level, 5C Ampere GPU level,
  5D NVN API level), phase 6 commercial software, phase 7 polish: no code.

### Deviations from the hard rules found in the audit

- Fixed (2026-10-31): "Every instruction has a unit test": `interp/tests.rs`
  plus the 91 checks in `asm_checks.S` covered most implemented
  instructions, but not all (for example BIT/BIF, REV32, CCMN,
  SMADDL/UMADDL, LDSMIN/LDUMAX). `scripts/gen-int-vectors.py` now tests
  every general-purpose instruction one by one (see "CPU gaps before
  phase 3").
- Fixed: `system.rs` treated every MSR (immediate) and every SYS/SYSL other
  than `DC ZVA` as a no-op, and `svcOutputDebugString` truncated at 1 MiB.
  Now hints and barriers run as NOP (as the architecture defines), the EL0
  cache maintenance operations are listed explicitly, everything else stops
  as undefined, and debug strings are read in full.

### Owner decisions from the audit

- CI stays release-only (tags and manual runs); `PLAN.md` section 3 updated.
- Fix the platform coverage gap, the missing expected-output generator and
  the silently ignored system instructions, then start phase 1A. (Done:
  phase 1 is complete.)

### Plan changes (2026-10-29, evening)

Prompted by the first public Switch 2 demos on Apple hardware
(`PLAN.md` section 6). What changed and what it affects:

| Change in `PLAN.md` | Affects |
|---------------------|---------|
| Section 0 replaced: the audit is done; sessions start from this file's status and "Next task" | session start-up |
| Section 1 adds the goal of emulating graphics at the highest level that works | phase 5 |
| Section 2 adds: no ROM or key sites even for testing; no leaked SDK or NVN material and no private forks; rules for reusing other emulators' code; observed behaviour is written to `docs/SWITCH2_NOTES.md` before it is implemented | all phases |
| Phase 3 starts on macOS once phase 2's threads and synchronisation are done, in parallel with IPC and services | order of work after the handle table |
| Phase 5 split into 5A display, 5B Maxwell GPU level (homebrew, deko3d), 5C Ampere GPU level, 5D NVN API level for commercial games | graphics; resolves the old Maxwell question: both are needed |
| Phase 6 uses 5C or 5D, whichever gets a title further first | commercial software |
| New sections 6 (external landscape) and 7 (open decisions for the owner) | owner decisions: strategy, 5D approval, code reuse |

### Plan changes (2026-10-30)

The owner's answer to the clock-rate question (phase 2's last open
point). What changed in `PLAN.md` and what it affects:

| Change in `PLAN.md` | Affects |
|---------------------|---------|
| Section 1 adds the product goal: behave like a real Switch 2 in real time, 1:1 with wall-clock time, up to 120 fps, on the native and recompiler backends; every design choice is weighed against it | all phases, above all 3, 4 and 5 |
| Section 3 "Time and cores", the clock: interactive `kestrel run` reads the host's clock for the 19.2 MHz counter (guest time is real time); the self-test, unattended runs and the differential tester count one tick every 52 instructions (998.4 MHz, the Switch 2's rate of about 1 GHz), so their results repeat | the clock (implemented in the next commit), hid and vsync timing, scripted presses; phase 3 must answer guest counter reads from kestrel's clock (EL0 counter traps through CNTKCTL_EL1) |
| Section 3 "Time and cores", the cores: plan for six guest cores, each on its own host thread on the native and recompiler backends; the interpreter keeps its deterministic scheduler for tests | phase 3 and 4 design (shared arena and exclusive monitor, host atomics, a thread-safe OS layer); the guest-visible core count moves from four to six once the Switch 2's numbering is documented or observed |
| Phase 3 and 4 provisions point at those decisions | the native backend and the recompiler |

Why: tests need time that repeats and players need real time, which
one clock cannot give. The phase 2 clock (one tick per instruction) made
programs see a CPU about 50 times slower than a Switch 2 in guest time,
so their frame timing and timeouts were far from a console's. Six cores
on host threads is what lets the fast backends keep up with games that
spread their work over the console's cores; a single host thread taking
turns would cap them at one core's speed.

### Plan changes (2026-10-31)

The owner's decision of 2026-10-30 to close the CPU gaps before phase 3
(below). What changed in `PLAN.md` and what it affects:

| Change in `PLAN.md` | Affects |
|---------------------|---------|
| Phase 1A: half precision no longer stops as unsupported "for now"; it is added before phase 3 | `float.rs`, the scalar and NEON floating-point decoders |
| Phase 1B: the crypto extension, with the dot product, is added before phase 3 rather than when a program needs it | the NEON decoders |

### Plan changes (2026-10-31, the Windows milestone)

The owner's decisions of 2026-10-31. What changed in `PLAN.md` and what
it affects:

| Change in `PLAN.md` | Affects |
|---------------------|---------|
| Section 1: the product target is Windows x86_64 only until the Windows milestone (a game the owner owns playable from start to gameplay at full speed on Windows x86_64); the goal is full emulation at up to 120 fps, as soon as possible | the order of all work |
| Section 3, "Platform priority": the other platforms are frozen, not dropped: existing features keep working and passing CI on all six native targets; new features are built for Windows x86_64, and Linux x86_64 runs them where testing needs it (cloud sessions, lavapipe); on macOS and ARM a new feature compiles and stops with "not yet supported on this platform", never a build error | every new feature from now on; `create_backend(Jit)` off x86_64 |
| Section 3: two main tracks in parallel, the CPU track (differential tester, then the x86_64 recompiler) and the graphics track (5A then 5B, Vulkan, lavapipe on Linux, real GPUs on Windows); phases 4 and 5 no longer wait for phase 3 | phases 3, 4 and 5; the "Next task" below |
| Section 3: after the milestone, platforms in this order: macOS ARM, Linux x86_64, Windows ARM, macOS x86_64, Linux ARM; ARM hosts start with phase 3's in-process fallback (guest code run directly, rewriting svc, system and thread registers, x18 and counter reads), because GitHub's macOS ARM runners return HV_UNSUPPORTED from hv_vm_create and its Linux ARM runners have no /dev/kvm; hypervisor backends wait for Mac hardware | phase 3 (its order and first backend) |
| Section 3's machine table corrected: the owner has no ARM machine; their machines are x86_64 (a Windows PC, a Windows bare-metal server, Debian bare metal, Proxmox) | gameplay testing; ARM hosts tested only on CI |
| Phase 4 provisions: kestrel's own x86_64 encoder (no new dependency), every encoding checked against llvm-mc; the Windows x64 calling convention first, System V for Linux testing; executable memory never writable and executable at once; an interpreter fallback for anything not translated; integer, branches, loads and stores first, then floating point and NEON through the interpreter's routines where simpler; zero divergences at every step. Acceptance is now Windows x86_64 (and Linux x86_64 for testing) | the recompiler |
| Phase 5: 5A then 5B in parallel with phase 4 | the graphics track |

### CPU gaps before phase 3 (owner, 2026-10-30)

The owner's decision: close the CPU gaps on Linux before phase 3, keeping
the changes inside `kestrel-cpu` and its tests and leaving how
instructions reach memory alone (a separate branch is turning guest memory
into the phase 3 arena). Switch 2 games are built for the Cortex-A78C,
which has half precision, the dot product and the crypto extension.

| Step | Status | Evidence |
|------|--------|----------|
| 1. A unit test for every implemented instruction | done | `scripts/gen-int-vectors.py` writes `interp/int_vectors.rs`: 478 one-instruction cases (data processing immediate and register, branches, system registers and hints, single, SIMD-register, pair and literal loads and stores, exclusives, acquire/release, CAS, the LSE atomics, FMOV to and from general registers), run by `interp/int_tests.rs`; with the NEON vectors, every interpreter line except undefined-encoding paths runs under `cargo test -p kestrel-cpu` (measured with `-C instrument-coverage`) |
| 2. Half precision (FP16) | done | `Fmt::H` in `float.rs`, computed exactly in integers (every rounding mode, FPCR.FZ16); the scalar floating-point group with `ftype` 11 (arithmetic, FMA, compares, selects, FMOV, round-to-integral, conversions to and from single, double and integers), the FP16 three-same and two-register-misc groups (vector and scalar), across lanes, scalar pairwise, by element, fixed-point conversion, FMOV (vector), FCVTN/FCVTL to and from half; 240 cases in `neon_vectors.rs` (groups `fp16_same`, `fp16_misc`, `fp16_other`, `fp16_scalar`) |
| 3. DotProd (SDOT, UDOT) | done | vector (three same extra, opcode 0010) and by element (opcode 1110, index H:L), 2S and 4S, in `neon/int.rs`; 11 cases in `neon_vectors.rs` (group `dot`); flipping the signedness fails all 11 |
| 4. Crypto (AES, SHA1, SHA2, 64-bit PMULL) | done | `neon/crypto.rs`: AESE, AESD, AESMC, AESIMC (the S-box computed from FIPS 197's definition), SHA1C/P/M, SHA1H, SHA1SU0/SU1, SHA256H/H2, SHA256SU0/SU1; PMULL/PMULL2 1Q in `neon/int.rs`; 26 cases in `neon_vectors.rs` (group `crypto`); breaking SHA256SU1's upper words or the inverse MixColumns matrix fails 1 and 2 cases |
| 5. CRC32 and CASP (owner, 2026-10-31) | done | CRC32B/H/W/X and CRC32CB/H/W/X (FEAT_CRC32) in `dp_reg.rs`; CASP, CASPA, CASPL, CASPAL (FEAT_LSE) in `ldst.rs`, through the same load and store helpers as CAS; 16 cases in `int_vectors.rs`, with X26..X29 holding pairs that match memory; swapping the CRC-32C polynomial for CRC-32's, or giving Rs+1 the wrong old value, fails 5 and 6 cases |

How the integer vectors work: each case is one instruction at
`0x10080`, assembled by clang and read back with llvm-objdump (the script
refuses any encoding left to a relocation). Every case starts from the
same 31 registers (edge values, pointers into a 768-byte memory window,
a value for a CAS that matches), SP, vector registers, FPCR, FPSR and
TPIDR_EL0, with its own NZCV and exclusive-monitor address, runs one step,
and compares all of them plus the next PC and the memory window. The
models read the assembly text, not the encoding, and follow the Arm ARM
pseudocode (AddWithCarry, ConditionHolds, ShiftReg, ExtendReg, the
bitfield aliases, the exclusive monitor: a store-exclusive leaves the
local monitor open whether or not it succeeded). CTR_EL0, DCZID_EL0 and
CNTFRQ_EL0 are kestrel's documented values (64-byte lines, 64-byte DC ZVA
blocks, 19.2 MHz). Deliberately breaking CCMN's carry and LDSMIN's signed
compare fails 2 and 4 cases.

Half precision: the models are `scripts/armfp.py` with a 16-bit format
(FZ16 for half precision, FZ for the others; conversions clear FZ16 as
FPUnpackCV and FPRoundCV do), and the rounding mode now passed to the
arithmetic models (an exact zero sum is -0 only when rounding to minus
infinity). The square root of a half is computed exactly
(`armfp.exact_sqrt`), and the reciprocal estimate's overflow follows the
rounding mode. The NEON vectors gained NZCV, an input and an output per
case, for FCMP, FCCMP and FCSEL, and four half-precision registers
(V26..V29: subnormals, signed zeros, infinities, a quiet and a signalling
NaN, the largest normal). Checked beyond the committed cases: the Python
model matches numpy's float16 on 200,000 random add, subtract, multiply,
divide and square-root operands, and kestrel matched the model on 3,600
further random cases across rounding modes, FZ16 and DN. Writing the
cases found a bug: SCVTF and UCVTF read 16-bit lanes as unsigned
(`from_fixed` sign-extended only 32 and 64 bits). Deliberately using FZ
for half precision, or +0 for exact zero sums under round-to-minus,
fails 4 and 1 cases. Kestrel's choice: the alternative half-precision
format (FPCR.AHP) is not modelled; conversions to or from half
precision under it stop as `Unmodelled::AltHalfPrecision` (half-precision
arithmetic ignores AHP, as the pseudocode's FPUnpack does).

Crypto: the Python models are checked every time the vectors are
generated (`crypto_selfcheck` in `gen-neon-vectors.py`): AES-128 through
AESE/AESMC and back through AESD/AESIMC must give FIPS 197's Appendix C.1
vector, and SHA-1 and SHA-256 of "abc" through the hash and schedule
instructions must match Python's `hashlib`; the SHA-256 constants are
computed from their definition (FIPS 180-4), not copied. Perturbing any
schedule model or the majority function fails the check. SHA512, SHA3,
SM3 and SM4 (optional, and not on the Cortex-A78C) stay undefined.

The C test programs rebuilt with `GUEST_OPT="-O3
-march=armv8.2-a+fp16+dotprod+crypto"` (`scripts/check-optimised.sh`)
still give the expected output, 13/13, but clang 18 emits none of the new
instructions for them, so the unit vectors are what covers them.

CRC32 and CASP: both are mandatory from Armv8.1, so the Cortex-A78C has
them; the owner asked for them after the first four steps (2026-10-31).
The CRC model follows the pseudocode literally (BitReverse, Poly32Mod2,
BitReverse), while kestrel shifts the reflected polynomial, and the model
must give the standard check values (CRC-32 of "123456789" as zlib
computes it, CRC-32C 0xE3069283) every time the vectors are generated.
CASP with an odd Rs or Rt, which the Arm ARM leaves CONSTRAINED
UNPREDICTABLE, stops as undefined (kestrel's choice).

Not reached by any CPU unit test, though the instructions are tested:
some value paths of `float.rs` (FRECPE and FRSQRTE of subnormals and of
values whose reciprocal overflows under each rounding mode, FRECPS and
FRSQRTS of infinity times zero, single-precision FMA under FZ) in single
and double precision.

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

- Half precision: done since (2026-10-31, "CPU gaps before phase 3"),
  except conversions under FPCR.AHP.
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

- Half precision: done (2026-10-31, "CPU gaps before phase 3").
- DotProd (SDOT, UDOT) and the crypto extension (AES, SHA1, SHA256,
  64-bit PMULL): done (2026-10-31, "CPU gaps before phase 3").
- Not implemented, so they stop as undefined: SHA512, SHA3, SM3, SM4,
  and the Armv8.3+ complex, BFloat16 and Int8 matrix forms.
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
| 1 | MainThreadHandle | a real handle to the main thread |
| 7 | AppletType | Application (0), no flags |
| 5 | Argv | argv[0] is the file name, then the arguments; empty arguments and arguments with spaces are double-quoted |
| 14 | RandomSeed | a fixed value, so runs are reproducible |
| 16 | HosVersion | 20.0.0 in libnx's MAKEHOSVERSION encoding; 0 as the implementation magic, so libnx does not turn on its Atmosphere extensions |
| 0 | EndOfList | the loader string `kestrel <version>` |

Gaps:

- Not provided yet: SyscallAvailableHint, OverrideHeap and the other
  optional keys.
- argv[0] is the program's file name, not an `sdmc:/` path as hbmenu
  passes it, so libnx's __libnx_init_cwd leaves the working directory at
  the card's root.
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
handle gives InvalidHandle 0xe401 and a wrong subtype InvalidCombination
0xe801 (kestrel's choice; Switchbrew lists no result codes for svcGetInfo).

Gaps:

- Types 9, 10, 19, 20, 24-26 and unknown types stop with an explicit
  "unimplemented svcGetInfo type" message. Types 0 (CoreMask) and 1
  (PriorityMask) came with threads.
- The ELF test programs stay at 0x200000, below the address space.

Memory system calls, from Switchbrew's "SVC" page (ABIs, MemoryType and
MemoryAttribute tables, result codes). Every mapping carries its type:

| Mapping | MemoryType |
|---------|------------|
| Image, read-only parts | Code 0xdc7e03 |
| Image, writable parts | CodeData 0x3febd04 |
| Heap, svcMapPhysicalMemory, homebrew config | Normal 0x37ebd05 |
| Main thread stack, svcMapMemory destination | Stack 0x5c3c0b |
| Thread local region pages | ThreadLocal 0x40200c |

svcQueryMemory merges neighbours with the same type, attribute and
permission. Unmapped memory is reported as the gap up to the nearest
mappings: Free inside the address space, Inaccessible below or above it.
svcSetHeapSize grows and shrinks in place; svcMapMemory moves the source
bytes to the destination while the source is locked, which no program can
tell apart from shared pages.

Gaps:

- Where Switchbrew is silent, kestrel chose: the main stack and the
  homebrew config types, the reporting of unmapped and out-of-space
  addresses, accepting only Normal memory for svcSetMemoryPermission and
  svcSetMemoryAttribute, and InvalidCombination 0xe801 for an attribute
  other than Uncached.
- svcMapPhysicalMemory needs a system resource on the console (0xfa01
  without one); kestrel does not model the system resource and allows it.
- Memory is not shared yet (IPC and shared memory come later), so the
  reference counts in MemoryInfo are always 0.

Handles and kernel objects, from Switchbrew's "SVC" page (CloseHandle,
GetThreadId, GetProcessId, CreateEvent, SignalEvent, ClearEvent,
ResetSignal, CreateTransferMemory, MemoryState) and its "Error codes" page
(result names and values). Objects live in a per-process handle table
(`hle/kernel.rs`) for as long as a handle names them.

| Object | Handles | Behaviour |
|--------|---------|-----------|
| Thread | one per thread; the main thread's is MainThreadHandle | svcGetThreadId |
| Event | a writable and a readable side | the writable side signals; either side clears; svcResetSignal takes the readable side, 0xfa01 when unsignaled |
| TransferMemory | one | the range takes the given permission and Locked until the handle closes, then gets each block's old permission back |

The pseudo-handles name the caller: 0xffff8000 the current thread
(svcGetThreadId) and 0xffff8001 the current process (svcGetProcessId,
svcGetInfo). Wrong kinds, closed handles and handle 0 give InvalidHandle
0xe401.

Gaps:

- Where Switchbrew is silent, kestrel chose: the handle format (15-bit slot
  index, 15-bit counter so a closed handle stays invalid), 1024 handles per
  process (OutOfHandles 0xd201 when full), process ID 0x80 and main thread
  ID 0x100, InvalidHandle for closing a pseudo-handle, and for
  svcCreateTransferMemory: write-only refused with 0xd801, memory with an
  attribute or outside the address space refused with 0xd401.
- svcResetSignal on a process stops as unsupported: process state changes
  are not modelled.
- Shared memory arrived with the first service that hands one out
  (hid); see the first services notes. A program cannot create one
  itself (svcCreateSharedMemory is for system modules).
- No process handles; svcGetProcessId accepts only the pseudo-handle.
- The receiving side of transfer memory (svcMapTransferMemory) belongs to
  services and comes with them.

Fixed while doing this: the InvalidCombination constant held 0xf601
(SessionClosed); it is 0xe801. The kernel result constants now carry the
names of Switchbrew's error table (0xd401 InvalidCurrentMemory, 0xd801
InvalidNewMemoryPermission, 0xdc01 InvalidMemoryRegion, 0xfa01
InvalidState).

Threads, from Switchbrew's "SVC" page (CreateThread, StartThread,
ExitThread, SleepThread, the priority and core mask calls,
GetCurrentProcessorNumber, GetSystemTick) and its "Thread Local Region"
page. Each thread has a record in `hle/thread.rs` and a kernel object; the
main thread is the first.

| Call | Behaviour |
|------|-----------|
| svcCreateThread | priority 0 to 0x3F (InvalidPriority 0xe001), processor 0 to 3 or -2 (InvalidCoreId 0xe201); the thread starts with X0 = argument, SP = stack top, TPIDRRO_EL0 = its zeroed thread local region |
| svcStartThread | the thread becomes ready; a higher priority on the caller's core takes over at once |
| svcExitThread | the thread ends; its handles stay valid; the process ends with its last thread |
| svcSleepThread | 0, -1, -2 yield; a positive time sleeps on the process clock |
| svcSet/GetThreadPriority | 0 to 0x3F; the thread goes behind its new equals |
| svcSet/GetThreadCoreMask | ideal core and affinity mask; the thread moves to its new ideal core |

Scheduling is not documented, so the model is kestrel's, chosen to behave
like the console where programs can tell and to repeat exactly:

- Four virtual cores. On each, the ready thread with the best priority
  runs; equal priorities take turns only on yield, as the console does
  not time-slice them.
- The cores take turns on one host thread: on yield, sleep and exit, and
  after a slice of 19,200 instructions (about 19 µs at 998.4 MHz), so a
  thread spinning on one core cannot starve the others.
- One clock for the process, the 19.2 MHz counter (`kestrel_cpu::Clock`,
  the owner's decision of 2026-10-30, `PLAN.md` section 3). Counted, for
  the self-test and headless runs: one tick every 52 instructions
  (998.4 MHz), and when every thread sleeps the clock jumps to the next
  wake-up. On the host clock, with a window: real time, and when every
  thread sleeps the run loop waits on the host until the next wake-up
  (`Scheduler::idle_until`), at most 10 ms at a time.
- A thread switch clears the outgoing thread's exclusive monitor, so a
  store-exclusive another thread may have overtaken fails and retries.
  `hbthreads` fails without this (lost increments).
- Thread local regions are 0x200 bytes from page + 0x200 (slot 0 of each
  page is reserved), zeroed, with pages mapped as needed and slots reused.

Gaps:

- Where Switchbrew is silent, kestrel chose: core 0 for the main thread
  and for ProcessorId -2, main thread priority 0x2c, thread IDs counting
  up from 0x100, InvalidState 0xfa01 for starting a thread twice, the
  svcSetThreadCoreMask checks (core 0 to 3 and a non-empty mask of cores
  0 to 3, else InvalidCoreId; the ideal core in the mask, else
  InvalidCombination), the slice length, and stopping with an error when
  the only threads left were never started.
- Threads never migrate between cores, so the three yield kinds behave
  alike and the affinity mask only constrains svcSetThreadCoreMask.
- Undocumented values stop the process: svcSleepThread with other
  negative times, svcSetThreadCoreMask with a negative ideal core.
- svcGetSystemTick, CNTPCT_EL0 and CNTVCT_EL0 read the process clock
  (CNTPCT_EL0 since the time service: libnx's armGetSystemTick reads
  it). The counted clock's 52 instructions a tick assume one instruction
  a cycle, kestrel's choice: no cache or pipeline timing is modelled.
  `asm_checks` spins (at most 1000 times) until the counter moves, as
  two close reads of a 19.2 MHz counter on a 1 GHz CPU usually match on
  a console too.
- On the host clock, runs do not repeat: when threads wake and which
  core's slice an event lands in depend on host speed. Everything that
  must repeat runs on the counted clock.
- No limit on the number of threads beyond the handle table.

Synchronisation, from Switchbrew's "SVC" page (WaitSynchronization with
its object types and result codes, CancelSynchronization, the argument
lists of the arbitration calls, ArbitrationType and SignalType) and the
protocol of libnx's open-source mutex and condition variable
(`nx/source/kernel/mutex.c`, `condvar.c`), which runs on the console.
The code is in `hle/sync.rs`; a waiting thread records what it waits for
and an optional deadline on the process clock.

| Call | Behaviour |
|------|-----------|
| svcWaitSynchronization | up to 0x40 readable events and threads (0xee01 beyond); unreadable list 0xe601; pseudo-handles and other kinds 0xe401; returns the first signaled index, TimedOut 0xea01, or Cancelled 0xec01; holds a reference to each object while waiting |
| svcCancelSynchronization | ends the thread's wait with Cancelled, or makes its next wait return it |
| svcArbitrateLock | waits while the mutex word reads owner handle \| 0x40000000, returns at once otherwise |
| svcArbitrateUnlock | hands the mutex to the best waiter (priority, then longest waiting) with 0x40000000 if others remain, or writes 0 |
| svcWaitProcessWideKeyAtomic | X0 = mutex word, X1 = condition variable (libnx's order); releases the mutex, waits; owns it again on success, not after TimedOut |
| svcSignalProcessWideKey | wakes the best `count` waiters (all for count <= 0); each takes its mutex back or waits for it |
| svcWaitForAddress | WaitIfLessThan, WaitIfEqual |
| svcSignalToAddress | Signal, SignalAndIncrementIfEqual |

svcSignalEvent and thread exit wake the threads waiting on them. When
every thread waits with no deadline, the process stops with a deadlock
report naming what each thread waits for, for example "thread 0x103 waits
for the mutex at 0x80030c4 held by thread 0x100".

Gaps:

- Switchbrew labels svcWaitProcessWideKeyAtomic's X0 KeyAddress and X1
  TagAddress; libnx passes the mutex in X0 and the condition variable in
  X1, and kestrel follows libnx because it runs on the console.
- Where the sources are silent, kestrel chose: the first signaled handle
  in list order wins; a pending cancel is reported after the arguments
  are checked and before signaled objects; timeout 0 returns TimedOut
  without waiting (for the condition variable, after releasing the
  mutex); unaligned words give InvalidAddress and words not mapped
  read-write InvalidCurrentMemory; a signalled condition variable waiter
  whose mutex word names no thread gets InvalidHandle; for address
  arbitration, InvalidState when the condition does not hold and
  InvalidEnumValue 0xf001 for unknown types; count <= 0 wakes every
  waiter for svcSignalToAddress too.
- Stop as unsupported: DecrementAndWaitIfLessThan (decrementing always
  or only when waiting is not documented) and
  SignalAndModifyBasedOnWaitingThreadCountIfEqual (no documented rule).
- The condition variable word is never written; libnx never reads it.
  Official software may, which phase 6 will show.
- No priority inheritance for mutex owners.
- A thread that exits holding a mutex leaves its waiters waiting; the
  deadlock report names them.

IPC, from Switchbrew's "IPC Marshalling" page (header, special header,
descriptors, command types, the CMIF and TIPC formats), its "SVC" page
(svcConnectToNamedPort, svcSendSyncRequest) and "Services API" page
(sm:), with field widths and the order of a message's parts as libnx's
open-source `hipc.h`, `cmif.h` and `tipc.h` build them. Parsing and
encoding are in `hle/ipc.rs`; sessions and services in
`hle/service/`. Services are written in Rust and answer at once, so a
request never blocks and other threads do not run meanwhile.

| Call or command | Behaviour |
|-----------------|-----------|
| svcConnectToNamedPort | "sm:" is the only port; a new client session handle |
| svcSendSyncRequest | reads the message from the caller's thread local region and writes the reply there; CMIF Request and RequestWithContext, TIPC (command type 16 + n) for services that take it, and Close; SessionClosed 0xf601 after a close request |
| sm: RegisterClient (0) | needs the process ID descriptor; registers the session |
| sm: GetServiceHandle (1) | moves a session handle to the named service back |

The process ID the kernel inserts is the one svcGetProcessId reports.
sm: takes CMIF and TIPC: Switchbrew says it became a TIPC interface in
12.0.0, and libnx still sends it CMIF on every version.

Gaps:

- Where the sources are silent, kestrel chose: an unreadable port name
  is InvalidPointer 0xe601 and one with no NUL in its first 12 bytes
  OutOfRange 0xee01; sm:'s GetServiceHandle before RegisterClient is
  InvalidClient 0x415 and an empty name or characters after the zero
  padding InvalidServiceName 0xc15; registering is per session; a close
  request succeeds; replies carry command type 0 and CMIF replies version
  0 and token 0 (libnx reads none of these).
- Asking sm: for a service kestrel does not implement stops with its
  name ("unimplemented service ...").
- Stop as unsupported: other named ports; sm: RegisterService,
  UnregisterService and DetachClient (guest servers need server sessions
  and svcReplyAndReceive); CMIF control requests other than
  ConvertCurrentObjectToDomain and CloneCurrentObject
  (CopyFromCurrentDomain, QueryPointerBufferSize, CloneCurrentObjectEx,
  and converting a domain again); domain objects as arguments; legacy
  command types 1 and 3; moved handles in requests; receive list modes 1
  and 2 and explicit receive list offsets.
- Services read pointer (X) and send (A) buffers and write receive list
  (C) and receive (B) buffers; none uses exchange (W) buffers yet.
- Switchbrew notes that on Switch 2 every error from the SendSyncRequest
  calls closes the session (`docs/SWITCH2_NOTES.md`); not modelled.

First services. The system version kestrel reports is 20.0.0, the
owner's choice (the first major version of the Switch 2 era), held in
`hle::firmware`; every service answers as Switchbrew documents that
version. Homebrew gets it in the HosVersion loader entry, so libnx does
not ask set:sys. Each service follows its Switchbrew page and the
requests libnx's startup sends (`__appInit` in `runtime/init.c`,
`applet.c`, `apm.c`, `set.c`). The code is in `hle/service/` (`set.rs`,
`am.rs`); domains are in `service/mod.rs`.

Domains ("IPC Marshalling"): ConvertCurrentObjectToDomain makes the
session's object domain object 1 (kestrel's choice of ID); a command's
new interfaces become new objects with the lowest free IDs, listed after
the reply's data; a domain Close request frees an ID. Outside a domain,
each new interface is a new session, moved to the client.

| Service | Commands |
|---------|----------|
| set:sys | GetFirmwareVersion2 (4): the 0x100-byte FirmwareVersion through the client's receive buffer: 20.0.0, ReleaseNumber and Revision 0, kestrel's own Platform, DisplayVersion "20.0.0" and DisplayName |
| apm | OpenSession (0), GetPerformanceMode (1) |
| appletOE | OpenApplicationProxy (0): needs the process ID and a copy of the current-process pseudo-handle |
| IApplicationProxy | the getters 0-4, 11, 20 and 1000 |
| ICommonStateGetter | GetEventHandle (0), ReceiveMessage (1), GetOperationMode (5), GetPerformanceMode (6), GetCurrentFocusState (9) |
| ISelfController | SetOperationModeChangedNotification (11), SetPerformanceModeChangedNotification (12), SetFocusHandlingMode (13), SetOutOfFocusSuspendingEnabled (16), GetAccumulatedSuspendedTickChangedEvent (91) |
| IWindowController | GetAppletResourceUserId (1), AcquireForegroundRights (10) |
| IApplicationFunctions | NotifyRunning (40) |

Gaps:

- Where the sources leave it to the console's state, kestrel chose: the
  application is always in focus (FocusState 1) and never suspended, so
  no applet message arrives (ReceiveMessage 0x680) and neither event is
  ever signaled; docked (OperationMode 1, Console) with PerformanceMode 1
  (Boost); the applet resource user ID is the process ID; NotifyRunning
  answers 0; the notification and focus-handling flags are accepted and
  have no effect.
- set:sys GetFirmwareVersion (3) stops: Switchbrew says Switch 2 removed
  it (`docs/SWITCH2_NOTES.md`). Switch 2 software no longer uses apm;
  kestrel answers it for libnx.
- Every other command of these interfaces stops with its interface and
  number, as do IAudioController, IDisplayController,
  ILibraryAppletCreator, IDebugFunctions and IApmSession, which libnx
  opens but does not call at startup. Only one appletOE session should
  exist at a time ("Applet Manager services"); kestrel does not enforce
  it.

Shared memory, from Switchbrew's "SVC" page (MapSharedMemory and
UnmapSharedMemory arguments, the reference a mapping holds, MemoryType
Shared 0x00402006), in `hle/memory.rs`. A shared memory object holds its
bytes, the permission the process may map it with, and where it is
mapped; services make them and keep a reference.

- Where Switchbrew gives only the arguments, kestrel chose the checks,
  with the other memory calls' result codes: a shared memory handle
  (0xe401), page-aligned address and size (0xcc01, 0xca01), the object's
  size (0xca01), inside the address space and outside the heap and alias
  regions (0xdc01), free (0xd401), the object's permission (0xd801).
  svcUnmapSharedMemory of anything but the object's mapping is 0xd401.
- One mapping per object at a time; a second stops as unsupported.
- The service's own reference keeps every object alive so far, so the
  mapping's reference cannot be observed yet; it will be with the first
  object no service keeps.

hid, from Switchbrew's "HID services" page and the requests libnx's
hidInitialize and padConfigureInput send (`hid.c`, `pad.c`), in
`hle/service/hid.rs`:

| Command | Behaviour |
|---------|-----------|
| CreateAppletResource (0) | hands out IAppletResource |
| IAppletResource GetSharedMemoryHandle (0) | a copy handle to the one 0x40000-byte shared memory, mapped read-only |
| ActivateTouchScreen (11), ActivateMouse (21), ActivateKeyboard (31), ActivateNpad (103) | accepted |
| SetSupportedNpadStyleSet (100), GetSupportedNpadStyleSet (101) | the style bits, kept |
| SetSupportedNpadIdType (102) | up to 10 NpadIdTypes from the pointer (X) buffer, kept |
| ActivateNpadWithRevision (109) | the revisions Switchbrew lists (0, 1, 2, 3, 5), kept |

Each takes the process ID descriptor and the applet resource user ID;
an ID other than the one appletOE hands out stops, as do NpadIdType
lists and revisions Switchbrew does not allow.

Gaps:

- Without an input source (the self-test, `--headless`) the shared
  memory stays zeroed: no controller is connected, which libnx reads as
  no input. With one, see "Host input" below.
- Every other hid command stops with its number, including vibration,
  six-axis sensors and the npad style-set update event.

Time, from Switchbrew's "Glue services" (time:u, ISystemClock,
ISteadyClock, ITimeZoneService and their structures) and the requests
libnx's timeInitialize and __libnx_init_time send (`time.c`, `newlib.c`),
in `hle/service/time.rs`. The owner chose what time it is
(`hle::firmware`): the clocks start at 2025-06-05 00:00:00 UTC (POSIX
1749081600, the Switch 2's launch day) and the time zone is UTC. The
clocks run with the process clock, so every run sees the same times and
a sleep moves them by its length.

| Command | Behaviour |
|---------|-----------|
| time:u 0, 1, 4 | the user, network and local ISystemClock |
| time:u 2, 3 | ISteadyClock, ITimeZoneService |
| time:u GetSharedMemoryNativeHandle (20) | a copy handle to the one 0x1000-byte shared memory, mapped read-only |
| ISystemClock GetCurrentTime (0), GetSystemClockContext (2) | the start time plus the steady clock; the context |
| ISteadyClock GetCurrentTimePoint (0) | whole seconds of the process clock, and the source ID |
| ITimeZoneService GetDeviceLocationName (0) | "UTC" |
| ITimeZoneService ToCalendarTimeWithMyRule (101) | the UTC date, 0-based day of the week and year, "UTC", no DST, offset 0 |

The CPU reads CNTPCT_EL0 as the process clock, as CNTVCT_EL0 (the
virtual offset is 0): libnx reads the clocks from the shared memory with
it.

Gaps:

- Switchbrew does not describe the shared memory; kestrel writes the
  layout libnx reads: the steady clock (base time in nanoseconds, source
  ID) at 0x00, the local system clock's context at 0x38 (libnx reads it
  for the user clock too), the network clock's at 0x80, each a u32
  counter, padding and two copies, the counter's low bit picking one.
  Nothing changes them, so the counter stays 0.
- Where the sources leave it to the console's state, kestrel chose: the
  steady clock starts at 0 when the process starts; every system clock
  has the same context (the start time at steady time 0), so the network
  clock is always in step; the clock source ID is the text "kestrel"
  padded with zeros; month and day count from 1 (Switchbrew calls only
  the day of the week and of the year 0-based); a year CalendarTime's u16
  cannot hold stops.
- No time zone rules: LoadTimeZoneRule, ToCalendarTime with a rule,
  ToPosixTime and the location name list stop, as do setting a clock or
  the location, time:a, time:r and every other time command. A zone with
  daylight saving time would need the TZif rule format and zone data.

File system, from Switchbrew's "Filesystem services" (fsp-srv,
IFileSystem, IFile, IDirectory, DirectoryEntry, OpenFile's mode bits),
"HIPC" (CloneCurrentObject) and "Error codes" (module 2), and the
requests libnx's fsInitialize, fsdevMountSdmc and "sdmc:" device send
(`fs.c`, `fs_dev.c`, `sessionmgr.c`), in `hle/service/fs.rs`.

fsInitialize makes fsp-srv a domain and clones the session twice, then
sends requests for the domain's objects over any of the three. So
CloneCurrentObject hands out a new session to the same object, and a
clone of a domain session shares its domain (`service::Domain`, shared
and reference-counted). A domain's objects go when the last of its
sessions closes.

The owner chose where the card is, as Dolphin's portable mode keeps its
user folder: `user/sdmc` next to the kestrel executable, made on first
use, or the folder `--sd` names. Writes reach the folder. Games load from
any path; a games directory setting comes with the frontend. The
self-test gives each homebrew program a new empty folder in the system's
temporary directory.

| Command | Behaviour |
|---------|-----------|
| fsp-srv SetCurrentProcess (1) | needs the process ID |
| fsp-srv OpenSdCardFileSystem (18) | the card's IFileSystem; with no folder, PortSdCardNoDevice 0xfa202 |
| IFileSystem CreateFile (0), DeleteFile (1), CreateDirectory (2), DeleteDirectory (3), DeleteDirectoryRecursively (4), CleanDirectoryRecursively (13) | on the folder |
| IFileSystem RenameFile (5), RenameDirectory (6) | the two paths in pointer buffers 0 and 1 |
| IFileSystem GetEntryType (7) | 0 directory, 1 file |
| IFileSystem OpenFile (8), OpenDirectory (9) | an IFile with its mode; an IDirectory with its filter |
| IFileSystem Commit (10) | nothing to do |
| IFileSystem GetFileTimeStampRaw (14) | all zero (no timestamps), so runs repeat |
| IFile Read (0), Write (1), Flush (2), SetSize (3), GetSize (4) | through receive (B) and send (A) buffers |
| IDirectory Read (0), GetEntryCount (1) | DirectoryEntry records |

Paths are UTF-8 in a 0x301-byte pointer buffer and start with '/'.
Kestrel checks them the same way on every host and never follows a host
link, so no path leads outside the folder.

Gaps:

- Where Switchbrew lists a result but not when it applies, kestrel chose:
  no NUL is TooLongPath; not UTF-8, a control character, one of
  `\ : * ? " < > |`, a name ending in '.' or ' ', or a Windows device
  name is InvalidCharacter; no leading '/' is InvalidPathFormat; an
  empty, "." or ".." component is NotNormalized; a missing entry, the
  wrong kind of entry or a path through a file is PathNotFound; an
  existing one is PathAlreadyExists; deleting or renaming an open file,
  or a directory with something open in it, is TargetLocked; a negative
  offset is InvalidOffset and a negative size InvalidSize.
- Kestrel's other choices: a host link, or a path through one, hides what
  it points at (creating there is PathAlreadyExists, anything else
  PathNotFound, listings leave it out); reading at or past the end reads
  0 bytes; a directory lists in name order, read when it is opened;
  GetEntryCount counts every entry, read or not; the archive bit is never
  set; SetSize needs the write bit.
- Stop, as Switchbrew gives no result for them: a command the open mode
  does not allow (only the range 6200-6299, InvalidOperationForOpenMode,
  is listed); deleting a directory that is not empty; renaming a
  directory into itself; deleting the root. Also stop: OpenFile modes
  other than read, write and append (append needs write), OpenDirectory
  filters other than bits 0 and 1, CreateFile options (concatenation
  files), and read or write options other than write's flush bit.
- Not implemented: free and total space (11, 12), QueryEntry, OperateRange,
  the other file systems (save data, RomFS, BIS), and every other fsp-srv
  command. Host errors (a full disk, permissions) stop with the host's
  message.
- The SD card's FAT file system ignores case; Windows and macOS folders
  do too, Linux folders do not, so a program that mixes cases can see a
  difference there.
- OpenSdCardFileSystem before SetCurrentProcess stops (kestrel's choice):
  the file system does not know the process yet.

Power state, from Switchbrew's "PTM services" (psm, IPsmSession,
ChargerType, BatteryVoltageState) and libnx's `psm.c`, in
`hle/service/psm.rs`. kestrel runs docked (am's OperationMode Console),
so psm answers as a docked console on its charger, kestrel's choice:

| Command | Answer |
|---------|--------|
| GetBatteryChargePercentage (0) | 100 |
| GetChargerType (1) | 1, EnoughPower |
| IsBatteryChargingEnabled (4), IsEnoughPowerSupplied (14) | true |
| GetBatteryVoltageState (12) | 3, Good |
| GetRawBatteryChargePercentage (13), GetBatteryAgePercentage (15) | 100.0 |
| OpenSession (7) | IPsmSession |
| IPsmSession BindStateChangeEvent (0) | an event nothing signals: the state never changes |
| IPsmSession UnbindStateChangeEvent (1), the three Set...ChangeEventEnabled (2-4) | accepted |

Error reports and logging. Homebrew prints through svcOutputDebugString,
which kestrel already passes on; libnx has no client for the log service
(lm) that official software uses. What a program reports when it fails
is now readable:

- svcBreak (Switchbrew, "SVC"): with bit 31 of the reason set it only
  notifies a debugger, so kestrel notes it and returns 0. Otherwise the
  program stops (`Stop::Abort`) with the reason (named as libnx's
  BreakReason) and, when the size argument is 4, the result libnx's
  diagAbortWithResult passes by address.
- fatal:u ThrowFatalWithPolicy (Switchbrew, "Fatal services"), with the
  process ID: FatalType ErrorReport (1) only files a report, so kestrel
  notes it and the program goes on; ErrorReportAndErrorScreen (0) and
  ErrorScreen (2) end it (`Stop::Fatal`).
- `hle/results.rs` writes a result as Switchbrew's "Error codes" page
  does (2000 plus the module, then the description: "2002-0001") with
  the module's name from that page, and libnx's own modules and error
  names from its `result.h`. It is generated from those two lists.
- Kestrel's notes go to `Process::log`, and `kestrel run` prints them on
  stderr as "kestrel: ...". The unimplemented-call log will use the same
  log.

Gaps:

- psm's charging controls (2, 3, 5, 6, 8-11), GetBatteryChargeInfoEvent
  and Fields, and the commands Switchbrew lists without names stop.
- fatal:u ThrowFatal (0) and ThrowFatalWithCpuContext (2) stop; libnx
  sends ThrowFatalWithPolicy.
- No lm: official software's log service is for later phases.

The unimplemented-call log. The owner chose to keep stopping at the
first unimplemented call and to record it. Every such stop is
`Stop::Unimplemented`, apart from other errors, and names the call:

| Kind | Example |
|------|---------|
| SVC, with its name from Switchbrew's SVC table (`svc_names.rs`, generated) | `SVC 0x7f (CallSecureMonitor)` |
| svcGetInfo type | `svcGetInfo type 9 (ResourceLimit), subtype 0x0` |
| named port | `named port "nope" (kestrel has only "sm:")` |
| service | `service "nosuch"` |
| command, with its name where kestrel knows it | `psm command 2`, `set:sys command 3 (GetFirmwareVersion, removed on Switch 2)` |
| CMIF control command | `CMIF control command 3 (QueryPointerBufferSize), sent to fsp-srv` |

`kestrel run` prints it with the program counter, appends a line to
`user/logs/unimplemented.log` next to the executable (the UTC time,
kestrel's version, the program's file name and the call), and exits with
code 5. The self-test does not write the log.

Gaps:

- Unimplemented instructions and IPC message features kestrel cannot
  read (receive list modes, domain objects as arguments) still stop as
  errors, not in the log: they are CPU or format gaps, not calls.
- The log grows without limit; it is a plain text file to read or delete.

Host input, from Switchbrew's "HID services" (SharedMemoryFormat,
NpadSharedMemoryEntry, NpadInternalState, the RingLifo and
NpadFullKeyState layouts, NpadButton, NpadAttribute, DeviceType,
NpadSystemProperties, AppletFooterUiType, ColorAttribute) and the way
libnx's padUpdate and hidGetNpadStates read them (`pad.c`, `hid.c`), in
`hle/input.rs` and `hle/service/hid.rs`; the window is in kestrel-cli.

The owner's decisions (2026-10-30):
- winit for the window and keyboard, softbuffer to paint it, gilrs for
  gamepads, only in kestrel-cli and behind its default `window` feature
  (`--no-default-features` builds a terminal-only kestrel);
- a homebrew NRO opens a window unless `--headless` (ELF test programs
  never do);
- one emulated controller for now: a Pro Controller as player 1, or the
  Joy-Con attached to the console (handheld), chosen in the settings
  file; the keyboard and every gamepad drive it.

How it works:
- A `kestrel_hle::input::Source` gives hid the controller's state. Every
  5 ms of guest time (kestrel's choice) hid writes a sample into the npad
  entry: on the first, the style, Dual assignment, colours, DeviceType,
  system properties and footer type; on each, a ring entry with the next
  sampling number, the buttons, the sticks and the connected attributes.
  libnx's reader needs consecutive sampling numbers. When the clock jumps
  because every thread slept, hid catches up with at most a ring's worth
  of samples.
- The controller connects once the program has activated npads and
  accepts its style and NpadIdType (padConfigureInput does both);
  otherwise kestrel notes it once.
- The window's event loop owns the main thread (macOS requires it); the
  program runs on a worker thread, where its Process is made. Closing the
  window sets `Process::interrupt`, and the program stops at its next
  slice (`Stop::Interrupted`). The window paints a dark grey background:
  graphics come in phase 5. Without a display, kestrel runs without a
  window and says so.
- With a window the program runs on the host's clock, in real time, and
  without an instruction budget (`--max-steps` still sets one): a program
  sleeping 16 ms per frame takes 16 ms, and one computing sees real time
  pass (first found by the owner trying padview, which ran its loop flat
  out). The self-test and headless runs count time instead, so they
  repeat exactly and skip idle time.
- `user/config/input.cfg` is written with the defaults when missing:
  `controller = pro` or `handheld`, each button's keys (`key:` and winit
  KeyCode names) and gamepad buttons (`pad:` and gilrs names, which go by
  position: Switch A is East), each stick's gamepad stick and direction
  keys. A line kestrel cannot read stops the run with its line number.
- `guest/demo/padview.c` prints the controller as it changes, to try the
  mapping by hand: `kestrel run guest/bin/padview.nro`.

| Choice (kestrel's) | Value |
|--------------------|-------|
| Pro Controller | player 1 (NpadIdType No1), wireless (IsConnected), DeviceType FullKey, footer SwitchProController |
| Handheld | NpadIdType Handheld, wired Joy-Con (all six attribute bits), DeviceType HandheldLeft and HandheldRight, footer HandheldJoyConLeftJoyConRight |
| System properties | IsAbxyButtonOriented, IsPlusAvailable, IsMinusAvailable, IsDirectionalButtonsAvailable |
| Colours | a dark grey body (0xff2d2d2d) and light grey buttons (0xffe6e6e6); the other colour block NoController |
| Stick direction buttons | pressed past half the stick's range |
| Ring count | stops at 16, so the next entry written is never one a reader counts |
| Gamepad dead zone | 10% of the stick's range |
| Keys and gamepad | keys win over a gamepad stick; with several gamepads, the stick pushed furthest |
| Host waits | at most 10 ms at a time, so closing the window stops the program soon; oversleeping needs no making up, as the clock is the host's |

Gaps:

- One controller; no other NpadStyleTag, no second player, no six-axis
  sensors, vibration, or Joy-Con colours from real hardware. Later tasks.
- Linux builds need libudev for gilrs (`libudev-dev` to build,
  `libudev.so.1` to run, on every desktop distribution).
- The window has only been run on Windows so far; CI builds it on every
  platform on the next release.
- Keyboard keys are matched by winit's KeyCode names, so a misspelt name
  in the settings file is not reported; it never matches.
- On the host clock the interpreter is slower than a console: libnx's
  console takes about 1.7 million instructions a frame, about 21 ms at
  the interpreter's 80 MIPS, so a console program draws about 47 frames
  a second instead of 60 (measured on the owner's PC with hello-world).
  The fast backends of phases 3 and 4 close the gap.

### Phase 3 notes

The memory arena refactor (owner's decision, 2026-10-30: do it now, on
Linux, before phase 3 on macOS; a refactor with no behaviour change,
designed for the real-time goal: six guest cores on host threads, the
arena mapped into a hypervisor VM, a recompiler with guard pages).

What was built (`docs/ARCHITECTURE.md`, "Memory", has the design and what
the native backend and the recompiler must do with it):
- `kestrel_core::host`: reserve and commit host address space (`mmap` and
  `mprotect` on Linux and macOS, `VirtualAlloc` on Windows, declared
  directly, so the core crates still use std only; `std::alloc` on other
  targets).
- `kestrel_core::Arena`: guest physical memory, one host range aligned to
  2 MiB, committed in 2 MiB steps, handed out in 4 KiB frames.
- `kestrel_core::Memory`: a two-level page table from guest pages to
  frames, beside the OS layer's regions; every method takes `&self`;
  accesses take no lock and use host atomics; compare-and-swap,
  `translate` and `epoch` for the fast backends.
- Callers did not change, apart from `let mut` bindings the `&self`
  methods no longer need (one in a `kestrel-cpu` test). `kestrel-cpu` is
  otherwise untouched: a separate session works on it, and the
  `CpuBackend` trait moves to `&Memory` with the first threaded backend.

| Choice (kestrel's) | Value |
|--------------------|-------|
| Arena size | 16 GiB of host address space per process (the Switch 2's 12 GiB and room); only what the guest uses is committed. `TOTAL_MEMORY` stays 1 GiB for now |
| Alignment and commit step | 2 MiB: a multiple of every host page size (4, 16, 64 KiB) and a stage-2 block |
| Address space | 39 bits (Switchbrew's largest, AddressSpace64Bit); mapping above stops as `MemFault::OutOfRange` |
| Freed frames | back to a free list, zeroed when handed out again; committed memory is not returned to the host while the process lives |
| Running out of arena | `MemFault::OutOfMemory`, which the memory calls already turn into their result codes |
| Guest accesses | relaxed host atomics: aligned ones in one access, unaligned byte by byte, 128 bits as two 64-bit halves |
| Atomic accesses not aligned to their size | `MemFault::Unaligned` (Arm faults them) |
| svcMapMemory and shared memory | unchanged: bytes are copied, not shared; the arena allows sharing frames later |

Verified (Linux x86_64 host):
- The self-test's output is the same as before, line for line (27/27,
  32,583,559 instructions), and the interpreter is faster: 102 to 110
  MIPS against 73 to 82 before, alternating runs of both builds (no
  B-tree search per access).
- The same self-test, built for aarch64 Linux and run under qemu-user,
  and built for Windows (x86_64-pc-windows-gnu) and run under Wine:
  27/27, the same output (only the header naming the host differs). The
  core crate's tests pass in both, so the `mmap` and `VirtualAlloc` paths
  ran.
- `scripts/check-optimised.sh` (every program at `-O3 -march=armv8.2-a`):
  13/13; the four libnx examples (`build-examples.sh` in the pinned
  container, `run-examples.sh`): 4/4.
- Six host threads on one `Memory`: 120,000 compare-and-swap increments
  all land and no aligned 64-bit value is read torn, 20 runs out of 20.
  Mutation checks: a compare-and-swap made of a load and a store, an
  aligned write split in two, and unmapping without freeing frames each
  fail a test.

CI (manual run of `ci.yml` on the branch, run 5, 2026-10-31,
https://github.com/Lordnns/kestrel/actions/runs/41149930809):
all 11 jobs green. The six native jobs (Linux, Windows and macOS, x86_64
and aarch64) build, test, lint, pass the self-test and the four libnx
examples, so the arena's host code ran natively: macOS's `mmap` on
Apple Silicon's 16 KiB pages and on Intel, and Windows' `VirtualAlloc`
on x86_64 and ARM64. The same run is the first CI check of the clock
(52 instructions a tick), which this branch builds on.

Not verified (no ARM host with a hypervisor here): the hypervisor and
recompiler constraints in `docs/ARCHITECTURE.md` are for phases 3 and 4
to check.

### Phase 4 notes

The CPU track (owner, 2026-10-31): the differential tester first, then
the x86_64 recompiler.

The differential tester (`crates/kestrel-testsuite/src/diff.rs`,
`kestrel diff --backend <b> [program...]`; `docs/ARCHITECTURE.md`,
"Correctness", has how it works). Two copies of a self-test program run
on the counted clock, one on the interpreter and one on the candidate,
compared every 100,000 instructions (registers, flags, vector and system
registers, the exclusive monitor, instructions retired, the clock, the
running thread, system calls with X0..X7, the memory map and every
writable page, the output, the stop); the first stretch that differs is
replayed and narrowed down 100 times at a time to one instruction.

| Choice (kestrel's) | Value |
|--------------------|-------|
| Comparison points | every 100,000 instructions, then every 1,000, 10 and 1 in the replays |
| Memory compared | the memory map, and the bytes of every page mapped readable and writable; read-only pages change only through system calls, which are compared |
| System calls | each side's backend is wrapped and records every `Exit::Svc` with X0..X7 and the instructions retired |
| A stretch that does not diverge again in smaller steps | reported as the stretch, saying so |

Verified: the interpreter against itself on all 27 self-test programs,
zero divergences (`cargo test`, and `kestrel diff` on Linux x86_64). An
interpreter with a deliberate bug from the nth time an instruction runs
(an ADD result, the carry of SUBS, a value STR stores, a system call
number) is found at exactly the first instruction the bug struck, with
the register, flag, memory word or call named first; disabling the
memory comparison fails the memory test.

Limit: a wrong value overwritten before the next comparison point (a dead
flag) leaves no trace in the state and is not reported, since the
program is not affected; a program that uses it shows it.

The x86_64 recompiler, first steps (2026-10-31; `docs/ARCHITECTURE.md`,
"JIT", has the design): IR, frontend, code generation in both calling
conventions, kestrel's own encoder, executable memory per OS, the block
cache and dispatcher, the interpreter fallback.

| Choice (kestrel's) | Value |
|--------------------|-------|
| Block length | at most 64 instructions, ending at a branch; a block longer than the budget left is translated again cut to fit, so the recompiler retires exactly what the interpreter would |
| Guest state | registers and NZCV in `CpuState` (memory), read and written by every operation; IR values in RBP, R13-R15 or stack slots |
| Memory accesses | an inline page-table walk (`Memory::raw_tables`) and a direct arena access; the `Memory` helper for anything unmapped, not permitted, crossing a page or beyond the table |
| Chaining | blocks with at most 24 stack slots share one frame and jump into each other's bodies through link cells (data, not patched code); a chained jump needs room for 64 instructions in the budget, else the block returns to the dispatcher; links reset when the memory's change count moved and on `invalidate`; computed jumps always return to the dispatcher |
| Code memory | 64 MiB per backend; when full, every translation is dropped and made again |
| Self-modifying code | each block keeps its guest words; when `Memory::epoch` moves (a mapping or permission changed) they are fetched and compared before it runs again; blocks on writable and executable pages are compared every time |
| A backend given another process's memory | drops every translation |
| The default backend | still the interpreter (`auto`); `--backend jit` selects the recompiler. Making it the default is the owner's call |

Verified (Linux x86_64 host, this session):
- Every one of the 494 integer vectors (`int_vectors.rs`, expected values
  from the Python models) runs as a one-instruction block, in the System V
  and the Windows convention.
- The encoder matches llvm-mc on all 5,359 cases.
- The self-test: 27/27 on `--backend jit`; `kestrel diff --backend jit`:
  zero divergences on all 27 programs, and the same in `cargo test` in
  both conventions, with blocks of up to 64 and of 1 instruction.
- Windows: the same build for `x86_64-pc-windows-gnu`, run under Wine
  (VirtualAlloc, VirtualProtect, FlushInstructionCache, the Windows
  convention): self-test 27/27 on the recompiler, zero divergences, every
  recompiler and testsuite test passes. Not yet run on Windows itself;
  the CI job on `windows-2022` will be the first. (Under Wine an
  interpreter test, `fp_tests::flush_to_zero`, fails on a double FMA
  edge case, on the code before this work too: mingw's or Wine's `fma`,
  not kestrel's; CI on Windows with MSVC passes it.)
- Mutation check: setting C from the host's carry inverted makes `kestrel
  diff` report `cmp x0, #2` in algos and `cmp x12, #7` in hbthreads as
  the first divergence, with NZCV named.
- Speed (self-test, Linux x86_64 cloud machine, one run each): 254 MIPS
  on the recompiler against 62 on the interpreter. Straight-line code
  gains most: sha256 527 MIPS (14.6 instructions a block), bigint 592;
  branchy code less: algos 268 (5.2 a block), hbthreads 182 (3.7),
  where returning to the dispatcher after every block dominates.
- Then chaining and inline memory accesses (same machine, three runs
  of each backend): the self-test 202 to 337 MIPS against 65 to 75 on
  the interpreter. The total swings with the machine and is dominated by
  translating the many short programs; long ones are steady: sha256
  838, bigint 807, algos 554, hbthreads 319 (interpreter about 70).
  New tests run small programs on both backends with budgets from 1 to
  19,200 and compare: a fault part-way through a block, a store to
  code, accesses crossing into a page whose frame is elsewhere in the
  arena, halfwords, rewritten code (through a permission change and
  through `invalidate`). Mutation checks: dropping the chained jump's
  budget check, or the inline walk's page-crossing check, each fails a
  test.
- The four libnx examples (built in the pinned container, run by
  `run-examples.sh` with `--backend jit`): 4/4. hello-world headless on
  the counted clock with `--stats`, 503 million instructions and 243
  frames: interpreter 51 MIPS, 25.3 fps average, 1% low 14.1 fps, worst
  75.8 ms; recompiler 207 MIPS, 103.6 fps, 1% low 75.6 fps, worst
  22.7 ms (the console redraws every frame through the libnx console,
  so this is mostly guest code; the fps is host time between frames,
  not the console's 60 Hz pacing, which only a window's host clock has).
- What still runs in the interpreter: 0.7% of the self-test's
  instructions (223,459 of 32,583,559): exclusives (LDXR/STXR in the
  thread programs' spin loops), CLZ/REV/RBIT, system registers, floating
  point and NEON.

## Next task

The owner's decisions of 2026-10-31 ("Plan changes (2026-10-31, the
Windows milestone)"): Windows x86_64 first, with two tracks in parallel.

1. The CPU track, in this order:
   - A. Done (2026-10-31, "Phase 4 notes"): the differential tester,
     backend-agnostic: the interpreter and another backend side by side
     on the counted clock, reporting the first divergence (registers,
     flags, memory writes, system calls); proven with a deliberately
     faulty interpreter variant.
   - B. The x86_64 recompiler (phase 4) behind `create_backend(Jit)`.
     Done (2026-10-31, "Phase 4 notes"): the IR, the frontend, kestrel's
     encoder checked against llvm-mc, code generation in the Windows x64
     and System V conventions, executable memory per OS, the block cache
     with the memory's change count, the interpreter fallback, integer,
     branches, loads and stores, chaining and inline memory accesses;
     self-test 27/27 and zero divergences in both conventions. The
     remaining steps, in this order; each must keep the differential
     tester at zero divergences on every self-test program (`kestrel diff
     --backend jit`, and `cargo test`, which runs both conventions):
     1. Check the first CI run on `windows-2022` (a manual run of
        `ci.yml` on the branch, or a tag): the recompiler has run on
        Windows only under Wine so far. Then ask the owner whether
        `auto` should pick the recompiler on Windows and Linux x86_64.
     2. Translate what still runs in the interpreter, most frequent
        first (measure with a count per encoding in `jit::interp`):
        LDXR/STXR and the other exclusives (as a host compare-and-swap
        of the value LDXR saw, `docs/ARCHITECTURE.md`, "Memory"), CLZ,
        REV, RBIT, MRS/MSR of TPIDR_EL0, NZCV, FPCR and FPSR; counter
        reads need the retired count flushed first, as `Op::Interp`
        does.
     3. Floating point and NEON: first as calls to the interpreter's own
        routines (`float.rs`, the NEON lane helpers) with operands in
        `CpuState::v`, so results stay identical on every host; then
        host SSE where the Arm rules allow (round-to-nearest arithmetic
        on finite values, with the NaN and flush-to-zero handling of
        `float.rs` around it).
     4. Speed: computed jumps (RET, BR, BLR) looked up in generated code
        (a return-address stack or a small PC-indexed table) instead of
        returning to the dispatcher; guest registers kept in host
        registers inside a block and written back at exits and calls;
        NZCV only computed when read or at a block's exit (lazy flags).
        Measure with `kestrel selftest` (three runs) and `kestrel run
        --stats` on the libnx examples.
     5. One host thread per guest core (`PLAN.md` section 3, "Time and
        cores"), once the parallel session's thread-safe OS layer is in:
        one `Jit` per core or a shared, locked cache; the link cells
        assume the memory's change count cannot move inside one `run`,
        which another core breaks, so links must then be checked or
        reset when the OS layer changes mappings.
     6. `kestrel diff` on any program (an NRO path, not only the
        self-test's), to run the libnx examples and later homebrew side
        by side.
   - C. Done (2026-10-31): per-backend speed in the self-test output,
     and `kestrel run --stats` with frame times (average, 1% low, worst)
     and instructions per second.
2. The graphics track: phase 5A, then 5B, on Vulkan (lavapipe on Linux,
   real GPUs on Windows).

Other candidates, when the owner asks for them: a licence-notices file in
the release packages (they bundle MIT/Apache crates: winit, softbuffer,
gilrs and their dependencies); more homebrew, fixing what
`user/logs/unimplemented.log` names; the later input tasks (other
NpadStyleTag controllers, several players, real Joy-Con motion, rumble
and pairing). CPU features left for the owner to decide: LDAPR
(FEAT_LRCPC), the non-hint pointer authentication instructions (PACIA,
AUTIA, RETAA, LDRAA; FEAT_PAuth) and FMLAL/FMLSL (FEAT_FHM) stop as
undefined; check them against the Cortex-A78C's documentation before
adding. The hint-space PAC instructions (PACIASP and the like) already
run as NOP.

Each service so far followed the same steps; keep them:

1. Sources: the Switchbrew page as wiki source
   (`https://switchbrew.org/w/index.php?title=<Page>&action=raw`, for
   example `HID_services`, `Applet_Manager_services`, `SVC`), and the
   libnx client code that sends the requests (`nx/source/services/*.c`,
   `nx/source/runtime/init.c`, from
   `raw.githubusercontent.com/switchbrew/libnx/master/`), read for the
   protocol only. Switch 2 notes ([S2]) go into `docs/SWITCH2_NOTES.md`
   first.
2. Code in `crates/kestrel-hle/src/service/<service>.rs`, with unit tests
   through IPC in `service/mod.rs`. Where the sources are silent, choose,
   say so in a comment, and list the choice in these notes; anything
   undocumented stops with its interface and command number.
3. A homebrew test program `guest/src/nro/hb<service>.c` (with an empty
   `.args`) that sends the requests the way libnx lays them out, a model
   in `scripts/gen-expected.py` written from the documentation and
   kestrel's listed choices, and a `homebrew!` entry in
   `crates/kestrel-testsuite/src/lib.rs`.
4. Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets
   -- -D warnings` (on Windows also with `--no-default-features --target
   x86_64-unknown-linux-gnu`, which compiles the Unix-only tests),
   `cargo check --workspace --all-targets --target <t>` for the other
   desktop targets (`aarch64-apple-darwin`, `x86_64-apple-darwin`, and
   `x86_64-pc-windows-msvc` off Windows), `cargo test --workspace`,
   `scripts/gen-expected.py --check`, `kestrel selftest`; the program
   built at `-O3 -march=armv8.2-a` (`GUEST_OUT`, `GUEST_OPT` for
   `scripts/build-guest.sh`) gives the same output; one deliberate bug in
   the new code makes the program's output differ, then is undone.
5. Commits: one logical change each (code, test program, docs), with the
   ROADMAP, ARCHITECTURE and README updated in the same series. Push only
   when the owner says so.

### Phase 2 acceptance

Examples (switchbrew/switch-examples, public domain mark; libnx is
ISC), pinned to a commit: `graphics/printing/hello-world` (hello world),
`hid/read-controls` (input), `fs/sdmc` (file I/O), `misc/barrier`
(threads). Each prints with libnx's console, which draws its text on the
screen (`console_sw.c` through `framebuffer.c`), and waits for Plus.

The owner's decisions (2026-10-30):
- The real screen now: kestrel gets the frames libnx draws, shows them
  in the window, and CI compares the final frame with an expected image.
  This pulls the display path of phase 5 forward; the GPU itself stays
  in phase 5.
- Docker Desktop on the owner's PC, with the same public devkitPro
  container image as CI (`devkitpro/devkita64`, pinned), to build the
  examples locally. The built examples are not committed.

The path libnx takes to the screen (libnx `display/*.c`,
`services/vi.c`, `services/nv.c`, `nvidia/map.c`, `nvidia/fence.c`, read
for the protocol only; Switchbrew "Display services", "Nvnflinger
services", "NV services"):
- startup: `__nx_win_init` (linked when the program uses the console)
  opens vi:m, then vi:s, then vi:u (an application gets vi:u, command 0
  GetDisplayService with 0, then 100 GetRelayService), OpenDisplay
  "Default" (1010), am ISelfController 40 CreateManagedDisplayLayer,
  OpenLayer (2020, the native window parcel with the binder ID in a
  0x100-byte buffer), SetLayerScalingMode (2101);
- the native window: IHOSBinderDriver AdjustRefcount (1),
  GetNativeHandle (2, the buffer-free event), TransactParcel (3) carrying
  Android IGraphicBufferProducer parcels: CONNECT, SET_PREALLOCATED_BUFFER
  (the nvmap ID, offset and block-linear layout of each buffer),
  DEQUEUE_BUFFER (no fences), REQUEST_BUFFER, QUEUE_BUFFER,
  DISCONNECT;
- nvdrv: Initialize (3, with transfer memory), clone with control
  command 4 (CloneCurrentObjectEx), SetClientPID (8), Open (0)
  `/dev/nvmap` and `/dev/nvhost-ctrl`, Ioctl (1), Close (2); nvmap
  Create, Alloc (the program's own memory), GetId, FromId, Param, Free;
- the pixels stay in the program's memory: on QUEUE_BUFFER kestrel reads
  the buffer through its nvmap ID, undoes the block-linear layout, and
  converts the colour format (RGB565 for the console).

Steps:
1. Done: Docker Desktop on the owner's PC; `scripts/build-examples.sh`
   builds switch-examples 669786898205b7beb25ff1731e72982e6d0397d3 in
   `devkitpro/devkita64:20260219` (pinned by digest): libnx 4.12.0,
   devkitA64 r29.2, switch-tools 1.13.1.
2. Done: display services: vi:u, the layer, the binder relay and the
   buffer queue, with a vsync clock in guest time (buffers are released
   at 60 Hz, and the scheduler wakes for it), and am's
   CreateManagedDisplayLayer.
3. Done: nvdrv and nvmap, with CloneCurrentObjectEx and
   QueryPointerBufferSize.
4. Done: frames: block-linear to linear (envytools for the blocks, Mesa's
   `nil/tiling.rs` for the Tegra GOB, not libnx's code), colour formats,
   a `Screen` sink like `input::Source`; the window shows the frames.
5. Done: `hbdisplay`, drawing three frames of a known pattern; the
   self-test adds a CRC-32 line per frame shown, and the model renders
   the pattern straight to RGB.
6. Done: the examples run, with the fixes below; `kestrel run` gained
   `--press <button>@<seconds>` and `--save-frame <file.png>`;
   `scripts/run-examples.sh` runs them and `check-examples.py` reads each
   last frame back as text with libnx's font (4.12.0) and checks it
   against what the example's source prints (barrier against the
   barrier's rules). 4/4 pass; breaking the block-linear reading fails
   all four.
7. Done: `ci.yml` builds the examples in a job of its own and every
   native job runs and checks them: 4/4 on all six on `v0.1.17`.
   (`v0.1.16` failed to build a Unix-only test, which Windows never
   compiles; the checks now include a Linux type-check.)

What libnx 4.12's startup and the examples needed (each found by
running them):
- QueryPointerBufferSize on every session; sm: refusing vi:m and vi:s
  with NotAllowed.
- svcSetMemoryPermission on any FlagCanReprotect memory: libnx makes its
  relocated read-only data (RELRO, in the data segment) read-only and
  panics otherwise. Its svcSetMemoryAttribute PermissionLocked call gets
  InvalidCombination, as Switchbrew documents; libnx ignores it.
- svcGetInfo AliasRegionExtraSize (28), 0.
- OpenDirectory's filter as a u32, as libnx sends it.
- A loader return address in LR: a page with `svc #0x7`, so returning
  from main ends the process instead of jumping to 0.
- argv[0] as the program's place on the card, and the program's file
  shown there read-only when it lives elsewhere (the owner's choice,
  below); libnx starts in that folder.
- Repeated slashes in paths count as one: libnx's working directory ends
  in '/' and relative paths are joined with another.

The program's place on the card (the owner's decision, 2026-10-30): a
game library can be anywhere on the host, kestrel elsewhere, as with
Dolphin. Files kestrel only reads come from where they are; what a
program writes goes to kestrel's `user` folder. A program inside the
card's folder is at its real place; one elsewhere is told it is at
`sdmc:/switch/<name>` and shows there read-only, with `/switch`
(`fs::Program`); writing to it is UnsupportedWriteForReadOnlyFile(System);
creating files beside it makes `/switch` in the card's folder; a real
file at that path wins. A setting naming the library folder, to pick
games from a list, comes with the frontend.

A finding then: kestrel's clock counted one tick per instruction, so
programs saw a CPU about 50 times slower than a Switch's in guest time.
libnx's console took about 90 ms of guest time a frame and 0.6 s for a
frame that clears the screen, so read-controls missed a 100 ms press;
scripted presses now last 1 s. The owner decided the clock on
2026-10-30 (`PLAN.md` section 3): 52 instructions a tick for headless
runs, the host's clock with a window. A frame now takes a few
milliseconds of guest time, so read-controls showed A released and then
redrew its empty key list before a Plus at 3.5 s, as a console would;
`run-examples.sh` now presses Plus at 2.5 s, while A is still held.

How the display works (`hle/display.rs`, `hle/service/vi.rs`,
`hle/service/nv.rs`):
- nvmap objects record the program's memory (ALLOC's address); nothing
  is copied. A buffer given to the queue (SET_PREALLOCATED_BUFFER) names
  its nvmap object, offset and surface (size, Android pixel format,
  layout, pitch, block height), read from the GraphicBuffer's ints.
- QUEUE_BUFFER reads the buffer from the program's memory, converts it
  to 0x00RRGGBB pixels and gives the frame to `Process::screen` at once;
  the buffer is freed at the next vsync of guest time. The buffer-free
  event (GetNativeHandle 0x0f) is signalled while a buffer is free. The
  scheduler's idle jump also stops at the next vsync with a queued
  buffer (`vi::State::next_release`).
- The window's Screen keeps the newest frame and asks the window to
  repaint; the self-test's records a CRC-32 per frame.

| Choice (kestrel's) | Value |
|--------------------|-------|
| Pointer buffer size (QueryPointerBufferSize) | 0x8000 on every session, so libnx sends parcels and ioctls as pointers |
| sm: NotAllowed | vi:m and vi:s, as for an application; other unknown services still stop |
| IDs | display "Default" 1, layer 1, binder 1; nvdrv descriptors and nvmap handles count from 1; an nvmap ID is its handle |
| Display resolution | 1920x1080 (the docked console; "VI services" gives 1080p for "Default") |
| Queue size before buffers | 1280x720 |
| nvdrv Initialize | the transfer memory is checked, not kept or used |
| DEQUEUE_BUFFER | the free buffer freed longest ago, an NvMultiFence with no fences; none free: WOULD_BLOCK (-11) |
| Showing a frame | at QUEUE_BUFFER; the buffer is freed at the next vsync, 60 Hz of the 19.2 MHz clock (320,000 ticks) |
| RGB 565 | each channel's top bits repeated below it, so full intensity is 0xff |
| Window | the frame as large as fits with its shape kept, centred, sharp pixels |

Gaps:
- Pixel formats RGBA 8888, RGBX 8888, BGRA 8888 and RGB 565 only; others
  stop when a buffer is set.
- No GPU: only /dev/nvmap takes ioctls; /dev/nvhost-ctrl opens and stops
  on any ioctl, and no fences exist. Phase 5.
- Not yet: GetDisplayVsyncEvent, CreateStrayLayer, the QUERY transaction,
  other displays, the swap interval (every queued buffer is freed at the
  next vsync), layer position and size.

Phase 3 on macOS (needs a session on the owner's Mac, since
Hypervisor.framework is macOS-only): the native backend and the
differential tester; the memory arena is done.

### Order after that

1. The Windows x86_64 milestone (the two tracks above, then phase 6 on
   the owner's Windows machines).
2. Then the other platforms, in the owner's order: macOS ARM, Linux
   x86_64, Windows ARM, macOS x86_64, Linux ARM; ARM hosts with phase 3's
   in-process fallback first.

Owner decisions pending in `PLAN.md` section 7 (strategy, API-level
graphics, code reuse) do not block any of the above; ask before phase 5.