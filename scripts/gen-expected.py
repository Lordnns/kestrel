#!/usr/bin/env python3
"""Compute guest/expected/*.txt independently of kestrel.

Each function models what one program in guest/src must print, using Python
arithmetic, hashlib and zlib. Nothing here runs kestrel or reads its output,
so a match in `kestrel selftest` is checked against an outside source
(PLAN.md, "Every expected value comes from an independent source").

    python3 scripts/gen-expected.py          # rewrite guest/expected
    python3 scripts/gen-expected.py --check  # fail if any file differs
"""

import hashlib
import math
import re
import sys
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import armfp  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "guest" / "src"
OUT = ROOT / "guest" / "expected"
M64 = (1 << 64) - 1


def hex64(v):
    """rt.h put_hex: 0x plus 16 lowercase digits."""
    return f"0x{v & M64:016x}"


def c_div(a, b):
    """C99 signed division truncates toward zero."""
    q = abs(a) // abs(b)
    return q if (a < 0) == (b < 0) else -q


def hello():
    return "Hello from guest ARM64 code!\n"


def algos():
    fib = [0, 1]
    while len(fib) <= 25:
        fib.append(fib[-1] + fib[-2])

    crc = zlib.crc32(b"The quick brown fox jumps over the lazy dog")

    limit = 100000
    is_p = bytearray([1]) * (limit + 1)
    is_p[0] = is_p[1] = 0
    for i in range(2, math.isqrt(limit) + 1):
        if is_p[i]:
            is_p[i * i :: i] = bytearray(len(is_p[i * i :: i]))
    primes = sum(is_p)

    a, b = -7, 2
    q = c_div(a, b)
    r = a - q * b
    return (
        f"fib(25) = {fib[25]}\n"
        f"crc32 = {hex64(crc)}\n"
        f"primes <= 100000 = {primes}\n"
        # The program sorts 2000 values and checks the order; a correct
        # sort always reports success.
        "sort ok\n"
        f"signed div/mod = {q + 100} {r + 100}\n"
    )


def is_prime(n):
    """Deterministic Miller-Rabin for n < 2^64 (bases 2..37), on Python ints."""
    if n < 2:
        return False
    bases = [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37]
    for p in bases:
        if n % p == 0:
            return n == p
    d, s = n - 1, 0
    while d % 2 == 0:
        d, s = d // 2, s + 1
    for a in bases:
        x = pow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(s - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def bigint():
    f = math.factorial(30) & ((1 << 128) - 1)
    p = 0xFEDCBA9876543210 * 0x0123456789ABCDEF
    lines = [
        f"30! = {hex64(f >> 64)}:{hex64(f)}",
        f"128-bit product = {hex64(p >> 64)}:{hex64(p)}",
    ]
    for label, n in [
        ("2^61-1       : ", (1 << 61) - 1),
        ("2^64-59      : ", 2**64 - 59),
        ("2^64-1       : ", 2**64 - 1),
        ("561          : ", 561),
        ("1e18+9       : ", 10**18 + 9),
    ]:
        lines.append(label + ("prime" if is_prime(n) else "composite"))
    lines.append(f"powmod(3, 2^63, 2^64-59) = {pow(3, 2**63, 2**64 - 59)}")
    lines.append(f"popcount(0xdeadbeefcafebabe) = {bin(0xDEADBEEFCAFEBABE).count('1')}")
    return "\n".join(lines) + "\n"


def vm():
    n = 1000
    sum_squares = n * (n + 1) * (2 * n + 1) // 6
    x, steps = 27, 0
    while x != 1:
        x = x // 2 if x % 2 == 0 else 3 * x + 1
        steps += 1
    agree = " (both dispatchers agree)\n"
    return (
        f"vm sum_squares = {sum_squares}{agree}"
        f"vm collatz(27) = {steps}{agree}"
        f"vm 20!         = {math.factorial(20)}{agree}"
    )


def sha256():
    def line(label, data):
        return f"{label}{hashlib.sha256(data).hexdigest()}\n"

    return (
        line("sha256('')    = ", b"")
        + line("sha256('abc') = ", b"abc")
        + line("sha256(a*1e5) = ", b"a" * 100000)
    )


def asm_checks():
    # Every `expect` holds a value worked out by hand from the Arm rules, so
    # a correct CPU passes all of them. Count them from the source.
    text = (SRC / "asm_checks.S").read_text()
    n = len(re.findall(r"^\s*(?:\d+:)?\s*expect\s", text, re.M))
    return f"asm checks: {n} passed, 0 failed\n"


def fp():
    """guest/src/fp.c: read its operand tables and OP lines, model each one."""
    text = (SRC / "fp.c").read_text()

    def table(name):
        body = re.search(r"\b" + name + r"\[\] = \{(.*?)\};", text, re.S).group(1)
        return [int(v, 16) for v in re.findall(r"^\s*(0x[0-9a-f]+|\d+),", body, re.M)]

    dv, sv, iv = table("DV"), table("SV"), table("IV")
    flags = {"RN": 0, "RP": 1 << 22, "RM": 2 << 22, "RZ": 3 << 22, "FZ": 1 << 24, "DN": 1 << 25}
    ops = re.findall(r"^\s*OP\((\w+), ([\w |]+), (\d+), (\d+), (\d+)\)", text, re.M)
    F, D, S = armfp, armfp.FD, armfp.FS
    lines = []
    for n, (kind, fpcr, a, b, c) in enumerate(ops):
        fpcr = sum(flags[f.strip()] for f in fpcr.split("|"))
        a, b, c = int(a), int(b), int(c)
        F.FZ, F.DN = bool(fpcr & flags["FZ"]), bool(fpcr & flags["DN"])
        mode = {0: "ne", 1: "p", 2: "m", 3: "z"}[(fpcr >> 22) & 3]
        if kind[0] == "S" and kind != "STOD" or kind == "STOI32":
            x, y, z = sv[a], sv[b], sv[c]
        elif kind == "STOD":
            x = sv[a]
        elif kind[0] in "IU":
            x = iv[a]
        else:
            x, y, z = dv[a], dv[b], dv[c]
        to_int = lambda f, r, w, u: F.f_to_int(x, f, r, w, u)
        r = {
            "DADD": lambda: F.f_add(x, y, D),
            "DSUB": lambda: F.f_sub(x, y, D),
            "DMUL": lambda: F.f_mul(x, y, D),
            "DDIV": lambda: F.f_div(x, y, D),
            "DSQRT": lambda: F.f_sqrt(x, D),
            "DFMA": lambda: F.f_fma(z, x, y, D),
            "DNEG": lambda: F.f_neg(x, D),
            "DABS": lambda: F.f_abs(x, D),
            "DMIN": lambda: F.f_maxmin(x, y, D, False, True),
            "DMAX": lambda: F.f_maxmin(x, y, D, True, True),
            "DFLOOR": lambda: F.f_rint(x, D, "m"),
            "DCEIL": lambda: F.f_rint(x, D, "p"),
            "DTRUNC": lambda: F.f_rint(x, D, "z"),
            "DROUND": lambda: F.f_rint(x, D, "a"),
            "DRINT": lambda: F.f_rint(x, D, mode),
            "DNEARBY": lambda: F.f_rint(x, D, mode),
            "DTOI32": lambda: to_int(D, "z", 32, False),
            "DTOU32": lambda: to_int(D, "z", 32, True),
            "DTOI64": lambda: to_int(D, "z", 64, False),
            "DTOU64": lambda: to_int(D, "z", 64, True),
            "DLROUND": lambda: to_int(D, "a", 64, False),
            "DTOS": lambda: F.f_convert(x, D, S, mode),
            "DLT": lambda: int(F.f_cmp(y, x, D, "gt")),
            "DLE": lambda: int(F.f_cmp(y, x, D, "ge")),
            "DEQ": lambda: int(F.f_cmp(x, y, D, "eq")),
            "DUNORD": lambda: int(F.cls(x, D)[0] in ("qnan", "snan") or F.cls(y, D)[0] in ("qnan", "snan")),
            "SADD": lambda: F.f_add(x, y, S),
            "SSUB": lambda: F.f_sub(x, y, S),
            "SMUL": lambda: F.f_mul(x, y, S),
            "SDIV": lambda: F.f_div(x, y, S),
            "SSQRT": lambda: F.f_sqrt(x, S),
            "SFMA": lambda: F.f_fma(z, x, y, S),
            "SMIN": lambda: F.f_maxmin(x, y, S, False, True),
            "SMAX": lambda: F.f_maxmin(x, y, S, True, True),
            "SRINT": lambda: F.f_rint(x, S, mode),
            "STOI32": lambda: to_int(S, "z", 32, False),
            "STOD": lambda: F.f_convert(x, S, D),
            "I64TOD": lambda: F.f_from_int(x, 64, False, D, 0, mode),
            "U64TOD": lambda: F.f_from_int(x, 64, True, D, 0, mode),
            "I32TOS": lambda: F.f_from_int(x, 32, False, S, 0, mode),
            "U64TOS": lambda: F.f_from_int(x, 64, True, S, 0, mode),
        }[kind]()
        lines.append(f"{n:03} {kind} = {hex64(r)}\n")
    F.FZ = F.DN = False
    return "".join(lines)


def intrinsics():
    """guest/src/intrinsics.c: model each T/TS line's assembly with the lane
    models of gen-neon-vectors.py, on the program's own input vectors."""
    import importlib.util

    spec = importlib.util.spec_from_file_location("neonmodel", ROOT / "scripts" / "gen-neon-vectors.py")
    nm = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(nm)
    text = (SRC / "intrinsics.c").read_text()
    body = re.search(r"IN\[14\]\[2\] = \{(.*?)\};", text, re.S).group(1)
    pairs = re.findall(r"\{(0x[0-9a-f]+), (0x[0-9a-f]+)\}", body)
    vin = [int(lo, 16) | int(hi, 16) << 64 for lo, hi in pairs]
    x3 = int(re.search(r"X3 = (0x[0-9a-f]+);", text).group(1), 16)
    mem = b"".join(v.to_bytes(16, "little") for v in vin)

    def chunks(data):
        return "".join(" " + data[k:k + 16][::-1].hex() for k in range(0, len(data), 16))

    lines = []
    tests = re.findall(r'^\s*(TS?)\("([^"]+)", (\d+), (\d+), (\d+),', text, re.M)
    for n, (macro, asm, p1, p2, p3) in enumerate(tests):
        st = nm.State(0)
        nm.ST = st
        st.x[3] = x3
        if macro == "TS":
            a, b, size = int(p1), int(p2), int(p3)
            st.v[1:5] = [vin[a], vin[b], vin[(b + 1) % 14], vin[(b + 2) % 14]]
            st.mem = bytearray(len(st.mem))
            nm.model(asm)
            data = bytes(st.mem[:size])
        else:
            d, a, b = int(p1), int(p2), int(p3)
            st.v[0:5] = [vin[d], vin[a], vin[b], vin[(b + 1) % 14], vin[(b + 2) % 14]]
            st.mem[: len(mem)] = mem
            st.x[1] = nm.MEM_BASE + 16 * a
            nm.model(asm)
            first = asm.split(" ", 1)[1].split(",")[0].strip()
            if asm.startswith("ld"):
                regs = [int(r) for r in re.findall(r"v(\d+)", asm.split("}")[0])]
                data = b"".join(st.v[r].to_bytes(16, "little") for r in regs)
            else:
                m = re.match(r"v\d+\.(\d+)([bhsd])", first)
                if m:
                    size = 8 if m.group(1) + m.group(2) in ("8b", "4h", "2s", "1d") else 16
                elif first[0] == "v":
                    size = 16  # an element insert keeps the whole register
                else:
                    size = {"b": 1, "h": 2, "s": 4, "d": 8}[first[0]]
                data = st.v[0].to_bytes(16, "little")[:size]
        lines.append(f"{n:03} {asm} ={chunks(data)}\n")
    return "".join(lines)


def args(name):
    """The arguments a homebrew test program runs with: guest/src/nro/<name>.args,
    one per line (an empty line is an empty argument)."""
    lines = (SRC / "nro" / f"{name}.args").read_text().split("\n")
    if lines[-1] == "":
        lines.pop()
    return lines


def hbtest():
    """guest/src/nro/hbtest.c, started as an NRO the way Switchbrew's
    "Homebrew ABI" page describes, with argv[0] = the file name. The config
    keys are the ones kestrel provides (docs/ROADMAP.md, Phase 2 notes):
    MainThreadHandle 1, AppletType 7 (Application = 0), Argv 5, RandomSeed 14,
    then EndOfList 0. .bss starts zeroed, .data holds its initial value, and
    the five pointers in the tables are relocated."""
    argv = ["hbtest.nro"] + args("hbtest")
    out = ["homebrew test program", "config keys: 1 7(applet type 0) 5 14 0"]
    out += [f"argv[{i}] = [{a}]" for i, a in enumerate(argv)]
    out += [
        f"argc = {len(argv)}",
        "bss nonzero words: 0",
        "bss after write: 7",
        "data counter: 1234",
        "data counter after increment: 1235",
    ]
    out += [f"relocated string: {s}" for s in ("alpha", "beta", "gamma")]
    out += ["relocated call add(3, 4) = 7", "relocated call mul(3, 4) = 12"]
    return "".join(line + "\n" for line in out)


def hblayout():
    """guest/src/nro/hblayout.c: the properties of the svcGetInfo layout that
    Switchbrew documents. The regions fit the 39-bit address space ("SVC",
    AddressSpace64Bit) without overlapping; the alias and heap regions have
    the 39-bit default sizes ("16.0.0": 64 GiB and 8 GiB); a wrong handle is
    InvalidHandle 0xe401 and a wrong subtype InvalidCombination 0xe801
    ("Error codes"; the svcGetInfo section lists no result codes, so which
    result a wrong subtype gives is kestrel's choice); entropy has
    subtypes 0 to 3. With AppletType Application the process is an
    application, and no debugger is attached."""
    ok, bad_handle, bad_sub = hex64(0), hex64(0xE401), hex64(0xE801)
    out = [
        "homebrew layout test",
        "aslr region inside the 39-bit space: yes",
        "alias, heap and stack regions inside the aslr region: yes",
        "any two regions overlap: no",
        "alias region GiB: 64",
        "heap region GiB: 8",
        "image inside the aslr region, outside the others: yes",
        "stack pointer inside the stack region: yes",
        "heap at the start of the heap region: yes",
        "heap readable and writable: yes",
        "used memory grew by the heap size: yes",
        "used memory within total: yes",
    ]
    out += [f"entropy subtype {i}: {ok}" for i in range(4)]
    out += [
        f"entropy subtype 4: {bad_sub}",
        f"heap region with handle 0: {bad_handle}",
        f"heap region with subtype 1: {bad_sub}",
        f"debugger attached with the process handle: {bad_handle}",
        "debugger attached: 0",
        "is application: 1",
        "program id nonzero: yes",
    ]
    return "".join(line + "\n" for line in out)


def hbmemory():
    """guest/src/nro/hbmemory.c: the memory system calls as Switchbrew's
    "SVC" page documents them. Memory types are its MemoryType table (the
    4.0.0+ values): Code 0xdc7e03 for the image's read-only part, CodeData
    0x3febd04 for its data, Normal 0x37ebd05 for heap, Stack 0x5c3c0b for
    the svcMapMemory destination, Free 0 for unmapped memory. Result codes:
    0xca01 for a bad size, 0xd801 for a permission other than ---, r--, rw-,
    0xd401 for an svcUnmapMemory pairing that was never mapped, 0xdc01 for
    svcMapPhysicalMemory outside the alias region. The svcMapMemory source
    becomes --- with attribute bit 0 (Locked); Uncached is bit 3."""
    ok = hex64(0)
    code = {name: hex64(v) for name, v in
            (("size", 0xCA01), ("perm", 0xD801), ("state", 0xD401), ("range", 0xDC01))}
    free, code_t, data_t = hex64(0), hex64(0xDC7E03), hex64(0x3FEBD04)
    normal, stack = hex64(0x37EBD05), hex64(0x5C3C0B)
    out = [
        "homebrew memory test",
        f"image text: type {code_t} attr 0 r-x",
        f"image data: type {data_t} attr 0 rw-",
        f"unmapped address: type {free} attr 0 ---",
        f"heap 2 MiB: {ok}",
        f"heap: type {normal} attr 0 rw-",
        f"heap unaligned size: {code['size']}",
        f"heap over 4 GiB: {code['size']}",
        f"heap 4 MiB: {ok}",
        "growing kept the contents: yes",
        f"set r--: {ok}",
        f"heap page: type {normal} attr 0 r--",
        "only that page changed: yes",
        "still readable: yes",
        f"set ---: {ok}",
        f"set rw-: {ok}",
        f"set -w-: {code['perm']}",
        f"set r-x: {code['perm']}",
        f"set uncached: {ok}",
        f"heap page: type {normal} attr 8 rw-",
        f"clear uncached: {ok}",
        "other attribute rejected: yes",
        "heap is one block again: yes",
        f"map memory: {ok}",
        f"destination: type {stack} attr 0 rw-",
        f"source: type {normal} attr 1 ---",
        "destination holds the source bytes: yes",
        "map into the alias region rejected: yes",
        f"unmap with the wrong source: {code['state']}",
        f"unmap memory: {ok}",
        f"source after unmap: type {normal} attr 0 rw-",
        "write through the destination kept: yes",
        f"map physical: {ok}",
        f"physical block: {hex64(0x10000)} bytes, rw-",
        f"map a larger range over it: {ok}",
        "only the new part counted: yes",
        "physical memory writable: yes",
        f"map physical unaligned size: {code['size']}",
        f"map physical outside alias: {code['range']}",
        f"unmap physical: {ok}",
        f"after unmap: type {free} attr 0 ---",
        f"heap shrink to 2 MiB: {ok}",
        "shrinking kept the rest: yes",
    ]
    return "".join(line + "\n" for line in out)


def hbhandles():
    """guest/src/nro/hbhandles.c: handles and the first kernel objects as
    Switchbrew documents them. Result codes are named on its "Error codes"
    page: InvalidHandle 0xe401 for a closed handle, handle 0 or the wrong
    kind ("SVC": SignalEvent takes only the writable side; ClearEvent and
    GetThreadId the kinds they list), InvalidState 0xfa01 for ResetSignal
    on an unsignaled event ("SVC", ResetSignal). CreateTransferMemory makes
    the range take the given permission and attribute bit 0 (Locked) until
    its handle closes; locked memory cannot be reprotected
    (SetMemoryPermission: 0xd401) and the execute bit is refused. Results
    where the page is silent are kestrel's documented choices
    (docs/ROADMAP.md, phase 2 notes): 0xd801 for execute, 0xd401 for
    memory already lent or without FlagCanTransfer (the stack), 0xca01 for
    an unaligned size as in every memory call, and OutOfHandles 0xd201
    when the table is full."""
    ok, bad, unsignaled = hex64(0), hex64(0xE401), hex64(0xFA01)
    busy, perm, size, full = hex64(0xD401), hex64(0xD801), hex64(0xCA01), hex64(0xD201)
    normal = hex64(0x37EBD05)
    out = [
        "homebrew handle test",
        "main thread handle is a real handle: yes",
        f"thread id of the main thread handle: {ok}",
        f"thread id of the current thread: {ok}",
        "both name the same thread: yes",
        f"process id of the current process: {ok}",
        f"thread id of handle 0: {bad}",
        f"close handle 0: {bad}",
        f"create event: {ok}",
        "two different handles: yes",
        f"reset an unsignaled event: {unsignaled}",
        f"signal through the readable side: {bad}",
        f"signal: {ok}",
        f"reset: {ok}",
        f"reset again: {unsignaled}",
        f"signal: {ok}",
        f"clear through the readable side: {ok}",
        f"reset after clearing: {unsignaled}",
        f"clear a thread handle: {bad}",
        f"close the writable side: {ok}",
        f"signal through the closed handle: {bad}",
        f"clear through the readable side: {ok}",
        f"close the readable side: {ok}",
        f"close it again: {bad}",
        f"heap 2 MiB: {ok}",
        f"lend 8 KiB read-only: {ok}",
        f"lent: type {normal} attr 1 r--",
        "lent block is 8 KiB: yes",
        "still readable: yes",
        f"reprotect lent memory: {busy}",
        f"lend it again: {busy}",
        f"lend with execute: {perm}",
        f"lend an unaligned size: {size}",
        f"lend the stack: {busy}",
        f"close the transfer memory: {ok}",
        f"returned: type {normal} attr 0 rw-",
        "heap is one block again: yes",
        "writable again: yes",
        f"create events until the table is full: {full}",
        "closed every event handle: yes",
        f"create event after closing them: {ok}",
        "old handles stay closed: yes",
        f"close the main thread handle: {ok}",
        f"thread id of the current thread: {ok}",
        "unchanged: yes",
    ]
    return "".join(line + "\n" for line in out)


def hbthreads():
    """guest/src/nro/hbthreads.c: threads as Switchbrew's "SVC" page and
    the scheduling rules every multi-core console honours describe them.
    Result codes are named on the "Error codes" page: InvalidPriority 0xe001
    outside 0 to 0x3F (SetThreadPriority), InvalidCoreId 0xe201 for a
    processor other than 0 to 3 or -2 (CreateThread). A thread starts with
    its argument in X0, SP at its stack top and its own thread local region,
    which the kernel hands out zeroed ("Thread Local Region"). On one core a
    higher priority runs at once and a lower one only when the higher one
    sleeps; yielding (SleepThread 0) only lets equal priorities run. Two
    threads on different cores run side by side, so neither spinning nor
    atomic updates lose progress. The tick runs at 19.2 MHz (GetSystemTick).
    Kestrel's documented choices (docs/ROADMAP.md, phase 2 notes): the main
    thread runs on core 0, which ProcessorId -2 also picks; starting a
    thread twice is InvalidState 0xfa01; SetThreadCoreMask gives
    InvalidCombination 0xe801 for an ideal core outside the mask and
    InvalidCoreId for a core above 3."""
    ok = hex64(0)
    prio, core_id = hex64(0xE001), hex64(0xE201)
    state, combination = hex64(0xFA01), hex64(0xE801)
    out = [
        "homebrew thread test",
        f"create with priority 0x40: {prio}",
        f"create with priority -1: {prio}",
        f"create on processor 4: {core_id}",
        f"create on processor -1: {core_id}",
        f"create a worker on core 1: {ok}",
        "not running before it is started: yes",
        f"start it: {ok}",
        f"start it again: {state}",
        "worker got its argument: yes",
        "worker started on its stack: yes",
        "worker has its own thread local region: yes",
        "its thread local region started zeroed: yes",
        "worker's processor: 1",
        f"thread id of the exited worker: {ok}",
        "matches what the worker saw: yes",
        "differs from the main thread's: yes",
        f"close the worker: {ok}",
        "main thread's processor: 0",
        f"create a higher priority thread on this core: {ok}",
        f"create a lower priority thread on this core: {ok}",
        # H runs before StartThread returns; L waits through the yield (Y)
        # and runs while the main thread sleeps (before S).
        "run order: HMMYLS",
        f"create a thread on core 3: {ok}",
        f"get priority: {ok}",
        f"priority: {hex64(0x30)}",
        f"set priority 0x3f: {ok}",
        f"priority now: {hex64(0x3F)}",
        f"set priority 0x40: {prio}",
        f"get core mask: {ok}",
        "ideal core: 3",
        f"affinity mask: {hex64(0b1000)}",
        f"set core 2 in mask 0b0110: {ok}",
        "ideal core now: 2",
        f"affinity mask now: {hex64(0b0110)}",
        f"set core 1 in mask 0b0100: {combination}",
        f"set core 4: {core_id}",
        f"close it unstarted: {ok}",
        f"create a spinner on core 2: {ok}",
        "main and spinner both ran without yielding: yes",
        f"atomic counter from two threads: {2 * 20000}",
        "sleeping 1 ms advanced the tick by at least 19200: yes",
    ]
    return "".join(line + "\n" for line in out)


def hbsync():
    """guest/src/nro/hbsync.c: synchronisation as Switchbrew's "SVC" page
    documents WaitSynchronization (at most 0x40 handles, else 0xee01; an
    unreadable list is 0xe601; the pseudo-handles and objects that cannot
    be waited on are InvalidHandle 0xe401; TimedOut 0xea01; Cancelled
    0xec01; readable events signal while signaled and threads once they
    exit; the index is the signaled handle's) and CancelSynchronization (it
    ends a wait, or makes the next one return Cancelled). The mutex and
    condition variable follow libnx's protocol, so locked increments from
    four threads are never lost, the mutex word ends at 0, and a producer
    and consumer pass every item; a condition variable wait that times out
    returns 0xea01 with the mutex released. Address arbitration follows the
    ArbitrationType and SignalType names: WaitIfLessThan waits only while
    the word is less, SignalAndIncrementIfEqual adds 1 when the word
    matches. Kestrel's documented choices (docs/ROADMAP.md, phase 2
    notes): InvalidState 0xfa01 when an arbitration condition does not
    hold, TimedOut at once for timeout 0, InvalidEnumValue 0xf001 for an
    unknown type."""
    ok = hex64(0)
    out = [
        "homebrew synchronisation test",
        f"wait on 0x41 handles: {hex64(0xEE01)}",
        f"wait with an unreadable list: {hex64(0xE601)}",
        f"wait on the current thread pseudo-handle: {hex64(0xE401)}",
        f"wait on the writable side: {hex64(0xE401)}",
        f"poll an unsignaled event: {hex64(0xEA01)}",
        f"wait 1 ms on it: {hex64(0xEA01)}",
        "the tick advanced by at least 19200: yes",
        f"wait on two, the second signaled: {ok}",
        "index: 1",
        f"again, it stays signaled: {ok}",
        "the waiter is still waiting: yes",
        f"wait for the waiter to exit: {ok}",
        f"the waiter's result: {ok}",
        "the waiter's index: 0",
        f"cancel the current thread: {ok}",
        f"its next wait: {hex64(0xEC01)}",
        f"the wait after that: {hex64(0xEA01)}",
        f"cancel the waiting thread: {ok}",
        f"its wait ended with: {hex64(0xEC01)}",
        f"counter after 4 threads x 2000 locked increments: {4 * 2000}",
        f"mutex word afterwards: {ok}",
        "items consumed: 100",
        f"sum of the items: {sum(range(1, 101))}",
        f"wait 1 ms on a condition variable: {hex64(0xEA01)}",
        "the mutex was released: yes",
        f"with timeout 0: {hex64(0xEA01)}",
        "released again: yes",
        f"wait while 0 < 0: {hex64(0xFA01)}",
        f"wait while 0 < 1, no time: {hex64(0xEA01)}",
        f"unknown arbitration type: {hex64(0xF001)}",
        "the address waiter is still waiting: yes",
        f"signal and increment if 5: {hex64(0xFA01)}",
        f"signal and increment if 0: {ok}",
        "the word now: 1",
        f"the address waiter's result: {ok}",
    ]
    return "".join(line + "\n" for line in out)


def hbipc():
    """guest/src/nro/hbipc.c: IPC with the service manager. Switchbrew's
    "Services API" page: sm: RegisterClient (command 0) takes the process ID
    descriptor, GetServiceHandle (1) a zero-padded 8-byte name; from 12.0.0
    the same commands also come over TIPC as command types 16 and 17. Result
    names from the "Error codes" page: InvalidClient 0x415, InvalidServiceName
    0xc15 (module 21), SessionClosed 0xf601, InvalidHandle 0xe401,
    InvalidPointer 0xe601, OutOfRange 0xee01 (module 1). A reply goes to the
    thread that sent the request ("IPC Marshalling": the message lives in the
    sender's thread local region), and registering is per session. Kestrel's
    documented choices (docs/ROADMAP.md, phase 2 notes): InvalidClient for
    GetServiceHandle before RegisterClient; InvalidServiceName for an empty
    name or characters after the padding; an unreadable port name is
    InvalidPointer and one with no NUL in 12 bytes OutOfRange; a close
    request succeeds and later requests on the session give SessionClosed."""
    ok = hex64(0)

    def svc(code):
        return f"{ok}, service result {hex64(code)}"

    out = [
        "homebrew IPC test",
        f"connect with an unreadable name: {hex64(0xE601)}",
        f"connect with a 12-character name: {hex64(0xEE01)}",
        f"connect to sm: {ok}",
        "the session handle is a real handle: yes",
        f"get a service before registering: {svc(0x415)}",
        f"register: {svc(0)}",
        "the reply moved no handles: yes",
        f"get a service with an empty name: {svc(0xC15)}",
        f"get a service with a gap in its name: {svc(0xC15)}",
        f"connect to sm: again: {ok}",
        "a different handle: yes",
        f"TIPC get a service before registering: {svc(0x415)}",
        f"create a thread: {ok}",
        f"wait for it: {ok}",
        f"TIPC register from that thread: {ok}",
        f"its service result: {ok}",
        "this thread's region is untouched: yes",
        f"TIPC get a service with an empty name: {svc(0xC15)}",
        f"close request: {ok}",
        f"register after the close request: {hex64(0xF601)}",
        f"close the handle: {ok}",
        f"register on the closed handle: {hex64(0xE401)}",
        f"request on the current thread pseudo-handle: {hex64(0xE401)}",
        f"the first session still works: {svc(0xC15)}",
        f"close it: {ok}",
    ]
    return "".join(line + "\n" for line in out)


PROGRAMS = {
    "hello": hello,
    "asm_checks": asm_checks,
    "vm": vm,
    "bigint": bigint,
    "algos": algos,
    "sha256": sha256,
    "fp": fp,
    "intrinsics": intrinsics,
    "hbtest": hbtest,
    "hblayout": hblayout,
    "hbmemory": hbmemory,
    "hbhandles": hbhandles,
    "hbthreads": hbthreads,
    "hbsync": hbsync,
    "hbipc": hbipc,
}


def main():
    check = "--check" in sys.argv[1:]
    sources = {p.stem for p in SRC.glob("*.c") if p.stem != "start"}
    sources |= {p.stem for p in SRC.glob("*.S")}
    sources |= {p.stem for p in SRC.glob("nro/*.c")}
    if sources != set(PROGRAMS):
        missing = sorted(sources - set(PROGRAMS))
        extra = sorted(set(PROGRAMS) - sources)
        sys.exit(f"models out of sync with guest/src: missing {missing}, extra {extra}")
    stale = []
    for name, model in PROGRAMS.items():
        path = OUT / f"{name}.txt"
        want = model().encode()
        if check:
            if not path.exists() or path.read_bytes() != want:
                stale.append(name)
        else:
            path.write_bytes(want)
            print(f"wrote {path.relative_to(ROOT)}")
    if stale:
        print("expected output differs from the independent model: " + ", ".join(stale))
        sys.exit(1)
    if check:
        print(f"all {len(PROGRAMS)} expected files match the independent model")


if __name__ == "__main__":
    main()
