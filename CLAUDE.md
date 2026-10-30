# kestrel: notes for Claude sessions

This file hands a new session (local or in the cloud) what earlier
sessions learned. The plan and the status live elsewhere; read them first:

1. `PLAN.md`: the plan of record, its hard rules (section 2) and how to
   start a session (section 0). If anything here disagrees with it,
   `PLAN.md` wins.
2. `docs/ROADMAP.md`: the status tables, the "Phase N notes" (what was
   built, kestrel's choices where the documentation is silent, the gaps)
   and "## Next task". It is the single record of progress; keep it
   current in the same commits as the work, not in this file.

## Where things stand (2026-10-30)

- Phases 0, 1 and 2 are done. Phase 2 was accepted on `v0.1.17`: the four
  standard libnx examples (hello-world, read-controls, sdmc, barrier)
  pass on all six native CI jobs. All work is pushed.
- The owner has not yet picked the next task. The candidates, from the
  ROADMAP's "Next task":
  1. the clock rate (the owner's decision): kestrel counts one clock tick
     per guest instruction, so programs see a CPU about 50 times slower
     than a Switch's in guest time; explain this plainly, then ask;
  2. phase 3 on Linux KVM and Windows (phase 3 on macOS needs a session
     on the owner's Mac);
  3. a licence-notices file in the release packages (they now bundle
     MIT/Apache crates: winit, softbuffer, gilrs and their dependencies);
  4. more homebrew, fixing what `user/logs/unimplemented.log` names;
  5. the later input tasks: other controller types (GameCube,
     NES/SNES/N64/Genesis), several players, real Joy-Con motion, rumble
     and pairing.

## How the owner works

- One task at a time. At the end of each task, summarise what changed,
  what was verified (tests, the mutation check, the -O3 guest build) and
  what is left; then ask whether to push and whether to start the next
  task. Push only when the owner says so (usually "yes push, then start
  <next task>"). Never start the next task unasked.
- The owner makes the product decisions. Before asking, explain in plain
  terms what the feature is for and what the user would see or do;
  comparing with emulators they know (Dolphin) helps. Then ask with a
  recommended option first.
- CI runs only from the Release workflow on a version tag (`v0.1.x`) or a
  manual run, never on plain pushes. After a push, the owner tags a
  release and reports the run (often as a screenshot). Say when a tag is
  needed to verify something.
- The owner may close a session and start a new one at any point: the
  repository must always say where things stand (ROADMAP "Next task").

## Decisions the owner has made

Each is also recorded in the ROADMAP's phase notes.

- System version 20.0.0; clock starts 2025-06-05 00:00 UTC, time zone
  UTC.
- Portable, Dolphin-style: kestrel's `user/` folder next to the
  executable holds the SD card (`user/sdmc`), settings
  (`user/config/input.cfg`) and logs; `--sd` overrides the card.
- A game library can be anywhere on the host, with kestrel elsewhere.
  Files kestrel only reads come from where they are; what a program
  writes goes to `user/`. A homebrew program sees its own file read-only
  at `sdmc:/switch/<name>.nro` (or its real place if it is inside the
  card's folder). A setting naming the library folder comes later, with
  the frontend.
- Unimplemented calls keep stopping the program, named with their
  identifiers, and are appended to `user/logs/unimplemented.log`.
- A window by default for homebrew (winit, softbuffer, gilrs; `--headless`
  to skip): one emulated controller (Pro Controller or handheld) mapped
  from the keyboard and any gamepad. With a window the program runs in
  real time with no instruction budget.
- Console output is shown on the real screen path (vi, nvdrv, the buffer
  queue), not a text hook.
- Docker Desktop on the owner's PC, with the same pinned devkitPro image
  as CI, to build the libnx examples.

## Rules that are easy to forget

`PLAN.md` section 2 has the full list. In practice:

- Ask before adding any dependency. The core crates use std only.
- Licences: MIT, BSD, Apache 2.0, zlib (ISC has been accepted as
  MIT-equivalent). Nothing GPL in the product.
- Clean-room: no leaked material. Switchbrew and libnx (ISC) are read for
  the protocol only. Hardware formats come from public descriptions (for
  example the block-linear layout from envytools and Mesa's `nil`).
- Every expected value comes from an independent source: the Python
  model in `scripts/gen-expected.py`, a real assembler for CPU encodings
  (clang `--target=aarch64-none-elf`), or real hardware. Never from
  kestrel's own output.
- Where the documentation is silent, choose, say so in a comment ("kestrel's
  choice") and list the choice in the ROADMAP notes. Anything undocumented
  stops with its name and number rather than guessing.

## Checks before every commit

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`, and also with
  `--no-default-features` for `kestrel-cli`
- on Windows also `cargo clippy --workspace --all-targets
  --no-default-features --target x86_64-unknown-linux-gnu -- -D warnings`:
  Windows never compiles the `#[cfg(unix)]` tests, which broke `v0.1.16`
- `cargo test --workspace`
- `python3 scripts/gen-expected.py --check`
- `cargo run --release -p kestrel-cli -- selftest`
- for a change to a service or the guest programs: the program built at
  `-O3 -march=armv8.2-a` (`GUEST_OUT`, `GUEST_OPT` for
  `scripts/build-guest.sh`) gives the same output, and one deliberate bug
  in the new code makes a test fail, then is undone (the mutation check)
- for display or libnx-facing changes: `bash scripts/build-examples.sh`
  (needs Docker) then `bash scripts/run-examples.sh` (4/4 PASS)

One logical change per commit, with the ROADMAP, ARCHITECTURE and README
updated in the same series. End each commit message with the
Co-Authored-By line from the session's attribution instructions.

## Running things

- `cargo run --release -p kestrel-cli -- run <program.nro>` opens a
  window; `--headless` runs without one. `--press Plus@2` presses a
  button for 1 s at that time; `--save-frame shot.png` saves the last
  frame. Read the PNG to see what a program drew.
- The examples: `scripts/build-examples.sh` (Docker, pinned
  `devkitpro/devkita64:20260219` by digest, libnx 4.12.0) writes
  `target/examples/bin/*.nro`; `scripts/run-examples.sh [kestrel]` runs and
  checks them.
- Finding why a libnx program stops: `--trace --max-steps N` and map the
  last addresses to symbols with `llvm-nm` on the example's `.elf` in
  `target/examples/switch-examples-*/...` (NROs load at 0x8000000).

## Tooling pitfalls

These apply on the owner's Windows PC (Git Bash and PowerShell); most
are harmless elsewhere.

- Never pass backslash escapes through bash (heredocs or `python -c`):
  they put real newlines and NUL bytes into files, twice already. Write
  edit scripts and commit messages to files with the Write tool, then
  run them. Long heredocs with nested quotes also fail.
- Files may have CRLF endings (`core.autocrlf=true`); edit scripts should
  normalise to LF before matching and restore after. Python's default
  encoding there is cp1252: open files with `encoding="utf-8"`.
- `core.fileMode` is false in that clone: give a new script its
  executable bit with `git update-index --chmod=+x <path>` and check with
  `git ls-tree HEAD scripts/`. `scripts/armfp.py` is a module and stays
  100644.
- A shell hook (rtk) rewrites grep, ls and diff output; use the Grep tool
  or `/usr/bin/grep` when output looks odd.
- Editor (clangd) diagnostics on `guest/src` are noise: it parses the
  ARM64 guest code as the host. The real check is the guest build.
- gh is at `"/c/Program Files/GitHub CLI/gh.exe"` (account Lordnns, repo
  `Lordnns/kestrel`); `gh run view <id> --job <job id> --log` reads CI
  logs. The docker CLI is under
  `C:/Program Files/Docker/Docker/resources/bin` when PATH is stale, and
  Docker Desktop must be running.
- A single self-test run varies by about 5 MIPS; run it three times
  before calling something a speed regression.
