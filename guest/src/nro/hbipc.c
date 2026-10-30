// A homebrew-format (NRO) test program for IPC (Switchbrew, "SVC", "IPC
// Marshalling" and "Services API"): svcConnectToNamedPort, svcSendSyncRequest
// and the service manager "sm:", with requests laid out in the thread local
// region the way libnx builds them (CMIF with the "SFCI" header 16-byte
// aligned, and TIPC with the command in the header). It prints the call's
// result and the service's result from the reply. No other services exist
// yet, so sm:'s GetServiceHandle is tried only where it must fail. The
// output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_THREAD 0xffff8000ull
#define SFCI 0x49434653u
#define SFCO 0x4f434653u

// ---- system calls ---------------------------------------------------------

#define SVC_HANDLE(name, n)                                                  \
    static u64 name(u64 h) {                                                 \
        register u64 x0 __asm__("x0") = h;                                   \
        __asm__ volatile("svc #" #n : "+r"(x0) : : "memory");                \
        return x0;                                                           \
    }
SVC_HANDLE(start_thread, 0x09)
SVC_HANDLE(close_handle, 0x16)
SVC_HANDLE(send_sync_request, 0x21)

static u64 connect(u64 *h, const char *name) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)name;
    __asm__ volatile("svc #0x1f" : "=r"(x0), "+r"(x1) : : "memory");
    *h = x1;
    return x0;
}

static u64 create_thread(u64 *h, void (*entry)(u64), u64 arg, void *stack_top) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)entry;
    register u64 x2 __asm__("x2") = arg;
    register u64 x3 __asm__("x3") = (u64)stack_top;
    register u64 x4 __asm__("x4") = 0x2c;
    register u64 x5 __asm__("x5") = 1;
    __asm__ volatile("svc #0x08"
                     : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3), "+r"(x4), "+r"(x5)
                     :
                     : "memory");
    *h = x1;
    return x0;
}

__attribute__((noreturn)) static void exit_thread(void) {
    __asm__ volatile("svc #0x0a" ::: "memory");
    __builtin_unreachable();
}

static u64 wait_one(u64 h) {
    u32 list[1] = {(u32)h};
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)list;
    register u64 x2 __asm__("x2") = 1;
    register u64 x3 __asm__("x3") = (u64)-1ll;
    __asm__ volatile("svc #0x18" : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

// ---- messages ---------------------------------------------------------------

// The message buffer: the start of the thread local region.
static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

// Header, then the special header and process ID slot when asked for.
// Returns the index of the first raw data word.
static u32 header(u32 type, u32 raw_words, int pid) {
    volatile u32 *m = msg();
    m[0] = type;
    m[1] = raw_words | (pid ? 1u << 31 : 0);
    if (!pid) return 2;
    m[2] = 1; // send the process ID, no handles
    m[3] = 0;
    m[4] = 0;
    return 5;
}

// A CMIF request: padding to a 16-byte boundary, "SFCI", the command, then
// `size` bytes of arguments; the raw size counts 16 bytes of padding.
static void cmif(u32 type, int pid, u32 cmd, const void *args, u32 size) {
    u32 raw = (((16 + 16 + size + 1) & ~1u) + 3) / 4;
    u32 at = header(type, raw, pid);
    volatile u32 *m = msg();
    while (at % 4) m[at++] = 0;
    m[at] = SFCI;
    m[at + 1] = 0;
    m[at + 2] = cmd;
    m[at + 3] = 0;
    volatile u8 *data = (volatile u8 *)&m[at + 4];
    for (u32 i = 0; i < size; i++) data[i] = ((const u8 *)args)[i];
}

// A TIPC request: command type 16 + cmd, the arguments as raw data.
static void tipc(u32 cmd, int pid, const void *args, u32 size) {
    u32 at = header(16 + cmd, (size + 3) / 4, pid);
    volatile u8 *data = (volatile u8 *)&msg()[at];
    for (u32 i = 0; i < size; i++) data[i] = ((const u8 *)args)[i];
}

static u32 moved;   // handles moved by the last reply
static u32 handles; // their count

// The result in a reply: after "SFCO" at the first 16-byte boundary of the
// raw data for CMIF, the first raw word for TIPC.
static u32 reply(int is_cmif) {
    volatile u32 *m = msg();
    u32 at = 2;
    handles = 0;
    if (m[1] >> 31) {
        u32 special = m[2];
        if (special & 0x1f) fail("a reply with a process ID or copied handles");
        handles = special >> 5 & 0xf;
        if (handles) moved = m[3];
        at = 3 + handles;
    }
    if (!is_cmif) return m[at];
    while (at % 4) at++;
    if (m[at] != SFCO) fail("no SFCO header in the reply");
    return m[at + 2];
}

static void line(const char *label, u64 call, int is_cmif) {
    puts_(label);
    put_hex(call);
    if (call == 0) {
        puts_(", service result ");
        put_hex(reply(is_cmif));
    }
    puts_("\n");
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

// ---- requests -----------------------------------------------------------------

static const u64 zero = 0;

static u64 register_cmif(u64 h) {
    cmif(4, 1, 0, &zero, 8); // RegisterClient: process ID, reserved u64
    return send_sync_request(h);
}

static u64 get_service_cmif(u64 h, const char *name) {
    u8 n[8] = {0};
    for (int i = 0; i < 8 && name[i]; i++) n[i] = (u8)name[i];
    cmif(4, 0, 1, n, 8);
    return send_sync_request(h);
}

static volatile u64 worker_session;
static volatile u64 worker_call;
static volatile u32 worker_result;
static u8 worker_stack[0x4000] __attribute__((aligned(16)));

// Registers over the session from another thread, with its own region.
static void worker(u64 arg) {
    (void)arg;
    tipc(0, 1, 0, 0);
    worker_call = send_sync_request(worker_session);
    worker_result = reply(0);
    exit_thread();
}

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew IPC test\n");
    u64 sm, sm2, unused;

    // svcConnectToNamedPort.
    hex("connect with an unreadable name: ", connect(&unused, (const char *)0x10));
    hex("connect with a 12-character name: ", connect(&unused, "abcdefghijkl"));
    hex("connect to sm: ", connect(&sm, "sm:"));
    yes_no("the session handle is a real handle: ", sm != 0 && sm >> 30 == 0);

    // sm: over CMIF.
    line("get a service before registering: ", get_service_cmif(sm, "fsp-srv"), 1);
    line("register: ", register_cmif(sm), 1);
    yes_no("the reply moved no handles: ", handles == 0);
    line("get a service with an empty name: ", get_service_cmif(sm, ""), 1);
    const u8 gap[8] = {'a', 0, 'b'};
    cmif(4, 0, 1, gap, 8);
    line("get a service with a gap in its name: ", send_sync_request(sm), 1);

    // sm: over TIPC, on a second session, from a second thread.
    hex("connect to sm: again: ", connect(&sm2, "sm:"));
    yes_no("a different handle: ", sm2 != sm);
    tipc(1, 0, "fsp-srv", 8);
    line("TIPC get a service before registering: ", send_sync_request(sm2), 0);
    msg()[0] = 0x6b657374; // kestrel's marker in this thread's region
    worker_session = sm2;
    u64 t;
    hex("create a thread: ", create_thread(&t, worker, 0, worker_stack + sizeof worker_stack));
    start_thread(t);
    hex("wait for it: ", wait_one(t));
    close_handle(t);
    hex("TIPC register from that thread: ", worker_call);
    hex("its service result: ", worker_result);
    yes_no("this thread's region is untouched: ", msg()[0] == 0x6b657374);
    tipc(1, 0, &zero, 8);
    line("TIPC get a service with an empty name: ", send_sync_request(sm2), 0);

    // Closing.
    msg()[0] = 2; // a close request: command type 2, nothing else
    msg()[1] = 0;
    hex("close request: ", send_sync_request(sm2));
    hex("register after the close request: ", register_cmif(sm2));
    hex("close the handle: ", close_handle(sm2));
    hex("register on the closed handle: ", register_cmif(sm2));
    hex("request on the current thread pseudo-handle: ", send_sync_request(CUR_THREAD));
    line("the first session still works: ", get_service_cmif(sm, ""), 1);
    hex("close it: ", close_handle(sm));
}
