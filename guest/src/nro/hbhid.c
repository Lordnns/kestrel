// A homebrew-format (NRO) test program for input setup and shared memory
// (Switchbrew, "HID services" and "SVC"): the requests libnx's
// hidInitialize and padConfigureInput send, laid out as libnx lays them
// out, and svcMapSharedMemory/svcUnmapSharedMemory on hid's shared memory,
// mapped where libnx's virtmemFindAslr would look: the first free block
// in the address space outside the heap and alias regions. The output is
// modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u
#define SFCI 0x49434653u
#define SFCO 0x4f434653u
#define SHMEM_SIZE 0x40000ull

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
static volatile u8 *out;

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
    out = (volatile u8 *)&m[at + 4];
    return m[at + 2];
}

static void hex(const char *label, u64 v) {
    puts_(label);
    put_hex(v);
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

// The first free block of `size` bytes in the address space outside the
// heap and alias regions.
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

static const u32 npad_ids[] = {0, 1, 2, 3, 4, 5, 6, 7, 0x20};

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew hid test\n");
    u64 aruid = process_id(); // what appletOE's GetAppletResourceUserId gives
    options none = {0}, pid = {.pid = 1};

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call(sm, pid, 0, &zero, 8));
    u64 hid = get_service(sm, "hid");

    // hidInitialize.
    hex("CreateAppletResource: ", call(hid, pid, 0, &aruid, 8));
    u64 resource = moved;
    yes_no("it moved a session handle: ", resource != 0);
    hex("GetSharedMemoryHandle: ", call(resource, none, 0, 0, 0));
    u64 shmem = copied;
    yes_no("it copied a handle: ", shmem != 0);

    u64 at = find_free(SHMEM_SIZE);
    hex("map read-write: ", map_shmem(shmem, at, SHMEM_SIZE, 3));
    hex("map 0x1000 bytes: ", map_shmem(shmem, at, 0x1000, 1));
    hex("map unaligned: ", map_shmem(shmem, at + 0x10, SHMEM_SIZE, 1));
    hex("map in the heap region: ", map_shmem(shmem, info(4), SHMEM_SIZE, 1));
    hex("map in the alias region: ", map_shmem(shmem, info(2), SHMEM_SIZE, 1));
    hex("map with the session handle: ", map_shmem(hid, at, SHMEM_SIZE, 1));
    hex("map read-only: ", map_shmem(shmem, at, SHMEM_SIZE, 1));
    memory_info m = query(at);
    yes_no("one block of 0x40000 bytes: ", m.base == at && m.size == SHMEM_SIZE);
    hex("memory type: ", m.type);
    hex("permission: ", m.perm);
    u64 any = 0;
    for (u64 i = 0; i < SHMEM_SIZE / 8; i++) any |= ((volatile u64 *)at)[i];
    yes_no("every byte is zero (no input yet): ", any == 0);
    hex("unmap the wrong size: ", unmap_shmem(shmem, at, 0x1000));
    hex("unmap elsewhere: ", unmap_shmem(shmem, at + 0x1000, SHMEM_SIZE));
    hex("unmap: ", unmap_shmem(shmem, at, SHMEM_SIZE));
    yes_no("free again: ", query(at).type == 0);
    hex("unmap again: ", unmap_shmem(shmem, at, SHMEM_SIZE));
    hex("map again: ", map_shmem(shmem, at, SHMEM_SIZE, 1));
    hex("close the handle while mapped: ", close_handle(shmem));
    yes_no("still mapped: ", query(at).type == 0x402006);
    hex("unmap through the closed handle: ", unmap_shmem(shmem, at, SHMEM_SIZE));

    // padConfigureInput(1, standard styles): ActivateNpadWithRevision 5
    // ([18.0.0+]), the IDs No1-No8 and Handheld, the styles.
    struct {
        u32 value, pad;
        u64 aruid;
    } in = {5, 0, aruid};
    hex("ActivateNpadWithRevision 5: ", call(hid, pid, 109, &in, sizeof in));
    options ids = {.pid = 1, .ptr = (u64)npad_ids, .ptr_size = sizeof npad_ids};
    hex("SetSupportedNpadIdType: ", call(hid, ids, 102, &aruid, 8));
    in.value = 0x1f; // FullKey, Handheld, JoyDual, JoyLeft, JoyRight
    hex("SetSupportedNpadStyleSet: ", call(hid, pid, 100, &in, sizeof in));
    hex("GetSupportedNpadStyleSet: ", call(hid, pid, 101, &aruid, 8));
    hex("style set: ", *(volatile u32 *)out);
    hex("ActivateTouchScreen: ", call(hid, pid, 11, &aruid, 8));
    hex("ActivateMouse: ", call(hid, pid, 21, &aruid, 8));
    hex("ActivateKeyboard: ", call(hid, pid, 31, &aruid, 8));

    hex("close IAppletResource: ", close_handle(resource));
    hex("close hid: ", close_handle(hid));
    hex("close sm: ", close_handle(sm));
}
