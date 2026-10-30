// A homebrew-format (NRO) test program for the time service (Switchbrew,
// "Glue services", time:u): the requests libnx's timeInitialize sends,
// laid out as libnx lays them out, the clocks read from the shared memory
// the way libnx reads them (`_timeReadClockFromSharedMem`, with the tick
// count from CNTPCT_EL0), and the calendar time __libnx_init_time turns
// into the TZ variable. The output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u
#define SFCI 0x49434653u
#define SFCO 0x4f434653u
#define SHMEM_SIZE 0x1000ull

typedef struct {
    u64 base, size;
    u32 type, attr, perm, ipc_refs, device_refs, padding;
} memory_info;

// ---- system calls ---------------------------------------------------------

#define SVC_HANDLE(name, n)                                                  \
    static u64 name(u64 h) {                                                 \
        register u64 x0 __asm__("x0") = h;                                   \
        __asm__ volatile("svc #" #n : "+r"(x0) : : "memory");                \
        return x0;                                                           \
    }
SVC_HANDLE(close_handle, 0x16)
SVC_HANDLE(send_sync_request, 0x21)

static void sleep_ns(u64 ns) {
    register u64 x0 __asm__("x0") = ns;
    __asm__ volatile("svc #0x0b" : "+r"(x0) : : "memory");
}

static u64 connect(u64 *h, const char *name) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)name;
    __asm__ volatile("svc #0x1f" : "=r"(x0), "+r"(x1) : : "memory");
    *h = x1;
    return x0;
}

static u64 map_shmem(u64 h, u64 addr, u64 size, u64 perm) {
    register u64 x0 __asm__("x0") = h;
    register u64 x1 __asm__("x1") = addr;
    register u64 x2 __asm__("x2") = size;
    register u64 x3 __asm__("x3") = perm;
    __asm__ volatile("svc #0x13" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

static u64 unmap_shmem(u64 h, u64 addr, u64 size) {
    register u64 x0 __asm__("x0") = h;
    register u64 x1 __asm__("x1") = addr;
    register u64 x2 __asm__("x2") = size;
    __asm__ volatile("svc #0x14" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory");
    return x0;
}

static memory_info query(u64 addr) {
    memory_info m;
    register u64 x0 __asm__("x0") = (u64)&m;
    register u64 x1 __asm__("x1");
    register u64 x2 __asm__("x2") = addr;
    __asm__ volatile("svc #0x06" : "+r"(x0), "=r"(x1), "+r"(x2) : : "memory");
    if (x0 != 0) fail("svcQueryMemory failed");
    return m;
}

static u64 info(u32 kind) {
    u64 v;
    if (svc_get_info(&v, kind, CUR_PROCESS, 0) != 0) fail("svcGetInfo failed");
    return v;
}

// libnx's armGetSystemTick.
static u64 system_tick(void) {
    u64 v;
    __asm__ volatile("mrs %0, cntpct_el0" : "=r"(v));
    return v;
}

// ---- messages ---------------------------------------------------------------

static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

// A CMIF request as libnx's cmifMakeRequest lays it out, with an optional
// process ID and no handles or buffers.
static void request(int pid, u32 cmd, const void *args, u32 size) {
    volatile u32 *m = msg();
    m[0] = 4;
    m[1] = (((16 + 16 + size + 1) & ~1u) + 3) / 4 | (pid ? 1u << 31 : 0);
    u32 at = 2;
    if (pid) {
        m[at++] = 1;
        m[at++] = 0;
        m[at++] = 0;
    }
    while (at % 4) m[at++] = 0;
    m[at] = SFCI;
    m[at + 1] = 0;
    m[at + 2] = cmd;
    m[at + 3] = 0;
    volatile u8 *data = (volatile u8 *)&m[at + 4];
    for (u32 i = 0; i < size; i++) data[i] = ((const u8 *)args)[i];
}

static u32 copied, moved;
static volatile u8 *out;

// The reply, as libnx's cmifParseResponse reads it outside a domain.
static u32 call_with(u64 h, int pid, u32 cmd, const void *args, u32 size) {
    request(pid, cmd, args, size);
    if (send_sync_request(h) != 0) fail("svcSendSyncRequest failed");
    volatile u32 *m = msg();
    u32 at = 2;
    copied = moved = 0;
    if (m[1] >> 31) {
        u32 special = m[2], ncopy = special >> 1 & 0xf, nmove = special >> 5 & 0xf;
        at = 3;
        if (ncopy) copied = m[at];
        at += ncopy;
        if (nmove) moved = m[at];
        at += nmove;
    }
    while (at % 4) at++;
    if (m[at] != SFCO) fail("no SFCO header in the reply");
    out = (volatile u8 *)&m[at + 4];
    return m[at + 2];
}

static u32 call(u64 h, u32 cmd, const void *args, u32 size) {
    return call_with(h, 0, cmd, args, size);
}

static u64 out_u64(u32 at) {
    u64 v = 0;
    for (int i = 7; i >= 0; i--) v = v << 8 | out[at + i];
    return v;
}

static u32 out_u32(u32 at) { return (u32)(out_u64(at) & 0xffffffffu); }

// ---- output -----------------------------------------------------------------

static void hex(const char *label, u64 v) {
    puts_(label);
    put_hex(v);
    puts_("\n");
}

static void dec(const char *label, u64 v) {
    puts_(label);
    put_u64(v);
    puts_("\n");
}

static void yes_no(const char *label, int v) {
    puts_(label);
    puts_(v ? "yes\n" : "no\n");
}

static void two_digits(u32 v) {
    char s[3] = {(char)('0' + v / 10 % 10), (char)('0' + v % 10), 0};
    puts_(s);
}

static u64 get_service(u64 sm, const char *name) {
    u8 n[8] = {0};
    for (int i = 0; i < 8 && name[i]; i++) n[i] = (u8)name[i];
    if (call(sm, 1, n, 8) != 0 || !moved) fail("GetServiceHandle failed");
    return moved;
}

static int overlaps(u64 a, u64 a_size, u64 b, u64 b_size) {
    return a < b + b_size && b < a + a_size;
}

// The first free block of `size` bytes in the address space outside the
// heap and alias regions, where libnx's virtmemFindAslr would look.
static u64 find_free(u64 size) {
    u64 heap = info(4), heap_size = info(5), alias = info(2), alias_size = info(3);
    u64 at = info(12), end = at + info(13);
    while (at < end) {
        memory_info m = query(at);
        u64 top = m.base + m.size;
        if (m.type == 0) {
            u64 a = at;
            while (a + size <= top) {
                if (overlaps(a, size, heap, heap_size)) a = heap + heap_size;
                else if (overlaps(a, size, alias, alias_size)) a = alias + alias_size;
                else return a;
            }
        }
        at = top;
    }
    fail("no free block for the shared memory");
    return 0;
}

// ---- shared memory, as libnx reads it ----------------------------------------

static volatile u8 *shmem;

// _timeReadSharedmemObj: a u32 counter, then two copies of the object; the
// counter's low bit picks one, and a changed counter means read again.
static void read_obj(void *dst, u32 offset, u32 size) {
    volatile u32 *counter = (volatile u32 *)(shmem + offset);
    volatile u8 *data = shmem + offset + 8;
    u32 cur;
    do {
        cur = *counter;
        for (u32 i = 0; i < size; i++) ((u8 *)dst)[i] = data[(cur & 1) * size + i];
        __asm__ volatile("dmb ishld" : : : "memory");
    } while (cur != *counter);
}

typedef struct {
    i64 base_time;
    u8 source_id[16];
} steady_type;

typedef struct {
    i64 offset;
    i64 time_point;
    u8 source_id[16];
} clock_context;

static int same_id(const u8 *a, const volatile u8 *b) {
    for (int i = 0; i < 16; i++)
        if (a[i] != b[i]) return 0;
    return 1;
}

// _timeComputeSteadyClockTimePoint, with armTicksToNs.
static i64 steady_point(const steady_type *s) {
    return (s->base_time + (i64)((system_tick() * 625) / 12)) / 1000000000;
}

// ---- the program ------------------------------------------------------------

static u64 now(u64 clock) {
    if (call(clock, 0, 0, 0) != 0) fail("GetCurrentTime failed");
    return out_u64(0);
}

static const i64 calendar_inputs[] = {0, -1, 951825845, 1749081600, 4102444799, 253402300799};

// ToCalendarTimeWithMyRule, printed as a date and the additional info.
static void calendar(u64 zone, i64 t) {
    u32 rc = call(zone, 101, &t, 8);
    puts_("ToCalendarTimeWithMyRule ");
    if (t < 0) {
        puts_("-");
        put_u64((u64)-t);
    } else {
        put_u64((u64)t);
    }
    puts_(": ");
    if (rc != 0) {
        hex("", rc);
        return;
    }
    put_u64(out[0] | out[1] << 8);
    puts_("-");
    two_digits(out[2]);
    puts_("-");
    two_digits(out[3]);
    puts_(" ");
    two_digits(out[4]);
    puts_(":");
    two_digits(out[5]);
    puts_(":");
    two_digits(out[6]);
    puts_(" day ");
    put_u64(out_u32(8));
    puts_(" of the week, ");
    put_u64(out_u32(12));
    puts_(" of the year, ");
    char name[9] = {0};
    for (int i = 0; i < 8; i++) name[i] = (char)out[16 + i];
    puts_(name);
    puts_(", DST ");
    put_u64(out_u32(24));
    puts_(", offset ");
    put_u64(out_u32(28));
    puts_("\n");
}

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew time test\n");

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call_with(sm, 1, 0, &zero, 8));
    u64 time = get_service(sm, "time:u");

    // timeInitialize: the four clocks and the time zone service, then the
    // shared memory.
    static const char *const names[] = {
        "GetStandardUserSystemClock: ",  "GetStandardNetworkSystemClock: ",
        "GetStandardSteadyClock: ",      "GetTimeZoneService: ",
        "GetStandardLocalSystemClock: ",
    };
    u64 sessions[5];
    int all_moved = 1;
    for (u32 n = 0; n < 5; n++) {
        hex(names[n], call(time, n, 0, 0));
        sessions[n] = moved;
        all_moved &= moved != 0;
    }
    yes_no("each moved a session handle: ", all_moved);
    u64 user = sessions[0], network = sessions[1], steady = sessions[2], zone = sessions[3],
        local = sessions[4];
    hex("GetSharedMemoryNativeHandle: ", call(time, 20, 0, 0));
    u64 handle = copied;
    yes_no("it copied a handle: ", handle != 0);
    u64 at = find_free(SHMEM_SIZE);
    hex("map read-write: ", map_shmem(handle, at, SHMEM_SIZE, 3));
    hex("map read-only: ", map_shmem(handle, at, SHMEM_SIZE, 1));
    memory_info mi = query(at);
    hex("memory type: ", mi.type);
    hex("permission: ", mi.perm);
    shmem = (volatile u8 *)at;

    steady_type s;
    clock_context local_ctx, network_ctx;
    read_obj(&s, 0x00, sizeof s);
    read_obj(&local_ctx, 0x38, sizeof local_ctx);
    read_obj(&network_ctx, 0x80, sizeof network_ctx);
    dec("steady clock base time: ", (u64)s.base_time);
    dec("local clock epoch: ", (u64)local_ctx.offset);
    dec("local clock time point: ", (u64)local_ctx.time_point);
    dec("network clock epoch: ", (u64)network_ctx.offset);
    dec("network clock time point: ", (u64)network_ctx.time_point);
    yes_no("the source IDs match: ", same_id(s.source_id, local_ctx.source_id) &&
                                         same_id(s.source_id, network_ctx.source_id));
    // _timeReadClockFromSharedMem: the epoch plus the steady clock.
    dec("time from the shared memory: ", (u64)(local_ctx.offset + steady_point(&s)));

    dec("user clock GetCurrentTime: ", now(user));
    dec("network clock GetCurrentTime: ", now(network));
    dec("local clock GetCurrentTime: ", now(local));
    hex("GetSystemClockContext: ", call(user, 2, 0, 0));
    dec("its epoch: ", out_u64(0));
    dec("its time point: ", out_u64(8));
    yes_no("its source ID is the steady clock's: ", same_id(s.source_id, out + 16));
    hex("GetCurrentTimePoint: ", call(steady, 0, 0, 0));
    dec("its time point: ", out_u64(0));
    yes_no("its source ID is the steady clock's: ", same_id(s.source_id, out + 8));

    hex("GetDeviceLocationName: ", call(zone, 0, 0, 0));
    char name[0x25] = {0};
    for (int i = 0; i < 0x24; i++) name[i] = (char)out[i];
    puts_("location: ");
    puts_(name);
    puts_("\n");
    for (u32 i = 0; i < sizeof calendar_inputs / sizeof calendar_inputs[0]; i++)
        calendar(zone, calendar_inputs[i]);

    // __libnx_init_time: the boot time's calendar info becomes TZ.
    i64 boot = (i64)now(user);
    if (call(zone, 101, &boot, 8) != 0) fail("ToCalendarTimeWithMyRule failed");
    char tz[64] = {0}, *p = tz;
    int offset = (int)out_u32(28);
    int west = offset < 0;
    if (west) offset = -offset;
    *p++ = '<';
    for (int i = 0; i < 7 && out[16 + i]; i++) *p++ = (char)out[16 + i];
    *p++ = '>';
    *p++ = west ? '+' : '-';
    u32 parts[3] = {(u32)offset / 3600 % 24, (u32)offset / 60 % 60, (u32)offset % 60};
    for (int i = 0; i < 3; i++) {
        *p++ = (char)('0' + parts[i] / 10);
        *p++ = (char)('0' + parts[i] % 10);
        if (i < 2) *p++ = ':';
    }
    puts_("TZ: ");
    puts_(tz);
    puts_("\n");

    // The clocks run with the process clock.
    sleep_ns(2000000000ull);
    dec("after sleeping 2 seconds, user clock: ", now(user));
    read_obj(&s, 0x00, sizeof s);
    dec("time from the shared memory: ", (u64)(local_ctx.offset + steady_point(&s)));
    call(steady, 0, 0, 0);
    dec("steady clock time point: ", out_u64(0));

    hex("unmap: ", unmap_shmem(handle, at, SHMEM_SIZE));
    hex("close the shared memory: ", close_handle(handle));
    u32 closed = 0;
    for (int i = 0; i < 5; i++) closed |= (u32)close_handle(sessions[i]);
    hex("close the clocks: ", closed);
    hex("close time:u: ", close_handle(time));
    hex("close sm: ", close_handle(sm));
}
