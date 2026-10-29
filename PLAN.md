# kestrel: development plan for Claude Code

This document is the plan of record for kestrel, a clean-room emulator for the
Switch 2 generation of hybrid consoles. Read it at the start of every session.
It describes what to build, in what order, under which rules, and how to know
each step is finished. It deliberately contains no code: the repository is the
source of truth for how things are currently implemented.

---

## 0. First session: audit before building

The repository has been modified by hand since this plan was written. Before
starting any phase:

1. Read the whole repository: crate layout, CPU backends, loaders, OS
   emulation, test suite, CI workflows, scripts and docs.
2. Build it, run the full test suite, the linter and the built-in self-test,
   and record what passes.
3. Map what exists onto the phases below. Write the result into a status
   section at the top of `docs/ROADMAP.md` (create it if missing): for every
   task, mark done, partial or not started, with one line of evidence.
4. Report any conflict between the code and this plan to the owner instead
   of silently picking one. The owner's changes win unless they break a hard
   rule in section 2.

Then continue with the first unfinished task of the earliest unfinished phase.

---

## 1. Product goals

- Run on five host platforms with one codebase: Linux x86_64, Linux ARM64,
  Windows x86_64, Windows ARM64, macOS (Apple Silicon and Intel).
- Use the AnyPS5-style strategy where the hardware allows it: on ARM hosts,
  guest code runs directly on the host CPU; on x86 hosts it is recompiled.
  A portable interpreter is always available and is the reference for
  correctness.
- Emulate the console OS at a high level (system calls and services written
  in Rust), never by running console firmware.
- Ship self-testing packages so any tester can verify their machine with a
  single command.

---

## 2. Hard rules

### Legal and clean-room
- Never add, link, download or explain how to obtain console keys, firmware,
  system files, certificates, game dumps, or leaked or NDA SDK material.
- Never write decryption or key-extraction code. Loaders accept only plain
  formats the user has already prepared on their own hardware.
- Reverse engineering for interoperability is allowed and expected: observe
  behaviour, read public documentation (Switchbrew, the Arm Architecture
  Reference Manual, Mesa NVK and NAK, NVIDIA open-gpu-doc), study open-source
  homebrew, then implement in our own words and structure. Never paste or
  transliterate decompiled console code.
- Cite the public source of any non-obvious behaviour in a comment next to the
  implementation.
- Commercial game files are used only locally by the owner. They never enter
  the repository, CI, test fixtures, issues, logs or release packages.

### Correctness
- Every instruction the CPU supports has a unit test whose encoding comes from
  a real assembler, never hand-assembled.
- Every expected value comes from an independent source: Arm pseudocode worked
  by hand, a Python model, or real ARM hardware. Never copy kestrel's own
  output into an expected result.
- Never weaken, skip or delete a failing test to get a green build. If an
  expected value is wrong, prove it independently and explain it in the commit.
- Guest-visible behaviour must be identical on every host. Floating point
  follows Arm rules (NaN propagation, default NaN, flush-to-zero, rounding
  modes), not the host's.
- Unsupported instructions, system calls, services and commands must stop
  with an explicit message naming them. Never guess or silently ignore.
- Every fast backend is differentially tested against the interpreter.

### Portability
- Core crates use the standard library only. A new dependency needs a clear
  reason, written in the commit message.
- Host-specific code lives in its own module behind platform conditions, with
  a fallback, so all five targets always build.
- File paths, line endings and process handling must work on Windows.

### Definition of done for every change
1. Formatting, lint (warnings as errors) and the full test suite pass.
2. The built-in self-test passes. New test programs join it with expected
   output from an independent source.
3. `docs/ROADMAP.md` status and `docs/ARCHITECTURE.md` are updated if affected.
4. One logical change per commit, with a message that says what and why.

---

## 3. Platform and test provisions

| Host target      | CI builds and tests on            | Owner's gameplay test machine        |
|------------------|-----------------------------------|--------------------------------------|
| Linux x86_64     | GitHub hosted Ubuntu x86          | x86 PC, dual-boot                    |
| Linux ARM64      | GitHub hosted Ubuntu ARM          | Apple Silicon M1/M2 with Asahi Linux |
| Windows x86_64   | GitHub hosted Windows x86         | x86 PC, dual-boot                    |
| Windows ARM64    | GitHub hosted Windows 11 ARM      | Snapdragon X laptop                  |
| macOS            | GitHub hosted macOS ARM and Intel | Apple Silicon Mac                    |

- CI runs the full test suite and self-test natively on all five targets.
  Owner decision (2026-10-29): CI runs only as the first stage of the release
  workflow (version tags) or when started by hand, not on every push. Keep a
  cross-compile plus emulator job as a fallback for Linux ARM.
- The release workflow builds one package per target (macOS as a single
  universal, ad-hoc-signed binary), self-tests each on its own OS and CPU,
  and publishes them on version tags. Each package includes tester
  instructions and a way to produce a report file.
- Hosted runners have no usable GPU and never see game files, so gameplay
  testing is manual on the owner's machines. Gameplay hardware is bought only
  when phase 5 starts.

---

## 4. Phases

Each phase lists its goal, tasks, provisions (decisions already made) and
acceptance criteria. Finish a phase's acceptance criteria before starting the
next, unless the owner says otherwise.

### Phase 0: Foundation (expected done; confirm in the audit)
- Workspace split into memory, CPU, loader, OS emulation, test suite and
  command-line crates.
- Portable interpreter covering integer instructions, branches, all load and
  store forms, exclusives, atomics, user system registers and basic SIMD
  register moves.
- ELF test-program loader, minimal process layout, first system calls
  (heap, exit, sleep, tick, break, debug output).
- Embedded test suite with independently verified expected output, a
  self-test command, CI on all platforms, release packaging.

### Phase 1: Complete CPU
Goal: any ARM64 code a compiler emits runs correctly in the interpreter.

1A. Scalar floating point
- Single and double precision arithmetic, fused multiply-add family, min and
  max (including the NaN-ignoring variants and signed-zero rules), absolute,
  negate, square root, precision conversion, all round-to-integral variants,
  compares and conditional compares and selects, immediate moves, conversions
  between floating point and signed or unsigned integers in every rounding
  mode, fixed-point conversions, reciprocal and reciprocal-square-root
  estimates and steps.
- Provisions: process NaNs first, then compute on the host, then replace any
  host-generated NaN with the Arm default NaN, so x86 and ARM hosts give
  bit-identical results. Honour the default-NaN and flush-to-zero control bits.
  Estimates follow the architected algorithms exactly. Half precision raises an
  explicit unsupported error for now. Exception status flags are not tracked
  yet; document that.

1B. NEON (Advanced SIMD)
- Vector integer: add, subtract, multiply and accumulate, all compares,
  min/max, saturating add/subtract, halving and absolute-difference ops,
  register shifts.
- Vector floating point: arithmetic, fused multiply-accumulate, compares,
  min/max, pairwise forms.
- Two-register operations: byte reversals, bit counts, logical not and bit
  reverse, absolute and negate, compares against zero, pairwise widening adds,
  narrowing and widening, floating-point unary ops and conversions.
- Shifts by immediate, including accumulate, rounding, insert, narrowing,
  widening and fixed-point conversion forms.
- Widening "long" and "wide" arithmetic, by-element multiplies, across-lane
  reductions, permutes (zip, unzip, transpose, extract, table lookup),
  element copies, and all scalar SIMD forms.
- Structure loads and stores: one to four registers, single lane, replicate,
  with immediate and register post-increment.
- Crypto extensions only when a real program needs them.
- Provision: a single set of lane helpers serves all element sizes; operating
  on the 64-bit form clears the top half of the register.

1C. Phase 1 tests
- Unit tests for every instruction class.
- Build every existing C test program a second time with NEON enabled, sharing
  the same expected output, so auto-vectorised code is covered.
- A floating-point test program that prints results as exact bit patterns,
  built with contraction and math-errno disabled so results are reproducible.
- A NEON intrinsics test program that exercises every class above.
- Edge cases in the hand-written assembly checks: NaNs, denormals,
  flush-to-zero, saturation.

1D. Real-hardware oracle
- A host build mode for the C test programs, so the same sources compile
  natively on ARM machines with ordinary output instead of console system
  calls.
- A CI job on the macOS ARM and Linux ARM runners that compiles and runs them
  natively and compares against the expected files, so a real ARM CPU confirms
  every expected answer.

Acceptance: every C test program, with and without NEON, passes the self-test
on all five targets; the hardware oracle passes on both ARM runners; an
aggressively optimised NEON build of the test programs hits no unsupported
instruction.

### Phase 2: Homebrew (OS emulation)
Goal: open-source homebrew console applications run on all five platforms.

Provisions:
- Homebrew uses the publicly documented Switch 1 system interface, which the
  Switch 2 keeps for compatibility. Record Switch 2 differences, each with its
  public source, in `docs/SWITCH2_NOTES.md`.
- The homebrew test corpus is built from source in CI using the public
  homebrew toolchain container. Check each example's license before using it.
  Do not commit binaries of unclear origin.
- Start with a cooperative scheduler on one host thread for determinism; add
  preemption by instruction budget, then true multi-core.

Tasks:
- Homebrew executable loader with segment, BSS and asset handling, plus the
  argument and environment setup homebrew expects.
- Process layout reporting (address space, heap, alias and stack regions,
  entropy, program identity).
- Memory system calls: permissions, attributes, mapping, unmapping, querying,
  physical-memory mapping.
- A handle table and kernel objects: threads, events, sessions, shared and
  transfer memory.
- Threads with per-thread storage, priorities, identifiers, sleep and yield.
- Synchronisation: waiting and cancelling, mutex arbitration, condition
  variables, address-based wait and signal, events.
- Inter-process communication with the standard message format (buffers,
  handles, domains) and named ports.
- First services, grown on demand: service manager, settings, filesystem (SD
  card mapped to a host folder), input (keyboard and host gamepad), minimal
  applet, time, power state, logging.
- Command-line options for the SD folder, and a log of every unimplemented
  service or command with its identifiers.

Acceptance: the standard homebrew console examples (hello world, input,
file I/O, threads) run on all five platforms with their output checked in CI;
every unimplemented call is reported by name and number.

### Phase 3: Native execution on ARM hosts
Goal: guest code runs directly on the CPU of macOS ARM, Linux ARM and
Windows ARM hosts, many times faster than the interpreter.

Provisions (decided):
- Run guest user code inside a minimal virtual machine using each OS's
  hypervisor: Hypervisor.framework on macOS (requires the hypervisor
  entitlement and signing), KVM on Linux, Windows Hypervisor Platform on
  Windows. System calls and faults exit to the host and go to the OS
  emulation layer.
- Reason: running guest code directly in the emulator process collides with
  host rules that differ per OS: macOS and Windows reserve register x18,
  thread-pointer registers belong to the host, and macOS enforces
  write-xor-execute on code memory. A VM avoids all three. In-process
  execution with load-time patching stays a documented fallback.
- Guest memory becomes one host-allocated arena shared by every backend,
  mappable into the VM.
- Order: macOS first (owner's development Mac), then Linux KVM, then Windows.

Tasks: memory arena refactor; the native backend (VM setup, user-level page
tables, exception forwarding, register synchronisation); a differential tester
that runs interpreter and native backend side by side and reports the first
divergence; self-modifying code and cache maintenance; per-backend speed in
the self-test output.

Acceptance: the self-test passes on the native backend on all three ARM
operating systems; the differential tester finds zero divergences.

### Phase 4: x86_64 recompiler
Goal: fast execution on x86 hosts.

Provisions: a dynamic recompiler that translates ARM64 blocks to x86-64,
caches them by guest address and invalidates them on code writes; an
intermediate representation between decoding and code generation so it can be
reused; lazy flags; memory through the phase 3 arena with guard pages; atomics
via host compare-and-swap; floating point through the host vector unit with
the phase 1 NaN rules applied. Static whole-program recompilation is a later
option.

Acceptance: the self-test passes on the recompiler on Linux, Windows and
macOS Intel; zero divergences against the interpreter.

### Phase 5: Graphics
Goal: homebrew that draws, then first 3D output.

Provisions: the Switch 2 GPU is NVIDIA Ampere-based. Use public references
(NVIDIA open-gpu-doc class definitions, Mesa NVK for command submission, Mesa
NAK for the shader instruction encoding) and record anything learned by
observation in `docs/SWITCH2_NOTES.md`. Render with Vulkan (MoltenVK on
macOS) and a cross-platform windowing library.

Tasks: GPU driver services and their control calls, GPU address space,
display services and buffer queue with presentation to a window, command
stream processing for the 3D, compute and copy engines, a shader translator
from the GPU's instruction set to SPIR-V, texture formats, render targets and
caches.

Acceptance: homebrew framebuffer and 2D examples display correctly on all
five platforms; a first 3D sample renders.

Open question for the owner: Switch 1 homebrew 3D targets the older Maxwell
GPU. Decide whether to support Maxwell as a stepping stone or rely on locally
dumped Switch 2 software for Ampere testing.

### Phase 6: Commercial software (local testing only)
Goal: the owner's own legally dumped games boot, and progress is tracked.

Provisions: loaders accept only executables and file trees the owner has
already extracted on their own hardware; kestrel contains no decryption and no
keys; game files never leave the owner's machines; testing happens on the
three physical machines in section 3.

Tasks: a loader for extracted executables (compressed segments, module
headers, relocations) and game data from a host folder; the remaining services
discovered by booting titles, in order of need; a compatibility tracker in
`docs/COMPATIBILITY.md` (title, platform, backend, status from boots to
playable, frame rate, first blocking issue).

Acceptance: at least one title reaches its menu on every platform.

### Phase 7: Product polish
Audio service and host audio output; input mapping, controllers and motion;
save data management; a graphical front end with game list, settings and
per-game configuration; shader and pipeline caches; a performance overlay.

---

## 5. Working agreement

- Keep changes small and reviewable; never mix a refactor with a feature.
- When a task is larger than one session, write the remaining steps into the
  roadmap status before stopping.
- Ask the owner before: adding a dependency, changing a provision in this
  plan, or making anything that touches legal boundaries.
- At the end of each session, summarise what changed, what was verified, and
  the next task.
