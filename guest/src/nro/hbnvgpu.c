// A homebrew-format (NRO) test program for the GPU's nvdrv devices
// (Switchbrew, "NV services"): /dev/nvhost-ctrl-gpu, /dev/nvhost-as-gpu
// and /dev/nvhost-gpu, sent the way libnx's nvGpuInit, nvAddressSpace*,
// nvMapCreate and nvGpuChannelCreate send them (`nvidia/*.c`) and so as
// deko3d sets up a device and a queue: the GPU's description, an address
// space with a reservation and mappings, a channel with its queue, engine
// and error notifier. Then it submits one command list as libnx's
// nvGpuChannelKickoff does (Ioctl2), which binds the 3D engine, releases a
// host semaphore and a 3D report semaphore, and increments the channel's
// syncpoint twice: what the GPU wrote and the fence it answers are printed.
// A second list uploads a macro (MME) and calls it, and fills a constant
// buffer. A third copies: inline-to-memory and DMA copy, pitch and block
// linear. The expected output is modelled in scripts/gen-expected.py.
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
    buf x[2], a[2], b[1], c[1];
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

// Ioctl as libnx's nvIoctl sends a small one: the argument in an X
// pointer and back through a C pointer; the NvError.
// Ioctl2 as libnx's nvIoctl2 sends it: the argument and a second input
// (X pointers 0 and 1, the A buffers empty), the argument back through a C
// pointer; the NvError.
static u32 ioctl2(u32 fd, u32 code, void *arg, u32 size, const void *more, u32 more_size) {
    u32 args[2] = {fd, code};
    req r = {.nx = 2, .na = 2, .nb = 1, .nc = 1};
    r.x[0] = (buf){(u64)arg, size};
    r.x[1] = (buf){(u64)more, more_size};
    r.c[0] = (buf){(u64)arg, size};
    if (call_ex(nvdrv, &r, 4, 11, args, sizeof args) != 0) fail("nvdrv Ioctl2 failed");
    return out32(0);
}

static u32 ioctl(u32 fd, u32 code, void *arg, u32 size) {
    u32 args[2] = {fd, code};
    req r = {.nx = 1, .na = 1, .nb = 1, .nc = 1};
    r.x[0] = (buf){(u64)arg, size};
    r.c[0] = (buf){(u64)arg, size};
    if (call_ex(nvdrv, &r, 4, 1, args, sizeof args) != 0) fail("nvdrv Ioctl failed");
    return out32(0);
}

static u32 io(u32 fd, u32 code, void *arg, u32 size) { return ioctl(fd, code, arg, size); }

static u32 open_dev(const char *path, u32 len) {
    req r = {.na = 1};
    r.a[0] = (buf){(u64)path, len};
    hex("  open: ", call_ex(nvdrv, &r, 4, 0, 0, 0));
    u32 fd = out32(0);
    hex("  fd: ", fd);
    hex("  error: ", out32(1));
    return fd;
}

static void close_dev(const char *label, u32 fd) {
    puts_(label);
    hex(": ", call(nvdrv, 2, &fd, 4));
    hex("  error: ", out32(0));
}

// MAP_BUFFER_EX's argument (libnx's nvioctlNvhostAsGpu_MapBufferEx).
typedef struct {
    u32 flags, kind, handle, page_size;
    u64 buffer_offset, mapping_size, offset;
} map_args;

static u32 map(u32 as, u32 flags, u32 kind, u32 handle, u32 page, u64 off, u64 size, u64 at,
               u64 *out) {
    map_args m = {flags, kind, handle, page, off, size, at};
    u32 err = io(as, 0xc0284106, &m, sizeof m);
    if (out) *out = m.offset;
    return err;
}

// ALLOC_SPACE's argument.
typedef struct {
    u32 pages, page_size, flags, pad;
    u64 offset;
} space_args;

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew GPU devices test\n");
    u64 aruid = process_id();
    req pid = {.pid = 1};

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call_ex(sm, &pid, 4, 0, &zero, 8));

    // nvInitialize: transfer memory from the heap's second half; the first
    // half holds the memory the GPU maps.
    u64 heap;
    hex("SetHeapSize: ", set_heap_size(0x200000, &heap));
    u64 tmem;
    hex("CreateTransferMemory: ", create_transfer_memory(&tmem, heap + 0x100000, 0x100000, 0));
    hex("nvdrv: ", get_service(sm, "nvdrv", &nvdrv));
    req init = {.ncopy = 2, .copy = {CUR_PROCESS, (u32)tmem}};
    const u32 tmem_size = 0x100000;
    hex("Initialize: ", call_ex(nvdrv, &init, 4, 3, &tmem_size, 4));
    hex("  error: ", out32(0));
    hex("SetAruid: ", call_ex(nvdrv, &pid, 4, 8, &aruid, 8));
    hex("  error: ", out32(0));

    // nvMapInit and nvMapCreate: 0x30000 bytes of the heap, aligned to the
    // big page size, as deko3d's memory blocks.
    static const char nvmap_path[] = "/dev/nvmap";
    puts_("/dev/nvmap\n");
    u32 nvmap = open_dev(nvmap_path, sizeof nvmap_path - 1);
    u32 create[2] = {0x30000, 0};
    hex("NVMAP_IOC_CREATE: ", io(nvmap, 0xc0080101, create, sizeof create));
    u32 handle = create[1];
    struct {
        u32 handle, heapmask, flags, align;
        u8 kind, pad[7];
        u64 addr;
    } alloc = {handle, 0, 0, 0x20000, 0, {0}, heap};
    hex("NVMAP_IOC_ALLOC: ", io(nvmap, 0xc0200104, &alloc, sizeof alloc));

    // nvGpuInit: /dev/nvhost-ctrl-gpu's description of the GPU.
    static const char ctrl_gpu_path[] = "/dev/nvhost-ctrl-gpu";
    puts_("/dev/nvhost-ctrl-gpu\n");
    u32 ctrl_gpu = open_dev(ctrl_gpu_path, sizeof ctrl_gpu_path - 1);
    struct {
        u64 size, addr;
        u32 c[0xa0 / 4];
    } chars = {0xa0, 1, {0}};
    hex("GET_CHARACTERISTICS: ", io(ctrl_gpu, 0xc0b04705, &chars, sizeof chars));
    hex("  size: ", chars.size);
    hex("  arch: ", chars.c[0]);
    hex("  impl: ", chars.c[1]);
    hex("  big page size: ", chars.c[10]);
    hex("  available big page sizes: ", chars.c[13]);
    dec("  GPU address bits: ", chars.c[18]);
    hex("  3D class: ", chars.c[23]);
    hex("  GPFIFO class: ", chars.c[25]);
    puts_("  chip: ");
    char chip[9] = {0};
    for (int i = 0; i < 8; i++) chip[i] = ((char *)&chars.c[36])[i];
    puts_(chip);
    puts_("\n");
    u32 ctx_size = 0xdead;
    hex("ZCULL_GET_CTX_SIZE: ", io(ctrl_gpu, 0x80044701, &ctx_size, 4));
    hex("  size: ", ctx_size);
    u32 zcull[10] = {0};
    hex("ZCULL_GET_INFO: ", io(ctrl_gpu, 0x80284702, zcull, sizeof zcull));
    for (int i = 0; i < 10; i++) dec("  ", zcull[i]);
    struct {
        u32 size, pad;
        u64 addr;
        u32 masks[2];
    } tpc = {4, 0, 1, {0, 0}};
    hex("GET_TPC_MASKS: ", io(ctrl_gpu, 0xc0184706, &tpc, sizeof tpc));
    hex("  GPC 0: ", tpc.masks[0]);
    u64 time[2] = {0, 0};
    hex("GET_GPU_TIME: ", io(ctrl_gpu, 0xc010471c, time, sizeof time));
    hex("  running: ", time[0] != 0);

    // nvAddressSpaceCreate with deko3d's page size (the big page size).
    static const char as_path[] = "/dev/nvhost-as-gpu";
    puts_("/dev/nvhost-as-gpu\n");
    u32 as = open_dev(as_path, sizeof as_path - 1);
    u32 init_ex[10] = {1, 0, 0x20000, 0, 0, 0, 0, 0, 0, 0};
    hex("InitializeEx: ", io(as, 0x40284109, init_ex, sizeof init_ex));
    struct {
        u64 addr;
        u32 size, pad;
        struct {
            u64 offset;
            u32 page_size, pad;
            u64 pages;
        } r[2];
    } regions = {0};
    hex("GET_VA_REGIONS: ", io(as, 0xc0404108, &regions, sizeof regions));
    hex("  size: ", regions.size);
    for (int i = 0; i < 2; i++) {
        hex("  region: ", regions.r[i].offset);
        hex("    page size: ", regions.r[i].page_size);
        hex("    pages: ", regions.r[i].pages);
    }
    // deko3d's code segment: a reservation, then a fixed mapping in it.
    space_args sp = {4, 0x20000, 0, 0, 0x20000};
    hex("ALLOC_SPACE: ", io(as, 0xc0184102, &sp, sizeof sp));
    hex("  at: ", sp.offset);
    u64 segment = sp.offset;
    space_args small = {2, 0x1000, 0, 0, 0};
    hex("ALLOC_SPACE small pages: ", io(as, 0xc0184102, &small, sizeof small));
    hex("  at: ", small.offset);
    u64 at;
    hex("MAP_BUFFER_EX: ", map(as, 4, 0xfe, handle, 0x20000, 0, 0, 0, &at));
    hex("  at: ", at);
    u64 pitch = at;
    hex("MAP_BUFFER_EX fixed: ", map(as, 1, 0, handle, 0x20000, 0, 0x20000, segment + 0x20000, &at));
    hex("  at: ", at);
    hex("MAP_BUFFER_EX over it: ", map(as, 1, 0, handle, 0x20000, 0, 0x20000, segment + 0x20000, 0));
    hex("MAP_BUFFER_EX outside: ", map(as, 1, 0, handle, 0x20000, 0, 0x20000, 0x8000000000, 0));
    hex("MAP_BUFFER_EX modify: ", map(as, 0x100, 0xdb, 0, 0, 0x10000, 0x10000, pitch, 0));
    hex("UNMAP_BUFFER: ", io(as, 0xc0084105, &pitch, 8));
    hex("UNMAP_BUFFER again: ", io(as, 0xc0084105, &pitch, 8));

    // nvGpuChannelCreate: a channel bound to the address space, with a
    // GPFIFO, the 3D engine, the error notifier and a priority.
    static const char gpu_path[] = "/dev/nvhost-gpu";
    puts_("/dev/nvhost-gpu\n");
    u32 ch = open_dev(gpu_path, sizeof gpu_path - 1);
    hex("SET_NVMAP_FD: ", io(ch, 0x40044801, &nvmap, 4));
    hex("BIND_CHANNEL: ", io(as, 0x40044101, &ch, 4));
    u32 gpfifo[8] = {0x800, 1, 0, 0, 0, 0, 0, 0};
    hex("ALLOC_GPFIFO_EX2: ", io(ch, 0xc020481a, gpfifo, sizeof gpfifo));
    hex("  syncpoint: ", gpfifo[3]);
    hex("  value: ", gpfifo[4]);
    u32 obj[4] = {0xb197, 0, 0, 0};
    hex("ALLOC_OBJ_CTX: ", io(ch, 0xc0104809, obj, sizeof obj));
    hex("  id: ", obj[2]);
    u32 compute[4] = {0xb1c0, 0, 0, 0};
    hex("ALLOC_OBJ_CTX again: ", io(ch, 0xc0104809, compute, sizeof compute));
    u32 q[2] = {ch, 3};
    hex("QueryEvent: ", call(nvdrv, 4, q, sizeof q));
    hex("  error: ", out32(0));
    u64 error_event = copied;
    hex("  event handed out: ", error_event != 0);
    u32 notifier[6] = {0, 0, 0, 0, 1, 0};
    hex("SET_ERROR_NOTIFIER: ", io(ch, 0xc018480c, notifier, sizeof notifier));
    u32 prio = 0x64;
    hex("SET_PRIORITY: ", io(ch, 0x4004480d, &prio, 4));

    // A command list at offset 0x1000 of the object, which the fixed
    // mapping shows at segment + 0x20000 (`clb06f.h`'s command headers:
    // incrementing 1 << 29, non-incrementing 3 << 29; method offsets in
    // bytes, written divided by 4).
#define INC(m, n) (1u << 29 | (n) << 16 | (m) >> 2)
#define NON(m, n) (3u << 29 | (n) << 16 | (m) >> 2)
    u64 gpu = segment + 0x20000;
    u32 hi = (u32)(gpu >> 32), lo = (u32)gpu;
    u32 sp_id = gpfifo[3];
    u32 *list = (u32 *)(heap + 0x1000);
    u32 n = 0;
    list[n++] = INC(0x00, 1); // SET_OBJECT: 3D on subchannel 0
    list[n++] = 0xb197;
    list[n++] = INC(0x10, 4); // host SEMAPHOREA-D: release, 4 bytes
    list[n++] = hi;
    list[n++] = lo + 0x800;
    list[n++] = 0xabcd;
    list[n++] = 2 | 1u << 24;
    list[n++] = NON(0x2c8, 2); // 3D INCREMENT_SYNC_POINT, twice
    list[n++] = sp_id;
    list[n++] = sp_id;
    list[n++] = INC(0x1b00, 4); // 3D SET_REPORT_SEMAPHORE_A-D: one word
    list[n++] = hi;
    list[n++] = lo + 0x804;
    list[n++] = 0x55;
    list[n++] = 1u << 28;
    list[n++] = INC(0x70, 2); // host SYNCPOINTA/B: wait for 2
    list[n++] = 2;
    list[n++] = sp_id << 8;
    u64 list_va = gpu + 0x1000;
    u64 entry = (u32)list_va | ((list_va >> 32 & 0xff) | (u64)n << 10) << 32;
    struct {
        u64 gpfifo;
        u32 entries, flags, fence_id, fence_value;
    } kick = {0, 1, 1u << 2 | 1u << 8, 0, 2};
    hex("KickoffPb: ", ioctl2(ch, 0xc018481b, &kick, sizeof kick, &entry, sizeof entry));
    hex("  fence syncpoint: ", kick.fence_id);
    hex("  fence value: ", kick.fence_value);
    hex("  host semaphore: ", *(volatile u32 *)(heap + 0x800));
    hex("  report semaphore: ", *(volatile u32 *)(heap + 0x804));

    // A macro (the MME's encoding, `gpu/mme.rs`): it sends its three
    // parameters to SET_REPORT_SEMAPHORE_A-C and releases it with D = 1 << 28
    // (1 shifted left by 28). Loaded at word 0x40 as macro 2, called with
    // increment-once data: CALL_MME_MACRO(2) takes the first word,
    // CALL_MME_DATA(2) the rest. Then a constant buffer of 12 bytes at
    // +0x900, filled from offset 4.
#define MADDI(as, d, a, v) (1u | (as) << 4 | (d) << 8 | (a) << 11 | (u32)(v) << 14)
#define MALU(as, d, a) ((u32)(as) << 4 | (d) << 8 | (a) << 11)
#define MSHL(as, a, b, sz) (4u | (as) << 4 | (a) << 11 | (b) << 14 | (sz) << 22)
#define ONCE(m, n) (5u << 29 | (n) << 16 | (m) >> 2)
    static const u32 mme[] = {
        MADDI(2, 0, 0, 0x1000 | 0x1b00 >> 2), // method 0x1b00, increment 1
        MALU(4, 0, 1),                        // send r1
        MALU(0, 2, 0),                        // parm r2
        MALU(4, 0, 2),                        // send r2
        MALU(0, 3, 0),                        // parm r3
        MALU(4, 0, 3),                        // send r3
        MADDI(1, 4, 0, 1),                    // r4 = 1
        MADDI(1, 5, 0, 28),                   // r5 = 28
        MSHL(4, 5, 4, 1) | 1u << 7,           // send 1 << 28, exit
        MALU(1, 0, 0),                        // the exit's delay slot
    };
    n = 0;
    list[n++] = INC(0x114, 1);
    list[n++] = 0x40;
    list[n++] = NON(0x118, sizeof mme / 4);
    for (u32 i = 0; i < sizeof mme / 4; i++) list[n++] = mme[i];
    list[n++] = INC(0x11c, 2);
    list[n++] = 2;
    list[n++] = 0x40;
    list[n++] = ONCE(0x3810, 3);
    list[n++] = hi;
    list[n++] = lo + 0x808;
    list[n++] = 0x77;
    list[n++] = INC(0x2380, 4); // SET_CONSTANT_BUFFER_SELECTOR_A-C, offset
    list[n++] = 12;
    list[n++] = hi;
    list[n++] = lo + 0x900;
    list[n++] = 4;
    list[n++] = ONCE(0x238c, 3); // LOAD_CONSTANT_BUFFER_OFFSET, then data
    list[n++] = 4;
    list[n++] = 0xaaaa;
    list[n++] = 0xbbbb;
    entry = (u32)list_va | ((list_va >> 32 & 0xff) | (u64)n << 10) << 32;
    kick.entries = 1;
    kick.flags = 1u << 2;
    kick.fence_id = kick.fence_value = 0;
    hex("KickoffPb with a macro: ", ioctl2(ch, 0xc018481b, &kick, sizeof kick, &entry, sizeof entry));
    hex("  fence value: ", kick.fence_value);
    hex("  macro's report: ", *(volatile u32 *)(heap + 0x808));
    hex("  constant buffer word 1: ", *(volatile u32 *)(heap + 0x904));
    hex("  constant buffer word 2: ", *(volatile u32 *)(heap + 0x908));

    // Copies (`clb197.h`'s inline-to-memory methods, `clb0b5.h`'s DMA
    // copy). Inline-to-memory through the 3D engine: two lines of 6 bytes
    // 0x10 apart at +0x2000, releasing a one-word semaphore at +0x2100;
    // then 16 bytes into a block-linear surface at +0x4000, 192 bytes by 16
    // rows in blocks 2 GOBs high, from byte 72 of row 11. DMA copy, bound to
    // subchannel 4 as deko3d does, copies those 16 bytes back to +0x2200
    // (a semaphore at +0x2104), then 0x18 bytes from +0x2000 to +0x2300 as
    // one line, as dkCmdBufCopyBuffer does.
#define INCS(s, m, n) (1u << 29 | (n) << 16 | (s) << 13 | (m) >> 2)
#define IMM(m, v) (4u << 29 | (v) << 16 | (m) >> 2)
    static const u32 copies[] = {
        INC(0x180, 5), 6, 2, 0, 0x2000, 0x10, // LINE_LENGTH_IN..PITCH_OUT
        INC(0x1dc, 3), 0, 0x2100, 0x99,       // SET_I2M_SEMAPHORE_A-C
        IMM(0x1b0, 1 | 2 << 4 | 1 << 12),     // LAUNCH_DMA: pitch, release
        NON(0x1b4, 3), 0x04030201, 0x08070605, 0x0c0b0a09,
        INC(0x194, 7), 1 << 4, 192, 16, 1, 0, 72, 11, // SET_DST_BLOCK_SIZE..
        INC(0x180, 4), 16, 1, 0, 0x4000,
        IMM(0x1b0, 0), // LAUNCH_DMA: block linear
        NON(0x1b4, 4), 0xc0ffee01, 0xc0ffee02, 0xc0ffee03, 0xc0ffee04,
        INCS(4, 0x000, 1), 0xb0b5,                         // SET_OBJECT
        INCS(4, 0x728, 6), 1 << 12 | 1 << 4, 192, 16, 1, 0, 72 | 11 << 16,
        INCS(4, 0x400, 4), 0, 0x4000, 0, 0x2200,           // OFFSET_IN, OUT
        INCS(4, 0x410, 4), 0, 0x100, 16, 1,                // PITCH_IN..
        INCS(4, 0x240, 3), 0, 0x2104, 0x5a,                // SET_SEMAPHORE_*
        INCS(4, 0x300, 1), 2 | 1 << 3 | 1 << 8 | 1 << 9,   // LAUNCH_DMA
        INCS(4, 0x400, 4), 0, 0x2000, 0, 0x2300,
        INCS(4, 0x418, 1), 0x18,
        INCS(4, 0x300, 1), 2 | 1 << 2 | 1 << 7 | 1 << 8,
    };
    n = 0;
    for (u32 i = 0; i < sizeof copies / 4; i++) list[n++] = copies[i];
    // The addresses: each method's upper word, then its lower word, is
    // written above as 0 and an offset from the mapping.
    for (u32 i = 0; i + 1 < n; i++)
        if (list[i] == 0 && list[i + 1] >= 0x2000 && list[i + 1] < 0x5000) {
            list[i] = hi;
            list[i + 1] += lo;
            i++;
        }
    entry = (u32)list_va | ((list_va >> 32 & 0xff) | (u64)n << 10) << 32;
    kick.fence_id = kick.fence_value = 0;
    hex("KickoffPb with copies: ", ioctl2(ch, 0xc018481b, &kick, sizeof kick, &entry, sizeof entry));
    hex("  fence value: ", kick.fence_value);
    volatile u32 *w = (volatile u32 *)heap;
    hex("  inline line 1: ", w[0x2000 / 4]);
    hex("  inline line 1, end: ", w[0x2004 / 4]);
    hex("  inline line 2: ", w[0x2010 / 4]);
    hex("  inline semaphore: ", w[0x2100 / 4]);
    for (u32 k = 0; k < 4; k++) {
        u32 at = 0xffffffff;
        for (u32 i = 0; i < 3 * 1024 / 4; i++)
            if (w[0x4000 / 4 + i] == 0xc0ffee01 + k) at = i * 4;
        hex("  block-linear word at: ", at);
    }
    for (u32 k = 0; k < 4; k++) hex("  copied back: ", w[0x2200 / 4 + k]);
    hex("  copy semaphore: ", w[0x2104 / 4]);
    hex("  buffer copy word 0: ", w[0x2300 / 4]);
    hex("  buffer copy word 4: ", w[0x2310 / 4]);
    hex("  buffer copy word 5: ", w[0x2314 / 4]);

    // nvGpuChannelClose, nvAddressSpaceClose, nvGpuExit, nvMapExit.
    notifier[4] = 0;
    hex("SET_ERROR_NOTIFIER off: ", io(ch, 0xc018480c, notifier, sizeof notifier));
    hex("close the error event: ", close_handle(error_event));
    close_dev("Close /dev/nvhost-gpu", ch);
    // FREE_SPACE: the address first, then pages and page size.
    u32 free_args[4] = {(u32)segment, (u32)(segment >> 32), 4, 0x20000};
    hex("FREE_SPACE: ", io(as, 0xc0104103, free_args, sizeof free_args));
    close_dev("Close /dev/nvhost-as-gpu", as);
    close_dev("Close /dev/nvhost-ctrl-gpu", ctrl_gpu);
    u32 nvfree[6] = {handle, 0, 0, 0, 0, 0};
    hex("NVMAP_IOC_FREE: ", io(nvmap, 0xc0180105, nvfree, sizeof nvfree));
    close_dev("Close /dev/nvmap", nvmap);
    hex("close nvdrv: ", close_handle(nvdrv));
    hex("close the transfer memory: ", close_handle(tmem));
    hex("close sm: ", close_handle(sm));
}
