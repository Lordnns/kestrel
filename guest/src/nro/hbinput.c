// A homebrew-format (NRO) test program for controller input (Switchbrew,
// "HID services": SharedMemoryFormat, NpadSharedMemoryEntry, the npad
// rings and their states). It sets up hid as libnx's hidInitialize and
// padConfigureInput do, maps the shared memory, and reads player 1 the way
// libnx's padUpdate and hidGetNpadStates read it (`pad.c`, `hid.c`). The
// self-test drives it with a scripted Pro Controller (ScriptedPad in
// crates/kestrel-testsuite). The output is modelled in
// scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u
#define SFCI 0x49434653u
#define SFCO 0x4f434653u
#define SHMEM_SIZE 0x40000ull

// NpadSharedMemoryFormat and NpadInternalState offsets.
#define NPAD 0x9a00
#define NPAD_ENTRY 0x5000
#define HANDHELD 8
#define STYLE_SET 0x0
#define FULL_KEY_COLOR 0x8
#define FULL_KEY_LIFO 0x28
#define DEVICE_TYPE 0x4188
#define SYSTEM_PROPERTIES 0x4190
#define FOOTER_TYPE 0x41ac
#define LIFO_ENTRIES 17

typedef struct {
    u64 base, size;
    u32 type, attr, perm, ipc_refs, device_refs, padding;
} memory_info;

// HidNpadCommonState.
typedef struct {
    u64 sampling_number;
    u64 buttons;
    int lx, ly, rx, ry;
    u32 attributes, reserved;
} npad_state;

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

static u64 process_id(void) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = CUR_PROCESS;
    __asm__ volatile("svc #0x24" : "=r"(x0), "+r"(x1) : : "memory");
    return x1;
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

// ---- messages ---------------------------------------------------------------

static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

typedef struct {
    int pid;        // send the process ID
    u64 ptr;        // pointer buffer (X) 0, or 0
    u32 ptr_size;
} options;

// A CMIF request as libnx's cmifMakeRequest lays it out.
static void request(options o, u32 cmd, const void *args, u32 size) {
    volatile u32 *m = msg();
    u32 raw = (((16 + 16 + size + 1) & ~1u) + 3) / 4;
    m[0] = 4 | (o.ptr ? 1u << 16 : 0);
    m[1] = raw | (o.pid ? 1u << 31 : 0);
    u32 at = 2;
    if (o.pid) {
        m[at++] = 1;
        m[at++] = 0;
        m[at++] = 0;
    }
    if (o.ptr) {
        m[at++] = (u32)(o.ptr >> 36) << 6 | ((u32)(o.ptr >> 32) & 0xf) << 12 | o.ptr_size << 16;
        m[at++] = (u32)o.ptr;
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

// The reply, as libnx's cmifParseResponse reads it outside a domain.
static u32 call(u64 h, options o, u32 cmd, const void *args, u32 size) {
    request(o, cmd, args, size);
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
    return m[at + 2];
}

// ---- output -----------------------------------------------------------------

static void hex(const char *label, u64 v) {
    puts_(label);
    put_hex(v);
    puts_("\n");
}

static void sdec(const char *label, int v) {
    puts_(label);
    if (v < 0) {
        puts_("-");
        v = -v;
    }
    put_u64((u64)v);
    puts_("\n");
}

static void yes_no(const char *label, int v) {
    puts_(label);
    puts_(v ? "yes\n" : "no\n");
}

static u64 get_service(u64 sm, const char *name) {
    u8 n[8] = {0};
    for (int i = 0; i < 8 && name[i]; i++) n[i] = (u8)name[i];
    options o = {0};
    if (call(sm, o, 1, n, 8) != 0 || !moved) fail("GetServiceHandle failed");
    return moved;
}

static int overlaps(u64 a, u64 a_size, u64 b, u64 b_size) {
    return a < b + b_size && b < a + a_size;
}

// The first free block of `size` bytes outside the heap and alias regions,
// where libnx's virtmemFindAslr would look.
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

// ---- the shared memory, as libnx reads it -----------------------------------

static volatile u8 *shm;

static u64 rd64(u32 at) { return *(volatile u64 *)(shm + at); }
static u32 rd32(u32 at) { return *(volatile u32 *)(shm + at); }

// libnx's _hidGetStates for an npad ring: up to `count` states, newest
// first, retrying when a sample changes under it or the sampling numbers
// are not consecutive.
static u32 get_states(u32 lifo, npad_state *states, u32 count) {
    int total = (int)rd64(lifo + 0x18);
    if (total < 0) total = 0;
    if (total > (int)count) total = (int)count;
    int tail = (int)rd64(lifo + 0x10);
    for (int i = 0; i < total; i++) {
        int pos = ((tail + (LIFO_ENTRIES + 1)) - total + i) % LIFO_ENTRIES;
        u32 entry = lifo + 0x20 + (u32)pos * 0x30;
        npad_state *out = &states[total - i - 1];
        u64 n0 = rd64(entry);
        for (u32 b = 0; b < sizeof(npad_state); b++) ((u8 *)out)[b] = shm[entry + 8 + b];
        u64 n1 = rd64(entry);
        if (n0 != n1 ||
            (i > 0 && out->sampling_number - states[total - i].sampling_number != 1)) {
            int now = (int)rd64(lifo + 0x18);
            now = total < now ? now : total;
            total = now < (int)count ? now : (int)count;
            tail = (int)rd64(lifo + 0x10);
            i = -1;
        }
    }
    return (u32)total;
}

static npad_state states[LIFO_ENTRIES];

static void show_state(const npad_state *s) {
    hex("  buttons: ", s->buttons);
    sdec("  left stick x: ", s->lx);
    sdec("  left stick y: ", s->ly);
    sdec("  right stick x: ", s->rx);
    sdec("  right stick y: ", s->ry);
    hex("  attributes: ", s->attributes);
}

static const u32 npad_ids[] = {0, 1, 2, 3, 4, 5, 6, 7, 0x20};

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew input test\n");
    u64 aruid = process_id();
    options none = {0}, pid = {.pid = 1};

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call(sm, pid, 0, &zero, 8));
    u64 hid = get_service(sm, "hid");

    // hidInitialize: the applet resource and its shared memory.
    hex("CreateAppletResource: ", call(hid, pid, 0, &aruid, 8));
    u64 resource = moved;
    hex("GetSharedMemoryHandle: ", call(resource, none, 0, 0, 0));
    u64 handle = copied;
    u64 at = find_free(SHMEM_SIZE);
    hex("map: ", map_shmem(handle, at, SHMEM_SIZE, 1));
    shm = (volatile u8 *)at;

    // Before padConfigureInput nothing is connected.
    sleep_ns(10000000);
    hex("player 1 style set before setup: ", rd32(NPAD + STYLE_SET));

    // padConfigureInput(1, standard styles).
    struct {
        u32 value, pad;
        u64 aruid;
    } in = {5, 0, aruid};
    hex("ActivateNpadWithRevision 5: ", call(hid, pid, 109, &in, sizeof in));
    options ids = {.pid = 1, .ptr = (u64)npad_ids, .ptr_size = sizeof npad_ids};
    hex("SetSupportedNpadIdType: ", call(hid, ids, 102, &aruid, 8));
    in.value = 0x1f;
    hex("SetSupportedNpadStyleSet: ", call(hid, pid, 100, &in, sizeof in));

    // A few samples later, player 1 is a Pro Controller.
    sleep_ns(20000000);
    u32 p1 = NPAD;
    hex("player 1 style set: ", rd32(p1 + STYLE_SET));
    hex("joy assignment mode: ", rd32(p1 + STYLE_SET + 4));
    hex("device type: ", rd32(p1 + DEVICE_TYPE));
    hex("system properties: ", rd64(p1 + SYSTEM_PROPERTIES));
    hex("applet footer type: ", shm[p1 + FOOTER_TYPE]);
    hex("full key colour attribute: ", rd32(p1 + FULL_KEY_COLOR));
    hex("handheld style set: ", rd32(NPAD + HANDHELD * NPAD_ENTRY + STYLE_SET));
    hex("ring buffer count: ", rd64(p1 + FULL_KEY_LIFO + 8));

    // hidGetNpadStatesFullKey(No1, states, 1): the newest state.
    u32 lifo = p1 + FULL_KEY_LIFO;
    hex("states read: ", get_states(lifo, states, 1));
    puts_("newest state:\n");
    show_state(&states[0]);
    yes_no("its sampling numbers agree: ", rd64(lifo + 0x20 + rd64(lifo + 0x10) * 0x30) ==
                                               states[0].sampling_number);

    // Every state in the ring, newest first, numbered one apart.
    u32 n = get_states(lifo, states, LIFO_ENTRIES);
    int consecutive = n > 1;
    for (u32 i = 1; i < n; i++)
        consecutive &= states[i - 1].sampling_number == states[i].sampling_number + 1;
    yes_no("more than one state, numbered one apart: ", consecutive);

    // Wait for the ninth sample, when the script lets go.
    while (get_states(lifo, states, 1) == 0 || states[0].sampling_number < 8) sleep_ns(5000000);
    puts_("after the script lets go:\n");
    show_state(&states[0]);

    hex("unmap: ", unmap_shmem(handle, at, SHMEM_SIZE));
    hex("close the shared memory: ", close_handle(handle));
    hex("close IAppletResource: ", close_handle(resource));
    hex("close hid: ", close_handle(hid));
    hex("close sm: ", close_handle(sm));
}
