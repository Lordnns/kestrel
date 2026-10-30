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
| Memory SVCs | done, with the gaps below | `hle/memory.rs`: memory types per mapping, svcQueryMemory, svcSetHeapSize in place with its checks, svcSetMemoryPermission, svcSetMemoryAttribute, svcMapMemory/svcUnmapMemory, svcMapPhysicalMemory/svcUnmapPhysicalMemory; `Memory` range operations in `kestrel-core`; self-test program `hbmemory` |
| Handle table and kernel objects | done, with the gaps below | `hle/kernel.rs`: per-process handle table, objects alive while a handle names them; svcCloseHandle, svcGetThreadId, svcGetProcessId, svcCreateEvent, svcSignalEvent, svcClearEvent, svcResetSignal; svcCreateTransferMemory in `hle/memory.rs`; the main thread has a real handle; self-test program `hbhandles` |
| Threads | done, with the gaps below | `hle/thread.rs`: thread records, a deterministic scheduler over four virtual cores with slices of 19,200 instructions, a process-wide 19.2 MHz clock, thread local regions; svcCreateThread, svcStartThread, svcExitThread, svcSleepThread (sleep and yield), svcGet/SetThreadPriority, svcGet/SetThreadCoreMask, svcGetCurrentProcessorNumber, svcGetInfo CoreMask and PriorityMask; self-test program `hbthreads` |
| Synchronisation | done, with the gaps below | `hle/sync.rs`: a waiting state with deadlines on the process clock; svcWaitSynchronization, svcCancelSynchronization, svcArbitrateLock/Unlock, svcWaitProcessWideKeyAtomic, svcSignalProcessWideKey, svcWaitForAddress, svcSignalToAddress; deadlock report; self-test program `hbsync` |
| IPC | done, with the gaps below | `hle/ipc.rs`: the message format (header, special header, handles, X/A/B/W descriptors, raw data, receive list) and CMIF and TIPC replies; `hle/service.rs`: client sessions as kernel objects, svcConnectToNamedPort, svcSendSyncRequest, the service manager sm: (RegisterClient, GetServiceHandle over CMIF and TIPC); unknown ports, services and commands stop by name and number; self-test program `hbipc` |
| First services | in progress | system version 20.0.0 (`hle::firmware`) in the HosVersion loader entry; `hle/service/`: CMIF domains, copied handles, receive (C) buffers; set:sys GetFirmwareVersion2; apm; appletOE with the commands libnx's applet startup sends; self-test program `hbapplet`. Not yet: hid, time, fsp-srv, power state, logging |
| SD folder option, unimplemented-call log | not started | unknown SVCs, ports, services and commands stop with their name and number |
| `docs/SWITCH2_NOTES.md` | done | purpose, entry format, the first public-source entries (Switchbrew [S2] marks); no observations yet |

### Phase 3: Native execution on ARM hosts: **not started**

Starts on macOS once phase 2's threads and synchronisation are done
(`PLAN.md` phase 3 priority).

| Task | Status | Evidence |
|------|--------|----------|
| Memory arena refactor | not started | `Memory` is a `BTreeMap` of `Vec<u8>` regions |
| Native backend | not started | `create_backend(Native)` returns "not implemented yet" |
| Differential tester | not started | none |
| Self-modifying code, cache maintenance | not started | `IC IVAU` is a no-op; `CpuBackend::invalidate` is never called |
| Per-backend speed in self-test | partial | self-test prints MIPS for the one backend it runs |

### Phases 4 to 7: **not started**

- Phase 4 x86_64 recompiler: `create_backend(Jit)` returns "not implemented yet".
- Phase 5 graphics (5A display, 5B Maxwell GPU level, 5C Ampere GPU level,
  5D NVN API level), phase 6 commercial software, phase 7 polish: no code.

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
| 1 | MainThreadHandle | a real handle to the main thread |
| 7 | AppletType | Application (0), no flags |
| 5 | Argv | argv[0] is the file name, then the arguments; empty arguments and arguments with spaces are double-quoted |
| 14 | RandomSeed | a fixed value, so runs are reproducible |
| 16 | HosVersion | 20.0.0 in libnx's MAKEHOSVERSION encoding; 0 as the implementation magic, so libnx does not turn on its Atmosphere extensions |
| 0 | EndOfList | the loader string `kestrel <version>` |

Gaps:

- Not provided yet: SyscallAvailableHint, OverrideHeap and the other
  optional keys.
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
- Shared memory has no object yet. A program cannot create one itself
  (svcCreateSharedMemory is for system modules), so it arrives with the
  first service that hands one out (input), together with
  svcMapSharedMemory and svcUnmapSharedMemory.
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
  after a slice of 19,200 instructions (1 ms of guest time), so a thread
  spinning on one core cannot starve the others.
- One clock for the process: an instruction is one tick of the 19.2 MHz
  counter. When every thread sleeps, the clock jumps to the next wake-up
  without counting instructions.
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
- svcGetSystemTick and CNTVCT_EL0 read the process clock; reading
  CNTPCT_EL0 directly, as official software does, is not supported yet.
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
  ConvertCurrentObjectToDomain (cloning, QueryPointerBufferSize, and any
  on a domain); domain objects as arguments; legacy command types 1 and 3;
  moved handles in requests; receive list modes 1 and 2 and explicit
  receive list offsets.
- Services write output through receive list (C) buffers; none reads or
  writes through the A, B, W or X descriptors yet.
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

## Next task

Two tracks now run in parallel (`PLAN.md` phase 3 priority).

Phase 2, the rest of the first services (on any machine), each from
its Switchbrew page, in the order libnx's startup opens them, and each
checked by a homebrew test program:

1. `hid` (input): CreateAppletResource and the shared memory it hands
   out, which needs shared memory objects and svcMapSharedMemory, then
   the npad setup libnx's hidInitialize sends.
2. Time (`time:u` or the service libnx picks for 20.0.0): the system
   clocks and the time zone service libnx's startup reads, with a
   documented choice of what time it is (a fixed time keeps runs
   reproducible).
3. `fsp-srv` with the SD folder option: OpenSdCardFileSystem, files and
   directories over a host folder, through A and B buffers.
4. Power state and logging, then the unimplemented-call log.

Phase 3 on macOS (needs a session on the owner's Mac, since
Hypervisor.framework is macOS-only): the memory arena refactor, the
native backend, and the differential tester.

### Order after that

1. Phase 2 acceptance (homebrew console examples on all five platforms),
   then phase 3 on Linux KVM and Windows.

Owner decisions pending in `PLAN.md` section 7 (strategy, API-level
graphics, code reuse) do not block any of the above; ask before phase 5.