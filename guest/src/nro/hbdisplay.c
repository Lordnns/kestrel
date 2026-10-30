// A homebrew-format (NRO) test program for the display (Switchbrew, "VI
// services", "Nvnflinger services", "NV services"). It sets the display up
// the way libnx's __nx_win_init, nvInitialize and framebufferCreate do
// (`display/*.c`, `services/vi.c`, `services/nv.c`, `nvidia/map.c`): vi:u,
// the "Default" display, the layer am makes, its buffer queue through the
// binder relay, and two block-linear RGB 565 buffers registered with nvmap.
// It then draws three frames of a known pattern, laid out as Mesa describes
// the Tegra GOB, and queues them. The self-test records the frames kestrel
// shows and adds a CRC-32 of each to the output, modelled in
// scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u

typedef unsigned short u16;
#define SFCI 0x49434653u
#define SFCO 0x4f434653u

// The frame: 96x40 RGB 565, 128 pixels (256 bytes, four GOBs) a row, in
// blocks of 16 GOBs (128 rows), as framebufferCreate sizes it.
#define WIDTH 96
#define HEIGHT 40
#define BPP 2
#define PITCH 256
#define BLOCK_HEIGHT_LOG2 4
#define FB_SIZE (PITCH * 128)
#define BUFFERS 2

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

// svcWaitSynchronization on one handle, with no timeout.
static u64 wait_one(u64 h) {
    u32 list = (u32)h;
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)&list;
    register u64 x2 __asm__("x2") = 1;
    register u64 x3 __asm__("x3") = ~0ull;
    __asm__ volatile("svc #0x18" : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

static u64 tick(void) {
    register u64 x0 __asm__("x0");
    __asm__ volatile("svc #0x1e" : "=r"(x0) : : "memory");
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
    buf x[2], a[2], b[2], c[2];
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

static u64 out64(u32 i) { return (u64)reply_data[i] | (u64)reply_data[i + 1] << 32; }

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

static void sdec(const char *label, int v) {
    puts_(label);
    if (v < 0) {
        puts_("-");
        v = -v;
    }
    put_u64((u64)v);
    puts_("\n");
}

static u32 get_service(u64 sm, const char *name, u64 *h) {
    u8 n[8] = {0};
    for (int i = 0; i < 8 && name[i]; i++) n[i] = (u8)name[i];
    u32 rc = call(sm, 1, n, 8);
    *h = moved;
    return rc;
}

// ---- parcels ----------------------------------------------------------------

static const char token[] = "android.gui.IGraphicBufferProducer";

typedef struct {
    u32 data[0x100];
    u32 len; // payload bytes
} parcel;

static void put(parcel *p, u32 v) { p->data[4 + p->len / 4] = v, p->len += 4; }

// A request parcel: the header, then writeInterfaceToken.
static void parcel_start(parcel *p) {
    for (u32 i = 0; i < 0x100; i++) p->data[i] = 0;
    p->len = 0;
    put(p, 0x100);
    put(p, sizeof token - 1);
    u16 *s = (u16 *)&p->data[4 + p->len / 4];
    for (u32 i = 0; i < sizeof token; i++) s[i] = (u16)token[i];
    p->len += (sizeof token * 2 + 3) & ~3u;
}

static void put_flattened(parcel *p, const void *b, u32 size) {
    put(p, size);
    put(p, 0);
    u8 *d = (u8 *)&p->data[4 + p->len / 4];
    for (u32 i = 0; i < size; i++) d[i] = ((const u8 *)b)[i];
    p->len += (size + 3) & ~3u;
}

static void parcel_end(parcel *p) {
    p->data[0] = p->len;
    p->data[1] = 0x10;
    p->data[2] = 0;
    p->data[3] = 0x10 + p->len;
}

static u64 relay;
static u32 binder_id;
static parcel tx, rx;
static u32 rx_at;

// TransactParcelAuto with pointers, as libnx's binderTransactParcel sends
// it once the relay's pointer buffer holds the parcels.
static u32 transact(u32 code) {
    parcel_end(&tx);
    u32 args[3] = {binder_id, code, 0};
    req r = {.nx = 1, .na = 1, .nb = 1, .nc = 1};
    r.x[0] = (buf){(u64)tx.data, 0x10 + tx.len};
    r.c[0] = (buf){(u64)rx.data, sizeof rx.data};
    u32 rc = call_ex(relay, &r, 4, 3, args, sizeof args);
    rx_at = rx.data[1] / 4;
    return rc;
}

static u32 take(void) { return rx.data[rx_at++]; }

// ---- nvdrv ------------------------------------------------------------------

static u64 nvdrv;

static u32 ioctl(u32 fd, u32 code, void *arg, u32 size) {
    u32 args[2] = {fd, code};
    req r = {.nx = 1, .na = 1, .nb = 1, .nc = 1};
    r.x[0] = (buf){(u64)arg, size};
    r.c[0] = (buf){(u64)arg, size};
    if (call_ex(nvdrv, &r, 4, 1, args, sizeof args) != 0) fail("nvdrv Ioctl failed");
    return out32(0);
}

// ---- the GraphicBuffer, as libnx's nvidia/graphic_buffer.h lays it out -----

typedef struct {
    u32 width, height;
    u64 color_format;
    u32 layout, pitch, unused, offset, kind, block_height_log2, scan, second_field_offset;
    u64 flags, size;
    u32 unk[6];
} surface;

typedef struct {
    int version, num_fds, num_ints;
    int unk0, nvmap_id;
    u32 unk2, magic, pid, type, usage, format, ext_format, stride, total_size, num_planes, unk12;
    surface planes[3];
    u64 unused;
} graphic_buffer;

// The offsets kestrel reads, from the ints after the NativeHandle header.
#define INT(field) (__builtin_offsetof(graphic_buffer, field) - 12)
_Static_assert(INT(nvmap_id) == 4, "nvmap ID");
_Static_assert(INT(planes[0].layout) == 0x44, "layout");
_Static_assert(INT(planes[0].pitch) == 0x48, "pitch");
_Static_assert(INT(planes[0].offset) == 0x50, "offset");
_Static_assert(INT(planes[0].block_height_log2) == 0x58, "block height");
_Static_assert(sizeof(graphic_buffer) == 0x150, "size");

// SET_PREALLOCATED_BUFFER's flattened buffer: the "GBFR" header, then the
// ints.
static struct {
    u32 magic, width, height, stride, format, usage, pid, refcount, num_fds, num_ints;
    u32 ints[(sizeof(graphic_buffer) - 12) / 4];
} flat;

// ---- drawing ----------------------------------------------------------------

static u8 *fb;

// The byte offset of byte x of row y in a buffer: blocks of 16 GOBs across,
// then down; GOBs down a block; in a GOB, 32-byte sectors of two 16-byte rows,
// numbered down the first 32-byte column in pairs, then the second (Mesa,
// GOBType::TegraColor).
static u32 offset(u32 x, u32 y) {
    u32 block = (y / 128) * (PITCH / 64) + x / 64, gob = (y % 128) / 8;
    u32 gx = x % 64, gy = y % 8;
    u32 sector = (gx / 32) * 8 + (gy / 2) * 2 + (gx % 32) / 16;
    return (block * 16 + gob) * 512 + sector * 32 + (gy % 2) * 16 + gx % 16;
}

// Frame f's pattern, kept in step with gen-expected.py.
static u16 pattern(u32 f, u32 x, u32 y) {
    return (u16)((x * 0x0841u) ^ (y * 0x1003u) ^ (f * 0x5555u));
}

static void draw(u32 slot, u32 f) {
    u8 *b = fb + slot * FB_SIZE;
    for (u32 y = 0; y < HEIGHT; y++)
        for (u32 x = 0; x < WIDTH; x++) *(u16 *)(b + offset(x * BPP, y)) = pattern(f, x, y);
}

static int dequeue(void) {
    parcel_start(&tx);
    put(&tx, 1); // async
    put(&tx, WIDTH);
    put(&tx, HEIGHT);
    put(&tx, 4); // PIXEL_FORMAT_RGB_565
    put(&tx, 0xb00);
    hex("  DEQUEUE_BUFFER: ", transact(3));
    int slot = (int)take();
    sdec("  slot: ", slot);
    if (take()) {
        hex("  fence bytes: ", take());
        hex("  fence descriptors: ", take());
        u32 fences = take();
        hex("  fences: ", fences);
        for (int i = 0; i < 8; i++) take();
    }
    sdec("  status: ", (int)take());
    return slot;
}

static void queue(int slot) {
    parcel_start(&tx);
    put(&tx, (u32)slot);
    u8 input[0x54] = {0};
    input[0x24] = 1; // swap interval 1
    put_flattened(&tx, input, sizeof input);
    hex("  QUEUE_BUFFER: ", transact(7));
    dec("  width: ", take());
    dec("  height: ", take());
    hex("  transform hint: ", take());
    dec("  pending: ", take());
    sdec("  status: ", (int)take());
}

static void frame(u32 f, int *requested) {
    dec("frame ", f);
    int slot = dequeue();
    if (slot < 0) fail("no buffer");
    if (!(*requested & 1 << slot)) {
        parcel_start(&tx);
        put(&tx, (u32)slot);
        hex("  REQUEST_BUFFER: ", transact(1));
        hex("  non-null: ", take());
        u32 len = take();
        hex("  buffer bytes: ", len);
        take();
        flat.ints[INT(planes[0].offset) / 4] = (u32)slot * FB_SIZE;
        int same = 1;
        for (u32 i = 0; i < len / 4; i++) same &= rx.data[rx_at + i] == ((u32 *)&flat)[i];
        rx_at += len / 4;
        puts_(same ? "  the buffer it was given: yes\n" : "  the buffer it was given: no\n");
        sdec("  status: ", (int)take());
        *requested |= 1 << slot;
    }
    draw((u32)slot, f);
    queue(slot);
}

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew display test\n");
    u64 aruid = process_id();
    req pid = {.pid = 1};

    u64 sm, vi, h;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call_ex(sm, &pid, 4, 0, &zero, 8));

    // viInitialize: vi:m and vi:s are refused to an application.
    hex("vi:m: ", get_service(sm, "vi:m", &h));
    hex("vi:s: ", get_service(sm, "vi:s", &h));
    hex("vi:u: ", get_service(sm, "vi:u", &vi));
    const u32 user = 0;
    hex("GetDisplayService: ", call(vi, 0, &user, 4));
    u64 ads = moved;
    hex("GetRelayService: ", call(ads, 100, 0, 0));
    relay = moved;
    hex("GetSystemDisplayService: ", call(ads, 101, 0, 0));

    // viOpenDefaultDisplay.
    char name[0x40] = "Default";
    hex("OpenDisplay: ", call(ads, 1010, name, sizeof name));
    u64 display = out64(0);
    hex("display: ", display);
    hex("GetDisplayResolution: ", call(ads, 1102, &display, 8));
    dec("width: ", out64(0));
    dec("height: ", out64(2));

    // viCreateLayer: am makes the layer, vi opens it.
    u64 am;
    hex("appletOE: ", get_service(sm, "appletOE", &am));
    req process = {.pid = 1, .ncopy = 1, .copy = {CUR_PROCESS}};
    hex("OpenApplicationProxy: ", call_ex(am, &process, 4, 0, &zero, 8));
    u64 proxy = moved;
    hex("GetSelfController: ", call(proxy, 1, 0, 0));
    u64 selfc = moved;
    hex("CreateManagedDisplayLayer: ", call(selfc, 40, 0, 0));
    u64 layer = out64(0);
    hex("layer: ", layer);
    static u8 window[0x100];
    struct {
        char name[0x40];
        u64 layer, aruid;
    } open = {"Default", layer, aruid};
    req w = {.pid = 1, .nb = 1};
    w.b[0] = (buf){(u64)window, sizeof window};
    hex("OpenLayer: ", call_ex(ads, &w, 4, 2020, &open, sizeof open));
    u64 size = out64(0);
    hex("native window bytes: ", size);
    u32 *nw = (u32 *)window;
    binder_id = nw[nw[1] / 4 + 2];
    hex("binder: ", binder_id);
    struct {
        u32 mode, pad;
        u64 layer;
    } scaling = {2, 0, layer};
    hex("SetLayerScalingMode: ", call(ads, 2101, &scaling, sizeof scaling));

    // nwindowCreate: references, the buffer-free event, CONNECT.
    hex("QueryPointerBufferSize: ", call_ex(relay, &none, 5, 3, 0, 0));
    hex("pointer buffer: ", reply_data[0] & 0xffff);
    u32 weak[3] = {binder_id, 1, 0}, strong[3] = {binder_id, 1, 1};
    hex("AdjustRefcount weak: ", call(relay, 1, weak, sizeof weak));
    hex("AdjustRefcount strong: ", call(relay, 1, strong, sizeof strong));
    u32 native[2] = {binder_id, 0x0f};
    hex("GetNativeHandle: ", call(relay, 2, native, sizeof native));
    u64 event = copied;
    parcel_start(&tx);
    put(&tx, 0);
    put(&tx, 2); // NATIVE_WINDOW_API_CPU
    put(&tx, 0);
    hex("CONNECT: ", transact(10));
    dec("  width: ", take());
    dec("  height: ", take());
    hex("  transform hint: ", take());
    dec("  pending: ", take());
    sdec("  status: ", (int)take());

    // nvInitialize: transfer memory from the heap, a clone, the aruid.
    u64 heap;
    hex("SetHeapSize: ", set_heap_size(0x200000, &heap));
    u64 tmem;
    hex("CreateTransferMemory: ", create_transfer_memory(&tmem, heap + 0x100000, 0x100000, 0));
    hex("nvdrv: ", get_service(sm, "nvdrv", &nvdrv));
    req init = {.ncopy = 2, .copy = {CUR_PROCESS, (u32)tmem}};
    const u32 tmem_size = 0x100000;
    hex("Initialize: ", call_ex(nvdrv, &init, 4, 3, &tmem_size, 4));
    hex("  error: ", out32(0));
    hex("close the transfer memory handle: ", close_handle(tmem));
    const u32 tag = 1;
    hex("CloneCurrentObjectEx: ", call_ex(nvdrv, &none, 5, 4, &tag, 4));
    hex("clone handed out: ", moved != 0);
    u64 clone = moved;
    hex("SetAruid: ", call_ex(nvdrv, &pid, 4, 8, &aruid, 8));
    hex("  error: ", out32(0));
    static const char nvmap_path[] = "/dev/nvmap", ctrl_path[] = "/dev/nvhost-ctrl";
    req path = {.na = 1};
    path.a[0] = (buf){(u64)nvmap_path, sizeof nvmap_path - 1};
    hex("Open /dev/nvmap: ", call_ex(nvdrv, &path, 4, 0, 0, 0));
    u32 nvmap = out32(0);
    hex("  fd: ", nvmap);
    hex("  error: ", out32(1));
    path.a[0] = (buf){(u64)ctrl_path, sizeof ctrl_path - 1};
    hex("Open /dev/nvhost-ctrl: ", call_ex(nvdrv, &path, 4, 0, 0, 0));
    hex("  fd: ", out32(0));
    hex("  error: ", out32(1));

    // framebufferCreate: two buffers in one nvmap object.
    fb = (u8 *)heap;
    u32 create[2] = {BUFFERS * FB_SIZE, 0};
    hex("NVMAP_IOC_CREATE: ", ioctl(nvmap, 0xc0080101, create, sizeof create));
    hex("  handle: ", create[1]);
    struct {
        u32 handle, heapmask, flags, align;
        u8 kind, pad[7];
        u64 addr;
    } alloc = {create[1], 0, 1, 0x20000, 0, {0}, (u64)fb};
    hex("NVMAP_IOC_ALLOC: ", ioctl(nvmap, 0xc0200104, &alloc, sizeof alloc));
    u32 get_id[2] = {0, create[1]};
    hex("NVMAP_IOC_GET_ID: ", ioctl(nvmap, 0xc008010e, get_id, sizeof get_id));
    hex("  id: ", get_id[0]);
    static graphic_buffer gb;
    gb.num_ints = (sizeof gb - 12) / 4;
    gb.unk0 = -1;
    gb.nvmap_id = (int)get_id[0];
    gb.magic = 0xdaffcaff;
    gb.pid = 42;
    gb.usage = 0xb00;
    gb.format = gb.ext_format = 4;
    gb.stride = PITCH / BPP;
    gb.total_size = FB_SIZE;
    gb.num_planes = 1;
    gb.planes[0] = (surface){.width = WIDTH,
                             .height = HEIGHT,
                             .color_format = 0x010a881210ull,
                             .layout = 3,
                             .pitch = PITCH,
                             .kind = 0xfe,
                             .block_height_log2 = BLOCK_HEIGHT_LOG2,
                             .size = FB_SIZE};
    flat.magic = 0x47424652;
    flat.width = WIDTH;
    flat.height = HEIGHT;
    flat.stride = PITCH / BPP;
    flat.format = 4;
    flat.usage = 0xb00;
    flat.pid = 42;
    flat.num_ints = (u32)gb.num_ints;
    for (u32 slot = 0; slot < BUFFERS; slot++) {
        gb.planes[0].offset = slot * FB_SIZE;
        const u32 *ints = (const u32 *)&gb + 3;
        for (u32 i = 0; i < sizeof flat.ints / 4; i++) flat.ints[i] = ints[i];
        parcel_start(&tx);
        put(&tx, slot);
        put(&tx, 1);
        put_flattened(&tx, &flat, sizeof flat);
        dec("SET_PREALLOCATED_BUFFER ", slot);
        hex("  result: ", transact(14));
        hex("  reply bytes: ", rx.data[0]);
    }

    // Two frames fill both buffers; a third must wait for a vsync.
    int requested = 0;
    frame(1, &requested);
    frame(2, &requested);
    puts_("a third buffer:\n");
    dequeue();
    u64 before = tick();
    hex("wait for the buffer-free event: ", wait_one(event));
    u64 waited = tick() - before;
    puts_(waited > 0 && waited <= 320000 ? "woken by the next vsync: yes\n"
                                          : "woken by the next vsync: no\n");
    frame(3, &requested);

    // nwindowClose and framebufferClose.
    parcel_start(&tx);
    put(&tx, 2);
    hex("DISCONNECT: ", transact(11));
    sdec("  status: ", (int)take());
    u32 nvfree[6] = {create[1], 0, 0, 0, 0, 0};
    hex("NVMAP_IOC_FREE: ", ioctl(nvmap, 0xc0180105, nvfree, sizeof nvfree));
    hex("  address is the buffers: ", ((u64)nvfree[2] | (u64)nvfree[3] << 32) == (u64)fb);
    hex("  size: ", nvfree[4]);
    hex("Close /dev/nvmap: ", call(nvdrv, 2, &nvmap, 4));
    hex("  error: ", out32(0));
    hex("close the clone: ", close_handle(clone));
    hex("close nvdrv: ", close_handle(nvdrv));
    hex("CloseLayer: ", call(ads, 2021, &layer, 8));
    hex("CloseDisplay: ", call(ads, 1020, &display, 8));
    hex("close the event: ", close_handle(event));
    hex("close the relay: ", close_handle(relay));
    hex("close the display service: ", close_handle(ads));
    hex("close vi:u: ", close_handle(vi));
    hex("close sm: ", close_handle(sm));
}
