# kestrel

# ⚠️ DISCLAIMER — READ BEFORE USE ⚠️

> [!CAUTION]
> ## 🤖 This project was entirely VIBE CODED by Claude AI
>
> **The code, binaries and scripts in this repository have NOT been reviewed or verified by an actual developer.**
>
> - 🧪 This is **merely an experiment**, not production-ready software.
> - ⚠️ **Use at your own risk.** It may contain bugs, security vulnerabilities, or behave in unexpected ways.
> - 🚫 **I am not responsible** for any damage, data loss, hardware issues, or any other problem that may arise from using this project.
> - 🔍 If you plan to use any part of it, **review and test the code yourself first.**
>
> By using, building, or running anything from this repository, you accept these terms.


An experimental, clean-room ARM64 console emulator written in Rust, aimed at
next-generation hybrid handheld consoles. It builds and runs on **x86_64 and
aarch64 hosts** (Linux, macOS, Windows) from day one.

> Status: early. It runs open-source homebrew console programs built with
> libnx (the standard examples pass on every platform), with no GPU yet. It
> does not run commercial software, and nothing here helps anyone obtain
> it.

## What works

- **Portable ARM64 interpreter** — integer data processing, branches, all
  load/store addressing modes, pairs, exclusives, acquire/release, CAS and
  CASP, ARMv8.1 LSE atomics, CRC32, user system registers, scalar half, single and double
  precision floating point (Arm NaN, default-NaN and flush-to-zero rules,
  bit-identical on every host), and Advanced SIMD (NEON) vector and scalar
  instructions with structure loads and stores, half precision, the dot
  product and the AES, SHA-1 and SHA-256 instructions of the Switch 2's
  Cortex-A78C. Every instruction has a unit test generated from an
  independent Python model. ~140 MIPS on a modern x86 core.
- **Guest address space** with permissions and fault reporting.
- **Loaders** for the ELF test programs in `guest/` and for homebrew NRO
  executables (segments, BSS, asset section), with the homebrew launch
  environment: loader config entries and the argument string.
- **HLE kernel stub** answering a first set of supervisor calls: exit,
  sleep, tick, debug output, svcGetInfo for the process layout, memory
  sizes, entropy and program identity, and the memory calls (heap,
  svcQueryMemory with memory types, permissions, attributes, svcMapMemory,
  physical memory), a handle table with thread, event and transfer-memory
  objects, the thread and process ID calls, threads (creation,
  priorities, cores, sleep and yield on a deterministic scheduler), and
  synchronisation: waiting on events and threads, mutexes, condition
  variables and address arbitration, with deadlock reports.
- **IPC**: the message format (CMIF, CMIF domains and TIPC), named
  ports, client sessions, svcSendSyncRequest and the service manager sm:,
  with unknown services and commands reported by name.
- **First services**, answering as system version 20.0.0: set:sys
  firmware version, apm, the applet manager commands an application
  sends at startup, hid with its shared memory and one controller,
  the time service (clocks that start at 2025-06-05 00:00 UTC and run
  with the guest's clock, in the UTC time zone), the file system
  with the SD card as a folder on your computer, the battery state of
  a docked console, and the display (vi and the GPU driver's memory
  handles), so a program's framebuffer shows in the window.
- **Error reports**: when a program aborts or throws a fatal error, kestrel
  prints the reason and its result code as the console writes it, with
  the module's name (`guest aborted (Panic) at 0x...: result 2345-0033
  (Libnx: InitFail_FS)`).
- **Backend selection** that reports which fast path the host will get.

```
$ cargo run --release -p kestrel-cli -- run guest/bin/algos.elf
fib(25) = 75025
crc32 = 0x00000000414fa339
primes <= 100000 = 9592
sort ok
signed div/mod = 97 99
```

Add `--stats` to see the instruction count and speed; `kestrel selftest`
reports speed for the whole suite.

### The SD card

Homebrew keeps its own files on the console's SD card: its settings and
saves, logs, and files you give it. kestrel's SD card is the folder
`user/sdmc` next to the `kestrel` executable (portable, as in Dolphin's
portable mode), made on first use. What a program writes there stays
between runs, and you can open the folder to add, back up or delete
files. `--sd <folder>` uses another folder instead. A program can never
reach outside the folder: `..`, host links and names some systems read
differently are refused. Games can be run from any folder.

### The window and controls

A homebrew program opens a window (`--headless` runs it without one).
The window shows what the program draws into its framebuffer (there is
no GPU yet, so programs that render with it show nothing), and takes
your keyboard and gamepads and gives them to the program as a Switch
controller. Text a program prints for debugging goes to the terminal.

kestrel emulates one controller, a Pro Controller by default. Which keys
and gamepad buttons press what is set in `user/config/input.cfg` next to
the executable, written with defaults on first run:

| Switch | Keyboard | Gamepad |
|--------|----------|---------|
| A, B, X, Y | X, Z, C, V | right, bottom, top, left face buttons |
| L, R, ZL, ZR | Q, E, 1, 3 | bumpers, triggers |
| Plus, Minus | Enter, Backspace | Start, Select |
| D-pad | arrow keys | D-pad |
| Left stick, right stick | W A S D, I J K L (click: F, H) | the sticks |

Set `controller = handheld` in that file for the Joy-Con attached to the
console instead. To see what kestrel makes of your keys and pads, run
padview from the repository and press away; each change prints a line
in the terminal. Close the window to stop.

```
cargo run --release -p kestrel-cli -- run guest/bin/padview.nro
```

With a window, programs run in real time: the guest's clock is the
host's, a program that sleeps waits for real, and there is no instruction
budget. Without one (`--headless`, the self-test) time is counted, one
tick of the 19.2 MHz counter every 52 instructions (a 998.4 MHz CPU, as
on a Switch 2), so every run repeats exactly. On Linux, gamepads need libudev (`libudev.so.1`, on every desktop
distribution).

### The standard homebrew examples

`scripts/build-examples.sh` builds four of the public homebrew examples
(hello world, reading the controller, listing the SD card, threads) from
source with the official homebrew toolchain in Docker, and
`scripts/run-examples.sh` runs them in kestrel and checks what each one
shows. CI does both on every platform. To try one yourself after
building them:

```
cargo run --release -p kestrel-cli -- run target/examples/bin/hello-world.nro
```

Press **+** (Enter on the keyboard) to leave it. For runs with nobody at
the controls, `--press Plus@2` presses a button for a second at that
time, and `--save-frame shot.png` saves the last frame shown.

A homebrew program sees its own file on the SD card at
`sdmc:/switch/<name>.nro` (read-only, read from wherever it is on your
computer), as the homebrew menu would place it; what it writes goes to
`user/sdmc`.

### What kestrel does not do yet

When a program uses a system call, service or command kestrel does not
implement, kestrel stops it and names what was missing:

```
unimplemented: SVC 0x7f (CallSecureMonitor), at pc 0x21024c
recorded in .../user/logs/unimplemented.log
```

Each such stop adds a line to `user/logs/unimplemented.log` next to the
executable (the time, kestrel's version, the program and the call), so
after running a few programs the file lists what they need next.
`kestrel run` then exits with code 5.

## Building

```
cargo build --release
cargo test
cargo run --release -p kestrel-cli -- selftest
```

## Self-test and test programs

`kestrel selftest` runs twenty-two ARM64 programs compiled into the binary and
compares their output byte for byte with answers computed independently
by `scripts/gen-expected.py` (Python, hashlib, zlib), so testers need
nothing but the executable. Each integer C program is in the suite twice:
built integer-only, and built with floating point and NEON allowed
(`name-neon`), so auto-vectorised code must give the same output:

| Program      | Exercises                                                        |
|--------------|------------------------------------------------------------------|
| `hello`      | loader, entry, debug-print SVC, exit                             |
| `asm_checks` | 121 hand-written instruction checks: carries, bitfields, atomics, exclusives, jump tables, system registers, SIMD moves, NaNs, subnormals, flush-to-zero, saturation |
| `vm`         | a bytecode VM run with switch dispatch and with function-pointer dispatch |
| `bigint`     | 128-bit multiply, Miller-Rabin on 64-bit primes, SWAR popcount  |
| `algos`      | recursion, CRC32, prime sieve, heap allocation, sort            |
| `sha256`     | SHA-256 of three inputs (about 6.5M guest instructions)         |
| `fp`         | 127 scalar FP operations printed as bit patterns: NaN rules, flush-to-zero, default NaN, FPCR rounding modes |
| `intrinsics` | 168 NEON intrinsics across every instruction class, including structure loads and stores |
| `hbtest`     | a homebrew NRO with its own startup code: loader config entries, arguments, `.bss`, `.data`, self-relocation |
| `hblayout`   | a homebrew NRO checking the svcGetInfo layout: regions in the 39-bit space without overlap, documented sizes, heap placement, error results |
| `hbmemory`   | a homebrew NRO driving the memory calls: memory types from svcQueryMemory, heap resizing, permissions and attributes, svcMapMemory, physical memory, error results |
| `hbhandles`  | a homebrew NRO checking handles, events and transfer memory, and filling the handle table |
| `hbthreads`  | a homebrew NRO running threads on four cores: priorities, core masks, sleep and yield, time slices, exclusive monitors |
| `hbsync`     | a homebrew NRO checking waits, timeouts, cancelling, mutexes, condition variables and address arbitration |
| `hbipc`      | a homebrew NRO talking to sm: over CMIF and TIPC, from two threads, with bad names, close requests and closed handles |
| `hbapplet`   | a homebrew NRO sending libnx's startup requests: set:sys firmware version, apm, appletOE as a domain with its interfaces and events |
| `hbhid`      | a homebrew NRO setting up input as libnx does: hid's applet resource and npad setup, and mapping its shared memory, with each kind of bad mapping |
| `hbfs`       | a homebrew NRO using the file system as libnx does: fsp-srv as a domain with cloned sessions, then creating, writing, reading, listing, renaming and deleting on an empty SD card, with bad paths and locked files |
| `hbinput`    | a homebrew NRO reading player 1 as libnx's padUpdate does, while the self-test plays a scripted Pro Controller |
| `hbdisplay`  | a homebrew NRO setting up the display as libnx does (vi, nvdrv, the buffer queue) and showing three frames; the self-test checks each frame |
| `hbpower`    | a homebrew NRO reading the battery and charger state through psm, then sending a debugger-only svcBreak and a report-only fatal error, after which it carries on |
| `hbtime`     | a homebrew NRO starting the time service as libnx does: the clocks, reading them from shared memory with CNTPCT_EL0, calendar times in UTC, and the clocks moving with a sleep |

`kestrel extract <dir>` writes them out as `.elf` files for `kestrel run`.

`kestrel selftest --backend jit` runs the same programs on the x86_64
recompiler (Windows and Linux x86_64; `kestrel info` says whether this
build has it). Without `--backend`, the self-test also runs the programs
on every other backend the build has and ends with each one's speed.
`kestrel run --stats` adds frame times for a program that shows frames:
the average, the 1% low (the 99th-percentile frame time) and the worst,
as host time between frames.

`kestrel diff --backend <name>` is the differential tester: it runs every
test program (or the ones named after the options) on the interpreter and
on the named backend side by side, on the counted clock, and reports the
first instruction after which their registers, flags, writable memory,
system calls or output differ.

## Packages for testers

Push a version tag and GitHub Actions builds and self-tests these, then
publishes them as a GitHub Release:

```
git tag v0.1.0 && git push origin v0.1.0
```

| Package                    | Built on                | Self-tested natively on     |
|----------------------------|-------------------------|-----------------------------|
| `kestrel-macos-universal`  | macOS 15, Apple Silicon | Apple Silicon and Intel Macs |
| `kestrel-linux-x86_64`     | Ubuntu x86_64           | Ubuntu x86_64               |
| `kestrel-linux-aarch64`    | Ubuntu ARM64            | Ubuntu ARM64                |
| `kestrel-windows-x86_64`   | Windows x86_64          | Windows x86_64              |
| `kestrel-windows-arm64`    | Windows 11 ARM64        | Windows 11 ARM64            |

### Download the latest release and self-test

These links always point at the newest release.

macOS (Apple Silicon or Intel):

```
curl -LO https://github.com/Lordnns/kestrel/releases/latest/download/kestrel-macos-universal.tar.gz
tar xzf kestrel-macos-universal.tar.gz && cd kestrel-macos-universal
xattr -dr com.apple.quarantine .
./kestrel selftest -v
```

Linux (`x86_64` or `aarch64`):

```
curl -LO https://github.com/Lordnns/kestrel/releases/latest/download/kestrel-linux-x86_64.tar.gz
tar xzf kestrel-linux-x86_64.tar.gz && cd kestrel-linux-x86_64
./kestrel selftest -v
```

Windows PowerShell (`x86_64` or `arm64`):

```
Invoke-WebRequest https://github.com/Lordnns/kestrel/releases/latest/download/kestrel-windows-x86_64.zip -OutFile kestrel.zip
Expand-Archive kestrel.zip -DestinationPath .; cd kestrel-windows-x86_64
.\kestrel.exe selftest -v
```

Each package has `TESTING.md` with step-by-step instructions. You can also
run the workflow by hand from the Actions tab to get the packages as
artifacts without making a release.

Rebuilding the guest test programs needs `clang` and `ld.lld` (they
cross-compile, so any host works): `./scripts/build-guest.sh`. On an ARM64
Linux or macOS machine, `./scripts/check-host.sh` builds the C programs
natively and checks that the real CPU prints exactly the expected output.
`./scripts/check-optimised.sh` rebuilds every program at `-O3` for Armv8.2
and checks that kestrel still runs each one to the expected output.

## How it runs on both architectures

| Host     | Today            | Fast path (planned)                                        |
|----------|------------------|------------------------------------------------------------|
| x86_64   | interpreter      | `jit`: ARM64 → x86_64 dynamic recompiler                   |
| aarch64  | interpreter      | `native`: guest code runs directly under KVM, Hypervisor.framework or WHP |

Every backend implements the same `CpuBackend` trait. The interpreter is the
reference: faster backends are differentially tested against it. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Roadmap

The plan of record, with its rules and acceptance criteria, is
[PLAN.md](PLAN.md); progress against it is tracked in
[docs/ROADMAP.md](docs/ROADMAP.md). In short:

1. Homebrew (in progress: the NRO loader, process layout, memory calls,
   handle table, threads, synchronisation, IPC with sm:, and the settings,
   applet and input setup services are done): time, filesystem and the
   other first OS services, from public documentation.
2. `native` backend on aarch64 hosts, starting on macOS, developed alongside
   the OS services; then a `jit` backend on x86_64. Both run on the guest
   memory arena, which is done.
3. Graphics at two levels: GPU emulation with shader translation to Vulkan
   for any software, and graphics-API-level translation for commercial games.
4. Running software owners have legally obtained, tested locally only.

## Legal and project rules

- Clean-room only. No console firmware, keys, SDKs, leaked material or
  commercial game code may be committed, linked or referenced. See
  [CONTRIBUTING.md](CONTRIBUTING.md).
- The project ships nothing derived from a console. Users who want to run
  their own software are responsible for dumping it from hardware they own.
- Not affiliated with or endorsed by any console manufacturer.

## License

MIT. See [LICENSE](LICENSE).
