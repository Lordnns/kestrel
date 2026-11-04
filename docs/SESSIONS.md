# Sessions: who does what, and the prompt for each

> **Meta information, not instructions.** This file is a proposal kept
> for later: a possible split of the work and draft prompts. No session
> acts on it by itself. A session follows only the owner's real
> instructions: a task here starts only when the owner pastes its prompt
> or names it, and the owner may change or drop any of it. If this file
> disagrees with the owner, `PLAN.md` or the ROADMAP, they win.

Written 2026-11-03 with the re-plan (`PLAN.md` section 3: the boot track
first, then graphics, then CPU). Every feature left in the plan is here as
a prompt to paste into a new Claude Code session, marked:

- **CLOUD**: runs in a cloud session (Linux x86_64). Needs no game files
  and no Windows: tests use homebrew, fixtures built from source, real
  assemblers and compilers, and lavapipe for Vulkan.
- **LOCAL**: runs on the owner's Windows PC. Needs the owner's Switch 2
  title (its files never leave that machine), a real GPU, a gamepad, or
  the owner's eyes and ears.

The work is grouped in **lanes**. A lane is one line of work: its tasks
run one after another, each in its own session or in the same one, on
its own branch. Lanes run in parallel because each owns different folders.

## Rules for every session

Every prompt below starts with "Follow docs/SESSIONS.md, 'Rules for
every session'". They are:

1. Read `CLAUDE.md`, `PLAN.md` (sections 2 and 3 above all), the
   ROADMAP's "Next task", and your task below. `PLAN.md` wins on any
   conflict.
2. Work on a new branch from the newest `main`, named
   `claude/<task id>-<topic>` (for example `claude/a1-exefs-loader`). One
   task per branch, one pull request per task.
3. Change only the files your lane owns (table below). If you must touch
   a shared file, keep the change small and say so in the pull request.
   Shared files: `crates/kestrel-hle/src/service/mod.rs` (only to
   register a service), `crates/kestrel-hle/src/svc.rs`,
   `crates/kestrel-cli/src/main.rs` (only to add an option or command),
   `Cargo.toml` files, `.github/workflows/*`.
4. Docs: update your own ROADMAP section (the "Phase N notes" part your
   task belongs to) and your task's line in "Next task". Add facts to
   `docs/SWITCH2_NOTES.md` as new entries at the end of the right
   section; never reorder or rewrite others' entries. Update
   `docs/ARCHITECTURE.md` only for the parts you built.
5. Facts from other emulators: `PLAN.md` section 2, "Other emulators as
   fact sources". Read Eglantine, Eden, yuzu or Ryujinx for facts only,
   record each in `docs/SWITCH2_NOTES.md` with project, file, row or line
   and commit, and write kestrel's own code. Never copy or translate
   their code. Facts whose only source is Eglantine's "changes.patch"
   rows (`docs/EGLANTINE.md`) wait for a LOCAL observation. Mesa (MIT)
   and envytools (MIT) are public descriptions of NVIDIA hardware.
6. Expected values always come from an independent source (a real
   assembler or compiler, a Python model in `scripts/`, real hardware),
   never from kestrel's output. Each new behaviour gets a test, and the
   mutation check (one deliberate bug makes a test fail, then is undone).
7. Ask the owner before adding any dependency, tool in CI, or anything
   touching legal limits. Unknown calls keep stopping by name; only the
   exploration flag `--auto-stub` (task A3) answers them, and never in
   tests.
8. **CLOUD sessions never see game files.** If your task needs to know
   what the owner's title does, write down the question in your pull
   request and the ROADMAP; a LOCAL session answers it.
9. Before each commit: the checks in `CLAUDE.md` ("Checks before every
   commit"), starting with `rustup update stable`. Graphics and libnx
   changes also run `scripts/build-examples.sh` and `run-examples.sh`.
10. At the end: summarise what changed, what was verified and what is
    left, then ask the owner whether to push. Push only when told. Never
    start the next task unasked.

Cloud machine setup (once per session, as `CLAUDE.md` "Running things"
says): `apt-get install qemu-user gcc-aarch64-linux-gnu
libc6-dev-arm64-cross wine64 gcc-mingw-w64-x86-64 mesa-vulkan-drivers
libvulkan1 spirv-tools`, `rustup update stable`, `rustup target add
aarch64-unknown-linux-gnu x86_64-pc-windows-gnu`, the runner variables
from `CLAUDE.md`, and `sudo dockerd &` for the devkitPro container.

## Lanes and the files they own

| Lane | Where | Owns | Tasks, in order |
|------|-------|------|-----------------|
| A Boot foundations | CLOUD | `crates/kestrel-loader/`, `crates/kestrel-hle/src/{homebrew,memory,thread,kernel}.rs`, the `scan` command in `kestrel-cli` | A1 scan and ExeFS loader, A2 process (six cores, memory layout), A3 `--auto-stub` and boot traces |
| B CPU instructions | CLOUD | `crates/kestrel-cpu/src/interp/`, `crates/kestrel-cpu/src/float.rs`, `scripts/gen-int-vectors.py`, `scripts/gen-neon-vectors.py` | B1 LDAPR, pointer authentication, memory tagging; B2 what `kestrel scan` finds next; B3 self-modifying code; B4 counter reads from kestrel's clock |
| C Graphics | CLOUD | `crates/kestrel-hle/src/gpu/`, `crates/kestrel-cli/src/{render,vulkan}.rs`, `scripts/gen-gpu-methods.py`, `scripts/check-examples.py` | C1 deko_console and deko_examples 5 to 9; C2 the Switch 2 GPU front end; C3 SM86 shaders; C4 Switch 2 textures; C5 task and mesh shaders; C6 the GPU thread split and macro memo |
| D Recompiler | CLOUD | `crates/kestrel-cpu/src/jit/`, `scripts/gen-x64-vectors.py` | D1 integer NEON lanes (SSE2); D2 scalar floating point; D3 NEON floating point; D4 speed (computed jumps, registers, lazy flags); D5 one code cache for all cores; D6 `kestrel diff` on any program |
| E Switch 2 services | CLOUD | new files in `crates/kestrel-hle/src/service/` (one per service), `am.rs`, `fs.rs`, `nv.rs`, `nvgpu.rs`, `vi.rs`, homebrew test programs in `guest/src/nro/` | E1 applet, oe, filesystem and fonts; E2 the graphics system server (gsv), syncpt, nx2:gpu, vi2, nvsrv; E3 audio; E4 the video decoder sessions |
| L The owner's PC | LOCAL | `docs/SWITCH2_NOTES.md` observations, `docs/COMPATIBILITY.md`, the ROADMAP's "Boot log" | L1 Windows confirmations and merging; L2 the boot loop on the title; L3 the first frame; L4 measuring speed on the title; L5 controllers; L6 the frontend |
| P Polish and platforms | CLOUD, after the milestone or when the owner asks | as each task says | P1 save data; P2 shader and pipeline cache; P3 performance overlay; P4 licence notices; P5 to P7 the other platforms |

Where lanes meet: E2 and C2 both touch the GPU's channels and memory. E2
owns the services (`service/`), C2 the engines (`gpu/`); a change one
needs in the other's files goes in its pull request description, and the
owner merges in order.

## Order: what to start now

| Wave | Start when | Sessions |
|------|------------|----------|
| 1 | now | A1, B1, C1, D1 (CLOUD); L1 (LOCAL) |
| 2 | A1 merged | A2, A3 (CLOUD); L2 starts the title's boot (LOCAL) |
| 3 | L2's first blocker list | E1, E2 (CLOUD) from that list and Eglantine's facts; B2 from the scan's list |
| 4 | C1 merged | C2, then C3 and C4 (CLOUD) |
| 5 | the title submits GPU work | L3 (LOCAL) with C2 to C4; then C5, C6, L4 |
| later | as needed | B3, B4, D2 to D6, E3, E4, L5, L6, P1 to P7 |

At most four or five sessions at once keeps the pull requests mergeable;
the owner merges them through L1.

---

## Lane A: boot foundations (CLOUD)

### A1: `kestrel scan` and the ExeFS loader (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: A1, PLAN.md
phase 6A tasks 6A.1 and 6A.2 (branch claude/a1-exefs-loader).

Build the loader for a Switch 2 title's ExeFS that the owner extracted on
their own hardware (plain files only: kestrel never decrypts):
- NSO: header, the three segments with kestrel's own LZ4 block decoder
  (written from the public LZ4 block format description, std only), the
  segment SHA-256 hashes checked when the header asks (kestrel's own
  SHA-256, or the one already in the tree if there is one), .bss, the
  module name and the dynamic section left for rtld.
- main.npdm: META, ACI0 and ACID, including the Switch 2 ACID layout with
  a 0x300-byte signature area (Eglantine NX2_NOTES row 5; record it in
  SWITCH2_NOTES first). Parsed, never verified. Expose what the process
  needs: 64-bit flag, address space type, main thread priority and core,
  stack size, the thread info capability (cores, priorities), the allowed
  SVCs, the kernel version.
- The ExeFS folder: rtld, main, subsdk0..9, sdk placed one after another
  as the console does (Switchbrew "NSO", "Loader"); a plain RomFS from a
  sibling romfs/ folder, read where it lies.
- `kestrel scan <exefs folder>`: decode every word of each module's .text
  with kestrel's decoder and list, per module, the encodings it rejects
  (mnemonic class when known, the word, a count), most frequent first.
  No guest code runs.
- `kestrel run <exefs folder>` loads it and stops at the first
  unimplemented thing, named, as for homebrew (the process setup is A2;
  stop clearly where A2 must continue).
Tests and fixtures: NSOs built from source in the pinned devkitPro
container (elf2nso compresses with LZ4), compared with their ELFs'
segments; main.npdm from npdmtool (switch-tools) and, for the Switch 2
ACID, a fixture written by a Python script from the documented layout;
an ExeFS folder of homebrew NSOs for `kestrel scan` and `kestrel run`.
Never a title's files. Update the ROADMAP (phase 6 status and notes, 6A.1
and 6A.2 in "Next task") and ARCHITECTURE ("Loader").
```

### A2: the Switch 2 process: six cores and the memory layout (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: A2, PLAN.md
phase 6A task 6A.4 (branch claude/a2-switch2-process). Needs A1 merged.

Make an ExeFS process look like a Switch 2 process, from the NPDM A1
parses and the facts in docs/EGLANTINE.md (record each in SWITCH2_NOTES
first, with its NX2_NOTES row):
- Six guest cores: thread::CORES from 4 to 6, the NPDM's core mask
  (0x3f) and priorities (28 to 59) honoured by CreateThread and
  SetThreadCoreMask; the deterministic scheduler and run_threaded both
  with six. Expected outputs that print core numbers change: re-derive
  them in scripts/gen-expected.py, never from kestrel's output.
- The Switch 2 memory layout for applications (39-bit address space, the
  12 GB layout's total memory counting the alias region, rows 37 and 80),
  beside the homebrew layout, chosen by the program's kind.
- svcGetInfo 21 (total memory) for this layout, 29 and 30 (the region
  between alias and ASLR), 31 and 32 (remote memory: 0), 33 (the page
  granule, 0x1000); svc 0x80 SetGpuMemoryAttribute as far as public facts
  go, the attribute bits 5 and 6 (SWITCH2_NOTES already has the Switchbrew
  entries).
- The NPDM's allowed SVC list: a call outside it stops, named.
Homebrew keeps the layout it has today and its self-test output. Tests:
unit tests per call, and a homebrew program (guest/src/nro/) that checks
the six cores and the new svcGetInfo values with an independent model.
Update the ROADMAP (phase 3 "One host thread per guest core", phase 6
notes) and ARCHITECTURE.
```

### A3: `--auto-stub` and the boot traces (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: A3, PLAN.md
section 2's exploration flag and phase 6A task 6A.5's tooling (branch
claude/a3-auto-stub). Needs A1 merged.

Give the LOCAL boot loop (L2) the tools to find many blockers per run:
- `kestrel run --auto-stub`: an unknown service, command or ioctl answers
  an empty success instead of stopping, and is appended to
  user/logs/unimplemented.log with its identifiers and a count; an
  unknown SVC or svcGetInfo type still stops. Off by default; the
  self-test, CI and the examples never use it (a test checks that a
  stubbed call without the flag still stops).
- `--trace-svc` and `--trace-ipc`: one line per SVC (number, name,
  arguments, result) and per IPC request (service, interface, command,
  result), to a file under user/logs/.
- A summary at exit: the first stop, the stubbed calls by count, the
  services opened in order.
- Logs carry identifiers and results only, never memory contents, so
  nothing of a title's data is written.
Tests with homebrew that calls an unknown command. Update the ROADMAP
and README (options).
```

## Lane B: CPU instructions (CLOUD)

### B1: LDAPR, pointer authentication, memory tagging (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: B1, PLAN.md
phase 6A task 6A.3 (branch claude/b1-lrcpc-pauth).

Switch 2 titles use these heavily (docs/EGLANTINE.md, "Facts to take" 1:
100,000+ RETAA/RETAB and 37,000 to 100,000 LDAPR per title). In the
interpreter, decoded from the Arm Architecture Reference Manual:
- FEAT_LRCPC and LRCPC2: LDAPR, LDAPRB, LDAPRH; LDAPUR*/STLUR* forms.
- FEAT_PAuth: PACIA/PACIB/PACDA/PACDB and the Z forms, AUTIA/AUTIB/
  AUTDA/AUTDB and Z forms, XPACI/XPACD, PACGA, BRAA/BRAB/BRAAZ/BRABZ,
  BLRAA/BLRAB and Z forms, RETAA/RETAB, ERETAA is EL1 (not needed),
  LDRAA/LDRAB. kestrel's choice, written in a comment and the ROADMAP:
  the keys give the identity (PAC adds no bits, AUT always succeeds, XPAC
  leaves a canonical pointer unchanged), which is what software that
  never checks a forged pointer sees; check against the Cortex-A78C's
  documentation that FEAT_PAuth is present.
- FEAT_MTE instructions the titles contain (STG, and whatever else the
  scan names): check whether the Cortex-A78C has MTE at all; if not,
  they stay undefined and stop, named, and the ROADMAP says so.
- The recompiler falls back to the interpreter for each (Op::Interp).
Encodings from clang (--target=aarch64-none-elf with -march naming the
features), expected values from the Python model (gen-int-vectors.py),
per-instruction tests in int_vectors.rs, a guest program built with
-mbranch-protection=pac-ret+leaf at -O3 so compiled code exercises them.
Update the ROADMAP (phase 1 gaps, "Next task" CPU features) and README.
```

### B2: what the scan finds next (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: B2 (branch
claude/b2-scan-findings). Needs A1 and B1 merged, and L2's scan list in
the ROADMAP's "Boot log".

L2 recorded which encodings `kestrel scan` rejects in the owner's title
(counts and instruction classes only). Implement them in the interpreter,
most frequent first (Eglantine saw 60 to 110 rare SIMD forms in sdk: FP16
and v8.2 forms such as FMLAL/FMLSL), each with clang encodings, the
Python models, per-instruction tests and the recompiler's fallback. Stop
when the scan of the title (rerun by L2) lists nothing, or the rest is
not in the Cortex-A78C. Update the ROADMAP.
```

### B3: self-modifying code and cache maintenance (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: B3, PLAN.md
phase 3 "Self-modifying code, cache maintenance" (branch
claude/b3-smc). Coordinate with lane D: the invalidation hook lives in
kestrel-cpu's backend interface.

IC IVAU is a no-op and CpuBackend::invalidate is never called. Make code
writes visible to every backend: IC IVAU and IC IALLU invalidate the
range in every core's translated code (the recompiler's change count
already covers mapping changes; this covers writes to executable pages,
as rtld's relocations and a game's own code loaders do), and
svcMapProcessCodeMemory-style remaps do too. A guest test program that
writes code, cleans and invalidates, and runs it, on the interpreter and
the recompiler, single and threaded; the differential tester finds zero
divergences. Update the ROADMAP and ARCHITECTURE.
```

### B4: guest counter reads from kestrel's clock, measured (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: B4, PLAN.md
section 3 "The clock" and phase 3 "Guest counter reads" (branch
claude/b4-counter).

Eglantine's title reads CNTPCT about 17 million times a second while it
waits (EGLANTINE.md, row 127b), and on AMD hosts without CPUID leaf 0x15
the TSC rate had to be measured (row 111). Make kestrel's counter read
cheap on the recompiler with the host's clock: a TSC-based read scaled
to 19.2 MHz with its rate measured at start (Windows and Linux),
QueryPerformanceCounter or clock_gettime as the fallback, the counted
clock unchanged for tests. A micro-benchmark in `kestrel selftest
--stats` style, and a guest program that checks the counter is monotonic
and runs at 19.2 MHz within a tolerance on the host's clock. Update the
ROADMAP.
```

## Lane C: graphics (CLOUD)

### C1: deko_console and deko_examples 5 to 9 (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: C1, the
ROADMAP's phase 5B step 6 (branch claude/c1-deko-rest). Read the ROADMAP's
"5B: picking up the work" first.

deko_console (2D array textures, blending), then deko_examples 5 to 9
(tessellation, multisampling, sRGB, deferred shading with the tiled
cache, compute), each until the example draws correctly and is checked
by run-examples.sh / check-examples.py; render targets kept on the host's
GPU between draws. Tested with lavapipe; the owner confirms on Windows
(L1). Update the ROADMAP's phase 5 notes and the 5B status line.
```

### C2: the Switch 2 GPU front end (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: C2, PLAN.md
phase 5C, the command stream part (branch claude/c2-ampere-frontend).
Needs C1 merged.

From public descriptions (NVIDIA open-gpu-doc class headers for Ampere,
Mesa nvk and its MME code for Turing, MIT, envytools) and the facts in
docs/EGLANTINE.md (rows 31, 32, 49 to 53, 56, 58, 60 to 62, 70, 73, 84;
recorded in SWITCH2_NOTES first; changes.patch rows only after a LOCAL
observation):
- Ampere classes C797 (3D), C7C0 (compute), C7B5 (copy) and class 0
  bound to kestrel's Maxwell engines where their methods agree; methods
  that differ stop, named.
- Turing MME64 macros: their own interpreter (Mesa's mme_tu104
  description), one data RAM per channel, MME DMA reads into the load
  FIFO. Test vectors from a Python model written from Mesa's
  description, never from kestrel.
- QMD 3.0 compute launches and their release slots; max releases.
- A channel suspends on an unmet semaphore acquire and resumes at the
  same word; methods whose data continues across GPFIFO entries; 4-byte
  releases.
Fixtures: command lists written by Python scripts from the class
headers, and Vulkan-side checks with lavapipe. Design so C6 can split the
front end (command parsing and macros) from the back end (engine
methods) onto two threads. Update the ROADMAP (5C status, phase 5 notes)
and ARCHITECTURE.
```

### C3: SM86 shaders into the shader IR (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: C3, PLAN.md
phase 5C, shaders (branch claude/c3-sm86). Needs C2 merged (or at least
its shader binding path).

A decoder for Ampere (SM86) shader instructions into the IR the Maxwell
decoder feeds (crates/kestrel-hle/src/gpu/shader/), then SPIR-V as
today. Encodings from public descriptions: Mesa NAK's SM70+ encoder
(MIT), envytools. Fixtures: programs compiled from source by Mesa NAK
(graphics stages) and, if the owner approves the tool, NVIDIA's ptxas
for sm_86 (compute); never a title's shaders. Start with what
docs/EGLANTINE.md lists games use (rows 54, 63 to 67, 74, 88, 91 to 95:
uniform datapath ops, R2UR, LDSM, IMAD forms, MUFU.SIN/COS in turns,
CALL/RET forms, BRX jump tables, constant bank 24). Every SPIR-V module
passes spirv-val. Update the ROADMAP and ARCHITECTURE ("Shaders").
```

### C4: Switch 2 textures (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: C4, PLAN.md
phase 5C, textures (branch claude/c4-switch2-textures). Needs C2 merged.

Texture formats and layouts Switch 2 titles use: ASTC (decoded from the
Khronos ASTC specification, kestrel's own decoder) with the Switch 2 GOB
swizzle (EGLANTINE.md row 69; record first), recompression to BC1/BC3 on
the host's GPU when VRAM is short (rows 99, 103, 107), huge images
created sparse with no memory (rows 87, 94), 3D textures. Fixtures: ASTC
blocks from the reference encoder astcenc (Apache 2.0; ask before using
it in CI), layouts from Mesa's nil (MIT). Update the ROADMAP.
```

### C5: task and mesh shaders (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: C5 (branch
claude/c5-mesh). Needs C3 merged.

Titles draw high-detail geometry with task and mesh programs
(EGLANTINE.md rows 89, 97: SET_MESH_CONTROL, the ISBERD/ISBEWR stage
buffer, the output layout). Translate them to VK_EXT_mesh_shader, with a
clear stop when the host lacks it. Fixtures compiled from GLSL mesh
shaders by Mesa NAK. lavapipe supports mesh shaders: test there. Update
the ROADMAP.
```

### C6: the GPU thread split and the macro memo (CLOUD, measured LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: C6, PLAN.md
phase 5C's design for the GPU thread's cost (branch claude/c6-gpu-split).
Needs C2 merged.

Eglantine's measurements (NX2_GPU_THREAD_SPLIT.md, EGLANTINE.md "Where
Eglantine spends its time"): one GPU thread is the limit. In kestrel's
own design: a front thread (GPFIFO, command lists, macros, a mirror of
the registers macros read) and a back thread (engine methods, draws)
joined by an ordered single-producer ring; sync points where the front
must wait (macro DMA reads, unmet acquires, idle waits); macro calls
memoised on their parameters and the data RAM words they read; same-value
writes to state registers dropped. A verify mode that runs both and
compares register state. Tests: the deko examples and C2's fixtures give
the same frames split and unsplit. L4 measures it on the title. Update
the ROADMAP and ARCHITECTURE.
```

## Lane D: the recompiler (CLOUD)

### D1: integer NEON lanes with SSE2 (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: D1, the
ROADMAP's recompiler step 3 (branch claude/d1-sse2-lanes).

Integer NEON in the recompiler: 128-bit values in XMM registers (the IR
already holds a V register as two 64-bit halves), integer lane
arithmetic, compares, shifts, min/max, saturating forms with SSE2 (no
newer extension without a CPUID check and a fallback). Every new x86_64
encoding checked against llvm-mc or clang's integrated assembler
(gen-x64-vectors.py); the NEON vectors (gen-neon-vectors.py) run on the
recompiler in both calling conventions; `kestrel diff --backend jit`
finds zero divergences. Measure with the self-test and hello-world
(three or four alternating runs against main; CLAUDE.md "speed runs are
noisy"). Update the ROADMAP (phase 4 notes, step 3).
```

### D2: scalar floating point (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: D2 (branch
claude/d2-scalar-fp). Needs D1 merged.

Scalar floating point in the recompiler with SSE2, keeping Arm's rules
(PLAN.md phase 1A provisions: NaN propagation, default NaN, flush to zero,
rounding modes from FPCR): fast paths where the host's result is
bit-identical, a call to the interpreter's routine otherwise. FP vectors
and fp.c on the recompiler, zero divergences, speed measured as D1.
Update the ROADMAP.
```

### D3: NEON floating point (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: D3 (branch
claude/d3-neon-fp). Needs D2 merged.

NEON floating-point lanes in the recompiler, same rules as D2, then the
rest of NEON by frequency (count per encoding in jit::interp on the
self-test and the libnx examples). Zero divergences; speed measured.
Update the ROADMAP.
```

### D4: recompiler speed (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: D4, the
ROADMAP's recompiler step 4 (branch claude/d4-jit-speed).

Computed jumps (RET, BR, BLR) looked up in generated code (a return
address stack or a small table) instead of returning to the dispatcher;
guest registers kept in host registers inside a block; NZCV computed only
when read or at a block's exit. One change per commit, each measured
(alternating runs against main) and each keeping zero divergences.
Update the ROADMAP.
```

### D5: one code cache for all cores (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: D5 (branch
claude/d5-shared-cache). Needs B3 merged.

Each core translates its own copy of the code today. Share translated
blocks between the six cores' threads (read-mostly, safe under B3's
invalidation and the memory's change count), with a test of two threads
racing translation and invalidation. Measure memory and start-up time on
the self-test threaded. Update the ROADMAP and ARCHITECTURE.
```

### D6: `kestrel diff` on any program (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: D6, the
ROADMAP's recompiler step 6 (branch claude/d6-diff-any).

`kestrel diff --backend jit <program.nro or ExeFS folder>`: the
interpreter and the recompiler side by side on any program, on the
counted clock, reporting the first divergence. Run it on the libnx and
deko examples; L2 runs it on the title locally. Update the ROADMAP and
README.
```

## Lane E: Switch 2 services (CLOUD)

### E1: applet, oe, filesystem and fonts (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: E1, PLAN.md
phase 6A task 6A.5, first part (branch claude/e1-switch2-applet-fs).
Needs A1 merged; use L2's blocker list in the ROADMAP's "Boot log" when
there is one.

From the facts in docs/EGLANTINE.md (rows 9, 10, 12 to 15, 18, 36, 123,
127; each recorded in SWITCH2_NOTES first) and Switchbrew's [S2] marks:
appletOE command 10 (OpenApplicationProxy), IApplicationProxy 40 and 60
(the graphics scale interface), IApplicationFunctions 320 and 321,
GetPseudoDeviceId without a NACP, the process registered with fsp-srv
and its RomFS from A1's folder (OpenDataStorageByCurrentProcess and the
RomFS file system), pl:u 2000/2010 with a font file the user provides
(none shipped), chat:u, the camera context. Each service in its own file;
each command tested with IPC unit tests and a homebrew program in
guest/src/nro/ that sends it, with an independent model. Unknown
commands still stop by name. Update the ROADMAP and SWITCH2_NOTES.
```

### E2: the graphics system server and the GPU sessions (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: E2, PLAN.md
phase 6A tasks 6A.5 and 6A.6, the services part (branch
claude/e2-gsv-gpu). Needs A1 and A2 merged.

The Switch 2's graphics services, from docs/EGLANTINE.md "Facts to
take" 5 to 9 (rows 17, 19 to 30, 33, 34, 40, 42 to 48, 82, 83, 85, 96,
124 to 126, 147, 149; record each in SWITCH2_NOTES first; rows 46 to 48
come from changes.patch and wait for L2's observation):
- gsv over TIPC (the session ISelfController 2000 returns): the commands
  that hand out the syncpt, GPU and vi2 sessions, layout sizes, native
  memory and images.
- syncpt: the shared page, slot events, the TIPC 0 token echo, signal
  requests and cancels.
- nx2:gpu: GPU-accessible memory, 4K mappings, sparse regions, GPU VA
  equal to CPU VA, job slots completed through events, the VA
  partitions, the characteristics block (the Ampere chip, subcontexts,
  mapping modify).
- vi2: layers, the present command and its fence, the 60 Hz events.
- nvsrv: kestrel's nvdrv protocol without Initialize, the Ampere chip.
Each in its own file, IPC unit tests and homebrew programs with
independent models. The GPU engines are lane C's: hand submissions to
the existing GPU entry point and note in the pull request what C2 needs.
Update the ROADMAP and ARCHITECTURE.
```

### E3: audio (CLOUD, heard LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: E3 (branch
claude/e3-audio). Ask the owner first: host audio output needs a
dependency (for example cpal, Apache 2.0).

The Switch 2 audio entry (aud:u, EGLANTINE.md rows 38, 39) and the audio
renderer it hands out (Switchbrew "Audio services"): the update protocol
with its revision checks, voices, mixing to 48 kHz stereo, a sink that
writes a WAV file in tests and plays on the host with the window. Tests
with libnx's audio examples built from source, checked against a Python
model of the mix. L1 listens on Windows. Update the ROADMAP.
```

### E4: the video decoder sessions (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: E4 (branch
claude/e4-video). Ask the owner first: a VP9 decoder is a dependency
question (libvpx is BSD-3-Clause), or kestrel writes its own.

The codec sessions games open for their movies (EGLANTINE.md rows 96,
105, 146 to 149): gsv's decoder sessions, the image pool, frame
submission and the syncpoint per decoded frame. Without a decoder:
frames complete as black, named in the log. With the owner's choice of
decoder: real frames. Tests with VP9 streams the session encodes itself
from a pattern (never a title's movie). Update the ROADMAP.
```

## Lane L: the owner's PC (LOCAL)

### L1: Windows confirmations and merging (LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: L1, the
local integration session, on the owner's Windows PC.

1. Confirm what cloud sessions cannot: the deko3d examples on the real
   GPU (`kestrel run` with a window, and run-examples.sh), Vulkan
   presentation, gamepad input, audio when E3 lands.
2. For each open pull request the owner wants merged: check it out,
   run CLAUDE.md's checks on Windows, run the examples, report, and start
   a CI run on the branch (gh workflow run ci.yml --ref <branch>). Merge
   only when the owner says so; after merging several, the owner tags a
   release.
3. Keep the ROADMAP's "Next task" and the waves in docs/SESSIONS.md
   current: which tasks are merged, which can start.
```

### L2: the boot loop on the title (LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: L2,
PLAN.md phase 6A on the owner's title (branch claude/l2-boot-log). Needs
A1 merged; A3 makes it much faster.

The title's ExeFS (and romfs/) is on this PC, extracted by the owner.
Nothing from it enters the repository, logs committed, tests or pull
requests: only identifiers, counts and behaviour in words.
1. `kestrel scan <exefs>`: record the rejected instruction classes and
   counts in the ROADMAP's "Boot log" (new section in phase 6 notes), for
   B1 and B2.
2. `kestrel run <exefs> --headless --auto-stub --trace-ipc --trace-svc`:
   record, in order, each stop and each stubbed command (service,
   command, how often) in the Boot log. That list is lane E's and A2's
   worklist.
3. Observe what cloud sessions cannot: for each changes.patch fact and
   each guess in EGLANTINE.md that a task needs, check it on the title
   (what the request carries, what the program does with the reply) and
   write an observation entry in SWITCH2_NOTES (how observed, on what,
   when), never the title's code or data.
4. Implement small blockers directly when they are local by nature;
   leave the rest to the lane that owns the files.
Repeat after each merge. Update the ROADMAP's Boot log and "Next task".
```

### L3: the first frame (LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: L3,
PLAN.md phase 6A task 6A.6 (branch claude/l3-first-frame). Needs E2 and
C2 merged.

Drive the title from its first GPU submission to its first presented
frame, headless then in a window on the real GPU: run, find the first
stop in the GPU path, record what the title does in SWITCH2_NOTES, fix
what is local, hand the rest to lanes C and E through the Boot log.
--save-frame shows progress (screenshots stay on this PC). Acceptance of
6A: the title's first presented frame headless and in a window on
Windows x86_64. Update the ROADMAP.
```

### L4: speed on the title (LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: L4 (branch
claude/l4-title-speed). Needs L3.

Measure where the title's time goes on the owner's PC: guest MIPS per
core, the OS lock (WaitForAddress/SignalToAddress per frame, EGLANTINE.md
row 109), counter reads, the GPU front and back ends (C6), host GPU load,
frame times (`--stats`). Record numbers and their meaning in the
ROADMAP; turn each bottleneck into a task for its lane, with a clear
target. Fix the OS lock contention here if lane A agrees (the OS layer
behind one lock, ROADMAP "Threaded cores").
```

### L5: controllers (LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: L5,
PLAN.md phase 7 input (branch claude/l5-controllers).

With the owner's real gamepads: the Switch 2 controller styles the title
asks for, several players, motion from controllers that have it, rumble;
the mapping file user/config/input.cfg extended. Checked by playing; unit
tests for the mapping and hid's shared memory. Update the ROADMAP.
```

### L6: the frontend (LOCAL)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: L6,
PLAN.md phase 7 frontend (branch claude/l6-frontend). Ask the owner
first: the toolkit is a dependency choice.

A window with a game list (the library folder setting from CLAUDE.md's
decisions), per-game settings, the controller mapping, and a performance
overlay hook (P3). Tried on Windows by the owner. Update the ROADMAP and
README.
```

## Lane P: polish and platforms (CLOUD, later)

### P1: save data (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: P1 (branch
claude/p1-save-data).

Save data through fsp-srv (Switchbrew "Filesystem services": save data
creation, mounting, commit) stored under user/save/<program id>/, with
homebrew tests. L2 checks the title saves and loads. Update the ROADMAP.
```

### P2: shader and pipeline cache (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: P2 (branch
claude/p2-shader-cache). Needs C3 merged.

Translated shaders and Vulkan pipelines cached on disk under user/cache/,
keyed by the guest program's hash and state, so a second run has no
compile stutter; a cache that cannot be read is rebuilt, never trusted
blindly. Tests with the deko examples. Update the ROADMAP.
```

### P3: performance overlay (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: P3 (branch
claude/p3-overlay).

An optional overlay in the window (frame time, fps, guest MIPS per core,
GPU front and back load) drawn by kestrel's renderer, off by default,
toggled by a key. Tests with the examples. Update the ROADMAP and README.
```

### P4: licence notices in the packages (CLOUD)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: P4 (branch
claude/p4-notices).

A licence-notices file in every release package listing the bundled
crates and their licences (winit, softbuffer, gilrs, ash and their
dependencies), generated from Cargo metadata by a script in scripts/,
checked in CI's packaging step. Update the ROADMAP.
```

### P5 to P7: the other platforms (CLOUD, after the Windows milestone)

```text
Follow docs/SESSIONS.md, "Rules for every session". Your task: P5
(macOS ARM), P6 (Windows ARM and Linux ARM) or P7 (macOS x86_64), only
after the owner declares the Windows milestone met (PLAN.md section 3,
"After the milestone").

Bring the features built for Windows x86_64 to the platform: on ARM
hosts, phase 3's in-process fallback first (guest code run directly, with
load-time rewriting of svc, the system and thread registers, x18 and
counter reads); on macOS, Vulkan through MoltenVK. Test under qemu or on
CI's runners; every existing test keeps passing on all six targets.
Update the ROADMAP.
```
