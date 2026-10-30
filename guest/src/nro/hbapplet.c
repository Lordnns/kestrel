// A homebrew-format (NRO) test program for the first services: set:sys,
// apm and appletOE (Switchbrew, "Settings services" and "Applet Manager
// services"). It sends the requests libnx's startup sends for an
// application (`__appInit`, `appletInitialize`, `apmInitialize`), laid out
// as libnx lays them out: a C buffer for the firmware version, appletOE
// converted to a CMIF domain, OpenApplicationProxy with the process ID and
// a copied current-process handle, and the interfaces it hands out. The
// output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001u
#define SFCI 0x49434653u
#define SFCO 0x4f434653u

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

// svcWaitSynchronization on one handle with timeout 0.
static u64 poll(u32 h) {
    u32 list[1] = {h};
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)list;
    register u64 x2 __asm__("x2") = 1;
    register u64 x3 __asm__("x3") = 0;
    __asm__ volatile("svc #0x18" : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

// ---- messages ---------------------------------------------------------------

static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

typedef struct {
    int pid;     // send the process ID
    u32 copy;    // a handle to copy, or 0
    u32 object;  // the domain object, or 0 outside a domain
    u64 recv;    // a receive buffer (C) for the output, or 0
    u32 recv_size;
} options;

// A CMIF request as libnx's cmifMakeRequest lays it out.
static void request(u32 type, options o, u32 cmd, const void *args, u32 size) {
    volatile u32 *m = msg();
    u32 bytes = 16 + (o.object ? 16 : 0) + 16 + size;
    u32 raw = (((bytes + 1) & ~1u) + 3) / 4;
    int special = o.pid || o.copy;
    m[0] = type;
    m[1] = raw | (o.recv ? 3u << 10 : 0) | (special ? 1u << 31 : 0);
    u32 at = 2;
    if (special) {
        m[at++] = (o.pid ? 1u : 0) | (o.copy ? 1u << 1 : 0);
        if (o.pid) {
            m[at++] = 0;
            m[at++] = 0;
        }
        if (o.copy) m[at++] = o.copy;
    }
    u32 raw_start = at;
    while (at % 4) m[at++] = 0;
    if (o.object) {
        m[at++] = 1 | (16 + size) << 16; // SendMessage, no input objects
        m[at++] = o.object;
        m[at++] = 0;
        m[at++] = 0;
    }
    m[at] = SFCI;
    m[at + 1] = 0;
    m[at + 2] = cmd;
    m[at + 3] = 0;
    volatile u8 *data = (volatile u8 *)&m[at + 4];
    for (u32 i = 0; i < size; i++) data[i] = ((const u8 *)args)[i];
    if (o.recv) {
        u32 list = raw_start + raw;
        m[list] = (u32)o.recv;
        m[list + 1] = (u32)(o.recv >> 32) | o.recv_size << 16;
    }
}

// The last reply, as libnx's cmifParseResponse reads it.
static u32 copied, moved, object;
static volatile u8 *out;

static u32 reply(int domain, u32 size) {
    volatile u32 *m = msg();
    u32 at = 2;
    copied = moved = object = 0;
    if (m[1] >> 31) {
        u32 special = m[2], ncopy = special >> 1 & 0xf, nmove = special >> 5 & 0xf;
        if (special & 1) fail("a reply with a process ID");
        at = 3;
        if (ncopy) copied = m[at];
        at += ncopy;
        if (nmove) moved = m[at];
        at += nmove;
    }
    while (at % 4) at++;
    u32 objects = 0;
    if (domain) {
        objects = m[at];
        at += 4;
    }
    if (m[at] != SFCO) fail("no SFCO header in the reply");
    out = (volatile u8 *)&m[at + 4];
    if (objects) object = *(volatile u32 *)(out + size);
    return m[at + 2];
}

static u32 call(u64 h, options o, u32 cmd, const void *args, u32 size, u32 out_size) {
    request(4, o, cmd, args, size);
    u64 rc = send_sync_request(h);
    if (rc != 0) fail("svcSendSyncRequest failed");
    return reply(o.object != 0, out_size);
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
    if (call(sm, o, 1, n, 8, 0) != 0 || !moved) fail("GetServiceHandle failed");
    return moved;
}

static u8 firmware[0x100];

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew applet test\n");
    const u64 zero = 0;
    options none = {0};

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    options pid = {.pid = 1};
    hex("sm: register: ", call(sm, pid, 0, &zero, 8, 0));

    // set:sys GetFirmwareVersion2, through a C buffer.
    u64 setsys = get_service(sm, "set:sys");
    options c = {.recv = (u64)firmware, .recv_size = sizeof firmware};
    hex("set:sys GetFirmwareVersion2: ", call(setsys, c, 4, 0, 0, 0));
    puts_("version: ");
    put_u64(firmware[0]);
    puts_(".");
    put_u64(firmware[1]);
    puts_(".");
    put_u64(firmware[2]);
    puts_(", display version ");
    puts_((const char *)&firmware[0x68]);
    puts_("\n");
    hex("close set:sys: ", close_handle(setsys));

    // apm.
    u64 apm = get_service(sm, "apm");
    hex("apm OpenSession: ", call(apm, none, 0, 0, 0, 0));
    yes_no("it moved a session handle: ", moved != 0);
    u64 apm_session = moved;
    hex("apm GetPerformanceMode: ", call(apm, none, 1, 0, 0, 4));
    hex("performance mode: ", *(volatile u32 *)out);

    // appletOE as a domain.
    u64 am = get_service(sm, "appletOE");
    request(5, none, 0, 0, 0); // ConvertCurrentObjectToDomain
    hex("convert appletOE to a domain: ", send_sync_request(am));
    hex("its result: ", reply(0, 4));
    u32 root = *(volatile u32 *)out;
    yes_no("the object ID is not 0: ", root != 0);
    options open = {.pid = 1, .copy = CUR_PROCESS, .object = root};
    hex("OpenApplicationProxy: ", call(am, open, 0, &zero, 8, 0));
    u32 proxy = object;
    yes_no("a new domain object: ", proxy != 0 && proxy != root);

    static const u32 getters[] = {20, 11, 0, 1, 2, 3, 4, 1000};
    u32 objects[8];
    int distinct = 1;
    options at_proxy = {.object = proxy};
    for (int i = 0; i < 8; i++) {
        if (call(am, at_proxy, getters[i], 0, 0, 0) != 0) fail("an IApplicationProxy getter failed");
        objects[i] = object;
        for (int j = 0; j < i; j++) distinct &= objects[j] != object;
        distinct &= object != 0 && object != proxy && object != root;
    }
    yes_no("eight getters gave eight new objects: ", distinct);
    options functions = {.object = objects[0]}, state = {.object = objects[2]},
            self = {.object = objects[3]}, window = {.object = objects[4]};

    hex("GetAppletResourceUserId: ", call(am, window, 1, 0, 0, 8));
    yes_no("it is the process ID: ", *(volatile u64 *)out == process_id());
    hex("GetAccumulatedSuspendedTickChangedEvent: ", call(am, self, 91, 0, 0, 0));
    u32 tick_event = copied;
    hex("GetEventHandle: ", call(am, state, 0, 0, 0, 0));
    u32 message_event = copied;
    yes_no("two event handles: ", tick_event && message_event && tick_event != message_event);
    hex("poll the message event: ", poll(message_event));
    hex("poll the suspended tick event: ", poll(tick_event));
    hex("GetCurrentFocusState: ", call(am, state, 9, 0, 0, 1));
    hex("focus state: ", out[0]);
    hex("ReceiveMessage: ", call(am, state, 1, 0, 0, 4));
    hex("AcquireForegroundRights: ", call(am, window, 10, 0, 0, 0));
    const u8 focus[3] = {0, 0, 1};
    hex("SetFocusHandlingMode: ", call(am, self, 13, focus, 3, 0));
    const u8 flag = 0, on = 1;
    hex("SetOutOfFocusSuspendingEnabled: ", call(am, self, 16, &flag, 1, 0));
    hex("NotifyRunning: ", call(am, functions, 40, 0, 0, 1));
    hex("GetOperationMode: ", call(am, state, 5, 0, 0, 1));
    hex("operation mode: ", out[0]);
    hex("GetPerformanceMode: ", call(am, state, 6, 0, 0, 4));
    hex("performance mode: ", *(volatile u32 *)out);
    hex("SetOperationModeChangedNotification: ", call(am, self, 11, &on, 1, 0));
    hex("SetPerformanceModeChangedNotification: ", call(am, self, 12, &on, 1, 0));

    // Closing, as appletExit and serviceClose do: each object, then the
    // session.
    int closed = 1;
    for (int i = 7; i >= 0; i--) {
        volatile u32 *m = msg();
        m[0] = 4;
        m[1] = 8;
        m[2] = m[3] = 0;
        m[4] = 2; // Close
        m[5] = objects[i];
        m[6] = m[7] = 0;
        closed &= send_sync_request(am) == 0;
    }
    yes_no("closed the eight objects: ", closed);
    hex("close the event handles: ", close_handle(tick_event) | close_handle(message_event));
    hex("close appletOE: ", close_handle(am));
    hex("close the apm session: ", close_handle(apm_session));
    hex("close apm: ", close_handle(apm));
    hex("close sm: ", close_handle(sm));
}
