// A homebrew-format (NRO) test program for /dev/nvhost-ctrl (Switchbrew,
// "NV services"): the syncpoints and their events, sent the way libnx's
// nvInitialize, nvFenceInit and nvFenceWait send them (`services/nv.c`,
// `nvidia/fence.c`, `nvidia/ioctl/nvhost-ctrl.c`): an event slot
// registered and queried, a wait armed on a syncpoint the program then
// increments itself (no GPU runs), and the timeout path that clears the
// wait. The expected output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u

typedef unsigned short u16;
#define SFCI 0x49434653u
#define SFCO 0x4f434653u

// ---- system calls ---------------------------------------------------------

static u64 send_sync_request(u64 h) {
    register u64 x0 __asm__("x0") = h;
    __asm__ volatile("svc #0x21" : "+r"(x0) : : "memory");
    return x0;
}

static u64 close_handle(u64 h) {
    register u64 x0 __asm__("x0") = h;
    __asm__ volatile("svc #0x16" : "+r"(x0) : : "memory");
    return x0;
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

static u64 set_heap_size(u64 size, u64 *addr) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = size;
    __asm__ volatile("svc #0x01" : "=r"(x0), "+r"(x1) : : "memory");
    *addr = x1;
    return x0;
}

static u64 create_transfer_memory(u64 *h, u64 addr, u64 size, u64 perm) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = addr;
    register u64 x2 __asm__("x2") = size;
    register u64 x3 __asm__("x3") = perm;
    __asm__ volatile("svc #0x15" : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    *h = x1;
    return x0;
}

// svcWaitSynchronization on one handle, for up to `ns` nanoseconds.
static u64 wait_one(u64 h, u64 ns) {
    u32 list = (u32)h;
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)&list;
    register u64 x2 __asm__("x2") = 1;
    register u64 x3 __asm__("x3") = ns;
    __asm__ volatile("svc #0x18" : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

// svcResetSignal, as libnx's auto-clearing events do after a wait.
static u64 reset_signal(u64 h) {
    register u64 x0 __asm__("x0") = h;
    __asm__ volatile("svc #0x17" : "+r"(x0) : : "memory");
    return x0;
}

// ---- messages ---------------------------------------------------------------

static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

typedef struct {
    u64 addr, size;
} buf;

// What a request carries besides its arguments, as libnx's cmifMakeRequest
// and hipcMakeRequest lay it out: pointers (X), send (A), receive (B) and
// receive list (C) buffers; each C buffer is an out pointer with its size in
// the table after the arguments.
typedef struct {
    int pid;
    u32 ncopy, copy[2];
    u32 nx, na, nb, nc;
    buf x[1], a[1], b[1], c[1];
} req;

static u32 copied, moved;
static volatile u32 *reply_data;

static u32 call_ex(u64 h, const req *r, u32 type, u32 cmd, const void *args, u32 size) {
    volatile u32 *m = msg();
    u32 table = (16 + 16 + size + 1) & ~1u;
    u32 words = (table + 2 * r->nc + 3) / 4;
    int special = r->pid || r->ncopy;
    m[0] = type | r->nx << 16 | r->na << 20 | r->nb << 24;
    m[1] = words | (r->nc ? (2 + r->nc) << 10 : 0) | (special ? 1u << 31 : 0);
    u32 at = 2;
    if (special) {
        m[at++] = (r->pid ? 1 : 0) | r->ncopy << 1;
        if (r->pid) {
            m[at++] = 0;
            m[at++] = 0;
        }
        for (u32 i = 0; i < r->ncopy; i++) m[at++] = r->copy[i];
    }
    for (u32 i = 0; i < r->nx; i++) {
        u64 a = r->x[i].addr;
        m[at++] = i | (u32)(a >> 36 & 0x3f) << 6 | (u32)(a >> 32 & 0xf) << 12 |
                  (u32)r->x[i].size << 16;
        m[at++] = (u32)a;
    }
    for (u32 i = 0; i < r->na + r->nb; i++) {
        const buf *d = i < r->na ? &r->a[i] : &r->b[i - r->na];
        m[at++] = (u32)d->size;
        m[at++] = (u32)d->addr;
        m[at++] = (u32)(d->addr >> 36 & 0x3fffff) << 2 | (u32)(d->size >> 32 & 0xf) << 24 |
                  (u32)(d->addr >> 32 & 0xf) << 28;
    }
    u32 data = at;
    for (u32 i = 0; i < words; i++) m[data + i] = 0;
    u32 hdr = (data + 3) & ~3u;
    m[hdr] = SFCI;
    m[hdr + 1] = 0;
    m[hdr + 2] = cmd;
    m[hdr + 3] = 0;
    volatile u8 *raw = (volatile u8 *)&m[hdr + 4];
    for (u32 i = 0; i < size; i++) raw[i] = ((const u8 *)args)[i];
    volatile u16 *sizes = (volatile u16 *)((volatile u8 *)&m[data] + table);
    for (u32 i = 0; i < r->nc; i++) sizes[i] = (u16)r->c[i].size;
    at = data + words;
    for (u32 i = 0; i < r->nc; i++) {
        m[at++] = (u32)r->c[i].addr;
        m[at++] = (u32)(r->c[i].addr >> 32 & 0xffff) | (u32)r->c[i].size << 16;
    }
    if (send_sync_request(h) != 0) fail("svcSendSyncRequest failed");
    at = 2;
    copied = moved = 0;
    if (m[1] >> 31) {
        u32 special_out = m[2], ncopy = special_out >> 1 & 0xf, nmove = special_out >> 5 & 0xf;
        at = 3;
        if (ncopy) copied = m[at];
        at += ncopy;
        if (nmove) moved = m[at];
        at += nmove;
    }
    while (at % 4) at++;
    if (m[at] != SFCO) fail("no SFCO header in the reply");
    reply_data = &m[at + 4];
    return m[at + 2];
}

static const req none = {0};

static u32 call(u64 h, u32 cmd, const void *args, u32 size) {
    return call_ex(h, &none, 4, cmd, args, size);
}

static u32 out32(u32 i) { return reply_data[i]; }

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

static u32 get_service(u64 sm, const char *name, u64 *h) {
    u8 n[8] = {0};
    for (int i = 0; i < 8 && name[i]; i++) n[i] = (u8)name[i];
    u32 rc = call(sm, 1, n, 8);
    *h = moved;
    return rc;
}

// ---- nvdrv ------------------------------------------------------------------

static u64 nvdrv;
static u32 ctrl;

// Ioctl as libnx's nvIoctl sends a small one: the argument in an X
// pointer and back through a C pointer; the NvError.
static u32 ioctl(u32 fd, u32 code, void *arg, u32 size) {
    u32 args[2] = {fd, code};
    req r = {.nx = 1, .na = 1, .nb = 1, .nc = 1};
    r.x[0] = (buf){(u64)arg, size};
    r.c[0] = (buf){(u64)arg, size};
    if (call_ex(nvdrv, &r, 4, 1, args, sizeof args) != 0) fail("nvdrv Ioctl failed");
    return out32(0);
}

// The /dev/nvhost-ctrl ioctls, numbered as libnx numbers them.
#define SYNCPT_READ 0xc0080014u
#define SYNCPT_INCR 0xc0040015u
#define SYNCPT_WAIT 0xc00c0016u
#define SYNCPT_READ_MAX 0xc008001au
#define EVENT_SIGNAL 0xc004001cu // SYNCPT_CLEAR_EVENT_WAIT
#define EVENT_WAIT_ASYNC 0xc010001eu // SYNCPT_WAIT_EVENT_EX
#define EVENT_REGISTER 0xc004001fu // SYNCPT_ALLOC_EVENT
#define EVENT_UNREGISTER 0xc0040020u // SYNCPT_FREE_EVENT
#define FREE_EVENT_BATCH 0x40080021u

#define SYNCPT 7
#define SLOT 0

static u32 read_syncpt(const char *label, u32 code, u32 id) {
    u32 a[2] = {id, 0xdeadbeef};
    u32 err = ioctl(ctrl, code, a, sizeof a);
    hex(label, err);
    // What an error leaves in the output is not documented: not printed.
    if (err == 0) dec("  value: ", a[1]);
    return a[1];
}

static void incr(u32 id) {
    u32 a[1] = {id};
    hex("SYNCPT_INCR: ", ioctl(ctrl, SYNCPT_INCR, a, sizeof a));
}

static void wait(u32 id, u32 threshold, int timeout) {
    u32 a[3] = {id, threshold, (u32)timeout};
    hex("SYNCPT_WAIT: ", ioctl(ctrl, SYNCPT_WAIT, a, sizeof a));
}

// EventWaitAsync as libnx's _nvFenceWait_200 sends it.
static void wait_async(u32 id, u32 threshold, u32 slot) {
    u32 a[4] = {id, threshold, (u32)-1, slot};
    hex("SYNCPT_WAIT_EVENT_EX: ", ioctl(ctrl, EVENT_WAIT_ASYNC, a, sizeof a));
    hex("  value: ", a[3]);
}

static u32 slot_op(u32 code, u32 v) {
    u32 a[1] = {v};
    return ioctl(ctrl, code, a, sizeof a);
}

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew nvhost-ctrl test\n");
    u64 aruid = process_id();
    req pid = {.pid = 1};

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call_ex(sm, &pid, 4, 0, &zero, 8));

    // nvInitialize: transfer memory from the heap, then the aruid.
    u64 heap;
    hex("SetHeapSize: ", set_heap_size(0x200000, &heap));
    u64 tmem;
    hex("CreateTransferMemory: ", create_transfer_memory(&tmem, heap, 0x100000, 0));
    hex("nvdrv: ", get_service(sm, "nvdrv", &nvdrv));
    req init = {.ncopy = 2, .copy = {CUR_PROCESS, (u32)tmem}};
    const u32 tmem_size = 0x100000;
    hex("Initialize: ", call_ex(nvdrv, &init, 4, 3, &tmem_size, 4));
    hex("  error: ", out32(0));
    hex("SetAruid: ", call_ex(nvdrv, &pid, 4, 8, &aruid, 8));
    hex("  error: ", out32(0));

    // nvFenceInit.
    static const char ctrl_path[] = "/dev/nvhost-ctrl";
    req path = {.na = 1};
    path.a[0] = (buf){(u64)ctrl_path, sizeof ctrl_path - 1};
    hex("Open /dev/nvhost-ctrl: ", call_ex(nvdrv, &path, 4, 0, 0, 0));
    ctrl = out32(0);
    hex("  fd: ", ctrl);
    hex("  error: ", out32(1));

    // Syncpoints: read, increment, read again; the max follows.
    read_syncpt("SYNCPT_READ: ", SYNCPT_READ, SYNCPT);
    incr(SYNCPT);
    incr(SYNCPT);
    read_syncpt("SYNCPT_READ: ", SYNCPT_READ, SYNCPT);
    read_syncpt("SYNCPT_READ_MAX: ", SYNCPT_READ_MAX, SYNCPT);
    read_syncpt("SYNCPT_READ of another: ", SYNCPT_READ, SYNCPT + 1);
    read_syncpt("SYNCPT_READ of 192: ", SYNCPT_READ, 192);
    // Waits that need no waiting: reached, or nothing to wait for.
    wait(SYNCPT, 2, -1);
    wait(SYNCPT, 1, 0);
    wait(SYNCPT, 3, 0);

    // nvFenceWait on a fence (SYNCPT, 4), as libnx 2.0.0+ does: register
    // an event slot, query its event, arm the wait.
    hex("EventRegister: ", slot_op(EVENT_REGISTER, SLOT));
    hex("EventRegister again: ", slot_op(EVENT_REGISTER, SLOT));
    u32 q[2] = {ctrl, 0x10000000u | SLOT};
    hex("QueryEvent: ", call(nvdrv, 4, q, sizeof q));
    hex("  error: ", out32(0));
    u64 event = copied;
    hex("  event handed out: ", event != 0);
    wait_async(SYNCPT, 4, SLOT);
    hex("wait for the event, 0 ns: ", wait_one(event, 0));
    // The program is the only one moving syncpoints: two increments.
    incr(SYNCPT);
    hex("wait for the event, 0 ns: ", wait_one(event, 0));
    incr(SYNCPT);
    hex("wait for the event: ", wait_one(event, ~0ull));
    hex("reset it: ", reset_signal(event));
    // Already reached: answered at once.
    wait_async(SYNCPT, 4, SLOT);

    // The timeout path: libnx's wait on the event times out, so it
    // clears the slot's wait with EventSignal; increments later signal
    // nothing.
    wait_async(SYNCPT, 6, SLOT);
    hex("wait for the event, 1 ms: ", wait_one(event, 1000000));
    hex("EventSignal: ", slot_op(EVENT_SIGNAL, 0x10000000u | SLOT));
    incr(SYNCPT);
    incr(SYNCPT);
    hex("wait for the event, 0 ns: ", wait_one(event, 0));
    read_syncpt("SYNCPT_READ: ", SYNCPT_READ, SYNCPT);

    // nvFenceExit: the event closed, the slot unregistered.
    hex("close the event: ", close_handle(event));
    hex("EventUnregister: ", slot_op(EVENT_UNREGISTER, SLOT));
    hex("EventUnregister again: ", slot_op(EVENT_UNREGISTER, SLOT));
    hex("QueryEvent of a free slot: ", call(nvdrv, 4, q, sizeof q));
    hex("  error: ", out32(0));
    // Slots in a batch.
    hex("EventRegister 5: ", slot_op(EVENT_REGISTER, 5));
    hex("EventRegister 63: ", slot_op(EVENT_REGISTER, 63));
    hex("EventRegister 64: ", slot_op(EVENT_REGISTER, 64));
    u64 mask = 1ull << 5 | 1ull << 63;
    hex("FREE_EVENT_BATCH: ", ioctl(ctrl, FREE_EVENT_BATCH, &mask, sizeof mask));
    hex("EventUnregister 5: ", slot_op(EVENT_UNREGISTER, 5));

    hex("Close /dev/nvhost-ctrl: ", call(nvdrv, 2, &ctrl, 4));
    hex("  error: ", out32(0));
    hex("close nvdrv: ", close_handle(nvdrv));
    hex("close the transfer memory: ", close_handle(tmem));
    hex("close sm: ", close_handle(sm));
}
