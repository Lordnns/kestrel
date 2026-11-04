# Eglantine: what it shows, and what kestrel takes from it

Written 2026-11-03, from Eglantine at commit `8500168`
(github.com/Djihads80/Eglantine, branch `nx2`). Re-check against its
current tree before relying on a detail: it changes daily.

## What Eglantine is

A Switch 2 emulator in C++, a fork of Eden (itself from yuzu and Sudachi),
licensed GPL-3.0-or-later. Its README says it runs Mario Kart World "at
decent/full speed depending on specs and configurations", with stutter and
graphical errors, and Splatoon Raiders at variable speed. Its
`NX2_NOTES.md` lists, in order, about 150 blockers it hit getting four
Switch 2 titles to boot and play, each with its cause and fix;
`NX2_GPU_THREAD_SPLIT.md` describes its main speed change.

## How kestrel uses it (owner, 2026-11-03)

`PLAN.md` section 2, "Other emulators as fact sources":

- kestrel stays MIT. No Eglantine code is copied or translated into kestrel
  (GPL-3.0; Eden's copy of dynarmic is relabelled GPL-3.0 as well).
- Sessions may read its notes and its source to learn **facts**: service
  names and command numbers, request and reply layouts, GPU classes and
  methods, which shader instructions games use, and design ideas for speed.
- Each fact is written into `docs/SWITCH2_NOTES.md` first, citing the file
  and row or line and Eglantine's commit, then implemented from that entry
  in kestrel's own structure and words.
- **Provenance.** Several of its pieces say they come "from changes.patch",
  a patch of unknown origin (NX2_NOTES rows 46, 47, 48, 50, 53, 54: the
  syncpoint shim page, native memory and images, presentation, semaphore
  suspension, the Turing macro interpreter, the Ampere shader lowering). A
  fact whose only source is one of those rows is confirmed by our own
  observation on a title the owner has before kestrel implements it
  (section 2: nothing from leaked material or private forks).

Row numbers below are NX2_NOTES "Blockers hit, in order" rows. The file
reuses numbers 123 to 128 twice (GameChat and camera first, then GPU
thread costs); "127b" means the second row 127.

## Side by side

| Area | Eglantine | kestrel (2026-11-03) |
|---|---|---|
| Base and licence | Eden/yuzu, C++, GPL-3.0; a decade of Switch 1 OS emulation underneath | its own Rust, MIT, clean-room; core crates std-only |
| CPU | dynarmic (recompiler), patched for LSE atomics, CAS/CASP, LDAPR, the PAC family as identity, MTE tags as 0 (row 3); NCE on ARM Android | complete interpreter (v8.2, FP16, dot product, crypto, CRC32, CASP, LSE) and its own x86_64 recompiler (about 5 times the interpreter; floating point and NEON still interpreted). Missing: LDAPR/LDAPUR, the PAC family (RETAA/RETAB, BRA*/BLRA*, LDRA*, PAC*/AUT*, XPAC*, PACGA), the MTE tag instructions |
| Loader | loose ExeFS: rtld, main, subsdk*, sdk NSOs and `main.npdm` (Switch 2 ACID with a 0x300-byte signature area, row 5), RomFS from a sibling `romfs/` folder (rows 4, 13) | ELF and NRO only |
| Cores | six (row 78); folding cores 4 and 5 onto 0 and 1 changed which heap a job allocated from and crashed the game (rows 77-79) | four (`thread::CORES`), each on its own host thread |
| Memory | 12 GB layout; total memory counts the alias region (rows 37, 80); `svcGetInfo` 21, 29-33 (rows 7, 8, 16); svc 0x80 SetGpuMemoryAttribute (rows 27, 47) | one documented 39-bit homebrew layout; the arena and lock-free page table are ready for more |
| OS and services | yuzu's services plus about 15 for Switch 2 (below) | the homebrew set: sm, set:sys, apm, appletOE, hid, time, fsp-srv (SD), psm, fatal, vi, nvdrv and the buffer queue; unknown calls stop by name |
| GPU front end | Ampere classes C797 (3D), C7C0 (compute), C7B5 (copy) run on yuzu's Maxwell engines (row 31); Turing MME64 macros (rows 53, 106, 117, 140); QMD 3.0 compute (rows 53, 60-62); unmet semaphore acquires suspend the channel (row 50) | 5A done; 5B: command processor, Maxwell (Fermi-style) MME, upload and copy engines, clears, render targets, draws |
| Shaders | SM86 programs lowered to yuzu's Maxwell IR (rows 54, 88, 91-95); task and mesh programs to VK_EXT_mesh_shader (row 89) | Maxwell shaders to SPIR-V; deko3d's triangle and cubes draw |
| Textures | ASTC with the Switch 2 GOB swizzle (row 69), recompressed to BC1/BC3 (rows 99, 103, 107); huge images sparse and unbacked (rows 87, 94) | block-linear layouts for the deko3d examples |
| Video, audio | VP9 through FFmpeg (row 148); `aud:u` over yuzu's audio renderer (rows 38, 39) | none |
| Testing | A/B runs, an offline shader-lowering test, a recorded-macro test harness | independent oracles for every expected value, the differential tester, CI on six native targets |

## Where Eglantine spends its time

Its measurements matter more to kestrel than its code: they say where a
Switch 2 game's cost is.

- **The GPU thread is the limit, not the CPU** (`NX2_GPU_THREAD_SPLIT.md`;
  rows 110, 122, 141): one host thread parsing commands, running macros,
  setting state and recording 33,000 to 55,000 draws a second, ~100% busy,
  while the host GPU sits at 20-40% and the game's main thread mostly polls
  the counter. Splitting it into a front thread (command parsing and macros)
  and a back thread (engine methods, draws) joined by a lock-free ring gave
  1.46 times the draws a second (rows 142-145).
- **Macros**: about a million calls a second, 7 instructions each, 96% exact
  repeats; a memo of calls that depend only on their parameters and the data
  RAM words they read replays 97% of them (row 117), then an x86_64 compiler
  for MME64 (row 140).
- **Same-value register writes**: about 60% of 6 million 3D register writes
  a second write the value the register already holds; dropping them after
  shadow RAM processing helped (row 135).
- **Buffer downloads at fences** cost 250-330 MB/s for data the guest almost
  never reads; turning them off and trapping the guest's first read of a
  GPU-written page instead fixed animated colliders without the cost (rows
  131, 150).
- **The guest counter**: the game reads CNTPCT about 17 million times a
  second while it waits (row 127b); on AMD hosts without CPUID leaf 0x15 the
  TSC rate had to be measured (row 111).
- **The OS lock**: about 1,400 system calls a frame, mostly
  WaitForAddress/SignalToAddress from the game's job system, all through one
  scheduler lock (row 109). kestrel's OS layer is also behind one lock
  (ROADMAP "Threaded cores"), so the same contention will show.
- **Pacing**: presents need 60 Hz vblank pacing (row 71), and the present
  reply must not report every buffer free before the GPU has finished, or
  the game runs a whole ring of command memory ahead (row 55).

## Facts to take, in the order a boot needs them

Each becomes a `docs/SWITCH2_NOTES.md` entry when its task implements it
(`PLAN.md` phase 6A).

1. **Instructions in the titles' code** ("Titles" table, row 3): RETAA/RETAB
   (100,000+ per title), LDAPR (37,000 to 100,000), LSE atomics, CAS, SWP,
   CASP, XPAC, PAC, PACGA, one STG. Then about 60 to 110 rare SIMD encodings
   in `sdk` (FP16 and v8.2 forms).
2. **Process** ("Titles", rows 5, 7, 8, 16, 37, 80): NPDM asks for 64-bit,
   a 39-bit address space, cores 0 to 5 (mask 0x3f), priorities 28 to 59,
   kernel version 9.1, SVCs 0x01 to 0x36 and 0x80; `svcGetInfo` 29/30 (the
   region between alias and ASLR), 31/32 (remote memory), 33 (page granule,
   0x1000).
3. **Applet and oe** (rows 9, 10, 12, 14, 15, 18): appletOE command 10;
   IApplicationProxy 40 and 60 (60 is a graphics-scale interface whose
   command 10 returns 10, 20 or 30); IApplicationFunctions 320, 321;
   GetPseudoDeviceId without a NACP.
4. **Filesystem** (row 13): the process registered with fsp-srv and given
   its RomFS.
5. **The graphics system server, gsv** (rows 17, 19-21, 33, 47, 96, 105,
   124-126, 146, 148, 149): opened by TIPC on the session ISelfController
   2000 returns; 202 syncpt, 497 GPU, 305 vi2, 1510 layout size, 111/112/
   114/423/426/428/429/430/435 native memory and images, 9xx/10xx/12xx codec
   and chat sessions.
6. **syncpt** (rows 19, 28, 29, 43, 147): the shared page's layout, slot
   events, the TIPC 0 token echo, request and cancel signals.
7. **nx2:gpu** (rows 20, 27, 30, 40, 42, 44, 45, 85, 149): GPU-accessible
   memory, 4K mappings into sparse regions, GPU VA equal to CPU VA, job
   slots completed through events, the VA partitions, the characteristics
   block (chip 0x17E, TSG subcontexts, mapping modify).
8. **vi2** (rows 21, 48, 82, 83): layers, the present command 229 and its
   fence, events 31/32 signalled at 60 Hz.
9. **nvsrv** (rows 23-26, 34, 46): the nvdrv protocol without Initialize,
   the Ampere chip and classes, nvhost-gpu and nvhost-tsg-gpu ioctls.
10. **Audio, fonts, newer SDKs** (rows 36, 38, 39, 123, 127): `aud:u`,
    `pl:u` 2000/2010, `chat:u`, the camera context.
11. **GPU command stream** (rows 31, 32, 49-53, 56, 58, 60-62, 70, 73, 84):
    class binds, syncpoint increments, subchannel 2 defaults, semaphore
    acquire suspension, methods split across GPFIFO entries, 4-byte
    releases, MME64 with per-channel data RAM, MME DMA reads, QMD release
    slots 24/28/52, max releases, latched waiters.
12. **Shaders and textures** (rows 54, 63-67, 69, 74, 87-95, 97): the SM86
    instructions the titles use, constant bank 24 for console blit
    parameters, MUFU.SIN/COS in turns, task/mesh conventions, ASTC GOBs.

## What kestrel keeps that Eglantine lacks

- An MIT licence and a clean-room record (every fact sourced in
  `docs/SWITCH2_NOTES.md`).
- Correctness tooling: independent oracles, the differential tester, the
  hardware oracle, CI on six native targets.
- One portable codebase in Rust with a std-only core.
- The real-time clock design (`PLAN.md` section 3) and deterministic runs
  for tests.
