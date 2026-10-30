// padview: shows the controller kestrel's window builds from your keyboard
// and gamepads. Run it with `kestrel run guest/bin/padview.nro`, press keys
// and buttons (mapped by user/config/input.cfg), and close the window to
// stop. It sets up hid as libnx does and prints player 1 (or the handheld
// Joy-Con) whenever the buttons or sticks change.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u
#define SFCI 0x49434653u
#define SFCO 0x4f434653u
#define SHMEM_SIZE 0x40000ull
#define NPAD 0x9a00
#define NPAD_ENTRY 0x5000
#define FULL_KEY_LIFO 0x28
#define HANDHELD_LIFO 0x378

typedef struct {
    u64 base, size;
    u32 type, attr, perm, ipc_refs, device_refs, padding;
} memory_info;

static u64 send_sync_request(u64 h) {
    register u64 x0 __asm__("x0") = h;
    __asm__ volatile("svc #0x21" : "+r"(x0) : : "memory");
    return x0;
}

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

static u64 map_shmem(u64 h, u64 addr) {
    register u64 x0 __asm__("x0") = h;
    register u64 x1 __asm__("x1") = addr;
    register u64 x2 __asm__("x2") = SHMEM_SIZE;
    register u64 x3 __asm__("x3") = 1;
    __asm__ volatile("svc #0x13" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

static memory_info query(u64 addr) {
    memory_info m;
    register u64 x0 __asm__("x0") = (u64)&m;
    register u64 x1 __asm__("x1");
    register u64 x2 __asm__("x2") = addr;
    __asm__ volatile("svc #0x06" : "+r"(x0), "=r"(x1), "+r"(x2) : : "memory");
    return m;
}

static u64 info(u32 kind) {
    u64 v = 0;
    svc_get_info(&v, kind, CUR_PROCESS, 0);
    return v;
}

static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

static u32 copied, moved;

// A CMIF request (optionally with the process ID and pointer buffer 0),
// and its reply, as libnx lays them out.
static u32 call(u64 h, int pid, u64 ptr, u32 ptr_size, u32 cmd, const void *args, u32 size) {
    volatile u32 *m = msg();
    m[0] = 4 | (ptr ? 1u << 16 : 0);
    m[1] = (((16 + 16 + size + 1) & ~1u) + 3) / 4 | (pid ? 1u << 31 : 0);
    u32 at = 2;
    if (pid) {
        m[at++] = 1;
        m[at++] = 0;
        m[at++] = 0;
    }
    if (ptr) {
        m[at++] = (u32)(ptr >> 36) << 6 | ((u32)(ptr >> 32) & 0xf) << 12 | ptr_size << 16;
        m[at++] = (u32)ptr;
    }
    while (at % 4) m[at++] = 0;
    m[at] = SFCI;
    m[at + 1] = 0;
    m[at + 2] = cmd;
    m[at + 3] = 0;
    for (u32 i = 0; i < size; i++) ((volatile u8 *)&m[at + 4])[i] = ((const u8 *)args)[i];
    if (send_sync_request(h) != 0) fail("svcSendSyncRequest failed");
    at = 2;
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

// The first free block outside the heap and alias regions.
static u64 find_free(void) {
    u64 heap = info(4), heap_end = heap + info(5), alias = info(2), alias_end = alias + info(3);
    u64 at = info(12), end = at + info(13);
    while (at < end) {
        memory_info m = query(at);
        u64 top = m.base + m.size, a = at;
        while (m.type == 0 && a + SHMEM_SIZE <= top) {
            if (a < heap_end && heap < a + SHMEM_SIZE) a = heap_end;
            else if (a < alias_end && alias < a + SHMEM_SIZE) a = alias_end;
            else return a;
        }
        at = top;
    }
    fail("no free block for the shared memory");
    return 0;
}

static const char *const names[] = {
    "A", "B", "X", "Y", "StickL", "StickR", "L", "R", "ZL", "ZR", "Plus", "Minus",
    "Left", "Up", "Right", "Down", "LStickLeft", "LStickUp", "LStickRight", "LStickDown",
    "RStickLeft", "RStickUp", "RStickRight", "RStickDown",
};

static void put_int(int v) {
    if (v < 0) {
        puts_("-");
        v = -v;
    }
    put_u64((u64)v);
}

static const u32 npad_ids[] = {0, 1, 2, 3, 4, 5, 6, 7, 0x20};

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("padview: press keys and buttons; close the window to stop\n");
    u64 aruid = process_id(), sm, zero = 0;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    call(sm, 1, 0, 0, 0, &zero, 8);
    const char name[8] = "hid";
    if (call(sm, 0, 0, 0, 1, name, 8) != 0) fail("no hid");
    u64 hid = moved;
    call(hid, 1, 0, 0, 0, &aruid, 8);
    call(moved, 0, 0, 0, 0, 0, 0);
    u64 at = find_free();
    if (map_shmem(copied, at) != 0) fail("cannot map hid's shared memory");
    volatile u8 *shm = (volatile u8 *)at;
    struct {
        u32 value, pad;
        u64 aruid;
    } in = {5, 0, aruid};
    call(hid, 1, 0, 0, 109, &in, sizeof in);
    call(hid, 1, (u64)npad_ids, sizeof npad_ids, 102, &aruid, 8);
    in.value = 0x1f;
    call(hid, 1, 0, 0, 100, &in, sizeof in);

    u64 last_buttons = ~0ull;
    int last[4] = {0, 0, 0, 0};
    for (;;) {
        sleep_ns(16000000);
        // Player 1's ring, or the handheld one when that is connected.
        u32 lifo = NPAD + FULL_KEY_LIFO;
        if (*(volatile u32 *)(shm + NPAD + 8 * NPAD_ENTRY)) lifo = NPAD + 8 * NPAD_ENTRY + HANDHELD_LIFO;
        if (*(volatile u64 *)(shm + lifo + 0x18) == 0) continue;
        u64 tail = *(volatile u64 *)(shm + lifo + 0x10);
        volatile u8 *s = shm + lifo + 0x20 + tail * 0x30 + 8;
        u64 buttons = *(volatile u64 *)(s + 8);
        int sticks[4];
        for (int i = 0; i < 4; i++) sticks[i] = *(volatile int *)(s + 0x10 + 4 * i);
        int changed = buttons != last_buttons;
        for (int i = 0; i < 4; i++) changed |= sticks[i] != last[i];
        if (!changed) continue;
        last_buttons = buttons;
        for (int i = 0; i < 4; i++) last[i] = sticks[i];
        puts_("buttons:");
        for (int i = 0; i < 24; i++)
            if (buttons >> i & 1) {
                puts_(" ");
                puts_(names[i]);
            }
        puts_("  left stick ");
        put_int(sticks[0]);
        puts_(",");
        put_int(sticks[1]);
        puts_("  right stick ");
        put_int(sticks[2]);
        puts_(",");
        put_int(sticks[3]);
        puts_("\n");
    }
}
