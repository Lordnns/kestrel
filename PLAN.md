# kestrel: development plan for Claude Code

This document is the plan of record for kestrel, a clean-room emulator for the
Switch 2 generation of hybrid consoles. Read it at the start of every session.
It describes what to build, in what order, under which rules, and how to know
each step is finished. It deliberately contains no code: the repository is the
source of truth for how things are currently implemented.

---

## 0. Starting a session

The initial audit is done (see `docs/ROADMAP.md`, audit of 2026-10-29).
At the start of every session:

1. Read this plan, then the status and "Next task" sections of
   `docs/ROADMAP.md`. ROADMAP tracks progress; this plan holds the rules and
   the order of work. If they disagree, this plan wins and the ROADMAP is
   corrected.
2. Check whether the repository changed outside your last session (commits
   by the owner). Re-audit only what changed, and record it in the ROADMAP.
3. Report any conflict between the code and this plan to the owner instead
   of silently picking one. The owner's changes win unless they break a hard
   rule in section 2.

Then continue with the ROADMAP's "Next task", or the owner's instruction if
it differs.

When this plan changes, the ROADMAP gets a dated "Plan changes" entry saying
what changed and which tasks it affects.

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
- Emulate graphics at the highest level that works: intercept the graphics
  API games use where that is possible, and emulate the GPU's command stream
  where it is not (phase 5).
- Ship self-testing packages so any tester can verify their machine with a
  single command.
- Behave like a real Switch 2 in real time (owner, 2026-10-30): guest time
  runs 1:1 with wall-clock time, and games run at their full frame rate,
  up to 120 fps, on the native (phase 3) and recompiler (phase 4)
  backends. Weigh every design choice against this goal; the interpreter
  remains the reference for correctness, not for speed. Section 3, "Time
  and cores", holds the decisions that follow from it.

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
  the repository, CI, test fixtures, issues, logs or release packages. Never
  help find, download or use games, keys or firmware from download sites,
  including "for testing only".
- No leaked material of any kind: no leaked SDK headers, NVN graphics API
  headers or documentation, leaked source code, or code from private or
  unreleased forks of other projects. Do not read them, even for reference.
- Code from other open-source emulators may be reused only with the owner's
  approval, with a compatible license (MIT, BSD, Apache 2.0, zlib; GPL code
  cannot enter this MIT project), with attribution in the file and in
  `docs/THIRD_PARTY.md` (created with the first reuse), and only from the
  project's own public releases.
- Behaviour learned by observing software (rather than from public
  documentation) is first written down as a behaviour description in
  `docs/SWITCH2_NOTES.md` (what happens, how it was observed, on what), and
  implemented from that description. This keeps the observation step and the
  implementation step separate, which is the core of a clean-room process.

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

### Time and cores (owner decisions, 2026-10-30)

These follow from the real-time goal in section 1.

- **The clock.** The guest's 19.2 MHz system counter (GetSystemTick,
  CNTPCT_EL0) has two sources:
  - Interactive `kestrel run` (a window) reads the host's monotonic clock,
    so guest time is 1:1 with real time. A game that waits for vsync or
    sleeps 16 ms waits that long, and a game measuring its frame time sees
    the real one. Runs are not repeatable, and need not be.
  - The self-test, unattended (headless) runs and the differential tester
    count time: one tick every 52 guest instructions, the Switch 2's CPU
    rate of about 1 GHz (52 x 19.2 MHz = 998.4 MHz), and when every thread
    sleeps the clock jumps to the next wake-up. Results repeat exactly on
    every host and backend, so expected output can be checked, and timing
    inside a test resembles a console's.
  - Reason: tests need reproducible time and players need real time; one
    clock cannot give both. Counting one tick per instruction (the
    phase 2 clock) made programs see a CPU about 50 times slower than a
    Switch 2 in guest time.
  - On the native backend, guest reads of the counter would read the
    host's, whose rate differs per host. kestrel owns the VM's EL1, so it
    can trap EL0 counter reads (CNTKCTL_EL1) and answer from its own clock;
    phase 3 measures whether that is fast enough.
- **Cores.** Switch 2 games get six CPU cores. Plan for six guest cores,
  each run on its own host thread by the native and recompiler backends,
  so a game's threads run in parallel as on the console. That needs guest
  memory and the exclusive monitor shared between host threads (the
  phase 3 arena), host atomics for guest atomics, and an OS layer that is
  safe to call from several host threads. The interpreter keeps its
  deterministic scheduler, all cores taking turns on one host thread, for
  the self-test and the differential tester. The guest-visible core count
  moves from the current four (Switchbrew's ProcessorId 0 to 3) to six
  once the Switch 2's numbering is documented or observed (recorded in
  `docs/SWITCH2_NOTES.md`).

---

## 4. Phases

Each phase lists its goal, tasks, provisions (decisions already made) and
acceptance criteria. Finish a phase's acceptance criteria before starting the
next, unless the owner says otherwise or a phase's provisions allow an earlier
start (phase 3 does).

### Phase 0: Foundation
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
  mappable into the VM, and by every host thread running a guest core.
- Real time (section 3, "Time and cores"): one host thread per guest core,
  six for Switch 2 games; guest counter reads answered from kestrel's
  clock.
- Order: macOS first (owner's development Mac), then Linux KVM, then Windows.
- Priority (2026-10-29): native execution on ARM is the approach the first
  public Switch 2 demos use (section 6), so it is not deferred until phase 2
  is complete. Start phase 3 on macOS as soon as phase 2's threads and
  synchronisation are done, and run it alongside phase 2's IPC and services.
  The differential tester keeps the two tracks honest.

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
the phase 1 NaN rules applied; one host thread per guest core, as in phase 3
(section 3, "Time and cores"). Static whole-program recompilation is a later
option.

Acceptance: the self-test passes on the recompiler on Linux, Windows and
macOS Intel; zero divergences against the interpreter.

### Phase 5: Graphics
Goal: homebrew that draws, then 3D, then a graphics path for commercial games.

Graphics can be emulated at two levels, and kestrel needs both:

- **GPU level:** emulate the GPU driver services, the command stream and the
  shader instruction set, then render with Vulkan. Works for any software,
  including homebrew, which talks to the GPU through open-source libraries.
  Most accurate, most work.
- **API level:** commercial games draw through Nintendo's NVN graphics API,
  which is part of the game's own code. Recognising those calls and
  translating them straight to Vulkan or Metal skips the command stream and
  much of the GPU emulation. This is how the first Switch 2 demos on Apple
  hardware render (section 6). Much less work per game, but it only applies
  to commercial software, and the API itself is not publicly documented.

Provisions:
- The Switch 2 GPU is NVIDIA Ampere-based; Switch 1 and its homebrew target
  Maxwell. Public references: NVIDIA open-gpu-doc class definitions, Mesa NVK
  (command submission), Mesa NAK (shader encodings), and the open-source
  homebrew graphics library deko3d, which documents by example how Maxwell is
  driven. Everything learned by observation goes to `docs/SWITCH2_NOTES.md`.
- Render with Vulkan everywhere (MoltenVK on macOS). A native Metal backend
  is an option later if MoltenVK limits performance.
- One shader intermediate representation, with a Maxwell decoder first and
  an Ampere decoder second, both producing SPIR-V. Ampere uses a different
  instruction encoding from Maxwell; it is a new decoder, not a table of
  renamed instructions.
- API-level work follows the clean-room note-taking rule in section 2
  strictly: the NVN API is known only from observation of software the owner
  legally has, never from leaked headers or SDK documentation. Owner approval
  is required before 5D starts (section 7).

5A. Display and framebuffer
- GPU driver services and their control calls, GPU address space, display
  services and buffer queue, presentation to a window on all five platforms.
- Acceptance: homebrew framebuffer and 2D examples display correctly.

5B. GPU level for homebrew (Maxwell)
- Command stream processing for the 3D, compute and copy engines; Maxwell
  shader decoder to SPIR-V; texture formats and layouts; render targets;
  caches.
- Acceptance: the deko3d examples built from source render correctly.

5C. GPU level for Switch 2 (Ampere)
- Ampere engine classes and the Ampere shader decoder into the same
  representation; Switch 2 texture formats.
- Acceptance: test software the owner has locally renders its first frames.

5D. API level for commercial games
- Identify how a game's graphics calls can be intercepted, document the
  observed behaviour of each call, and translate the used subset to Vulkan.
- Acceptance: one game the owner legally has reaches its menu through the
  API path, on at least one platform.

### Phase 6: Commercial software (local testing only)
Goal: the owner's own legally dumped games boot, and progress is tracked.

Provisions: loaders accept only executables and file trees the owner has
already extracted on their own hardware; kestrel contains no decryption and no
keys; game files never leave the owner's machines; testing happens on the
three physical machines in section 3.

Tasks: a loader for extracted executables (compressed segments, module
headers, relocations) and game data from a host folder; the graphics path
from 5C or 5D, whichever gets a title further first; the remaining services
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

---

## 6. External landscape (as of 2026-10-29)

Public facts that shaped this plan. Re-check them before relying on them;
this field moves weekly.

- The first Switch 2 jailbreak became public in late October 2026, so dumps
  of Switch 2 software now exist. Expect stronger legal action from Nintendo
  against emulator projects in the following months. Section 2 matters more,
  not less.
- A private fork of nxnative (an open-source compatibility layer that runs
  Switch software natively on Apple Silicon, with its own stand-in for OS
  services, NVN graphics translated to Metal, and shader translation reused
  from Ryujinx) showed a Switch 2 game running on macOS and iPhone on
  27-28 October 2026, with visible rendering errors. The fork's author says it
  will not be released. As of 26 October, no commercial Switch 2 game ran
  under any public emulator.
- On x86 hosts nothing comparable exists yet; that needs a recompiler
  (phase 4).
- Other Switch 2 projects (oboromi, Pound) are early foundations; Pound's
  developers estimate a decade to a functional emulator.
- A Switch 1 emulator (Eden, GPL) could in principle be extended for Switch 2,
  since the OS and CPU architecture carry over; unverified claims of such a
  patch circulate. See section 7.

What this means for kestrel: the strategy in this plan (native ARM
execution, OS emulated at a high level, graphics at the highest level that
works) is the one the first working demos use. The ordering in phases 3 and 5
reflects that.

---

## 7. Open decisions for the owner

Claude Code must not decide these. Ask, then record the answer here with the
date.

1. **Strategy.** Keep building kestrel from scratch, or contribute Switch 2
   support to an existing Switch 1 emulator? Contributing is faster but means
   GPL code, a project already under DMCA pressure, and less control. kestrel
   keeps its value either way as a clean-room, research-friendly project
   (native ARM execution, cross-platform benchmarking).
2. **API-level graphics (5D).** Approve or decline working at the NVN level,
   given that the API can only be learned from observing commercial software
   under the clean-room process in section 2.
3. **Code reuse.** Approve or decline reusing MIT-licensed code from other
   emulators (for example a shader translator), with attribution, instead of
   writing it from scratch.