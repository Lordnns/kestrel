// A homebrew-format (NRO) test program for power state and error reports
// (Switchbrew, "PTM services", "Fatal services" and "SVC"): the psm
// requests libnx's psm functions send, laid out as libnx lays them out,
// svcBreak with the notification-only flag (it returns), and a fatal error
// thrown with the ErrorReport policy (only a report: the program goes on).
// The output is modelled in scripts/gen-expected.py.
#include "hb.h"

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

static u64 svc_break(u64 reason, const void *arg, u64 size) {
    register u64 x0 __asm__("x0") = reason;
    register u64 x1 __asm__("x1") = (u64)arg;
    register u64 x2 __asm__("x2") = size;
    __asm__ volatile("svc #0x26" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory");
    return x0;
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

static u64 out_u64(void) {
    u64 v = 0;
    for (int i = 7; i >= 0; i--) v = v << 8 | out[i];
    return v;
}

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

static u64 get_service(u64 sm, const char *name) {
    u8 n[8] = {0};
    for (int i = 0; i < 8 && name[i]; i++) n[i] = (u8)name[i];
    if (call(sm, 1, n, 8) != 0 || !moved) fail("GetServiceHandle failed");
    return moved;
}

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew power test\n");

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    hex("sm: register: ", call_with(sm, 1, 0, &zero, 8));
    u64 psm = get_service(sm, "psm");

    // psmGetBatteryChargePercentage, psmGetChargerType,
    // psmIsBatteryChargingEnabled, psmGetBatteryVoltageState,
    // psmIsEnoughPowerSupplied.
    hex("GetBatteryChargePercentage: ", call(psm, 0, 0, 0));
    dec("percent: ", out[0] | out[1] << 8 | out[2] << 16 | (u32)out[3] << 24);
    hex("GetChargerType: ", call(psm, 1, 0, 0));
    dec("charger type: ", out[0]);
    hex("IsBatteryChargingEnabled: ", call(psm, 4, 0, 0));
    yes_no("charging enabled: ", out[0] & 1);
    hex("GetBatteryVoltageState: ", call(psm, 12, 0, 0));
    dec("voltage state: ", out[0]);
    hex("IsEnoughPowerSupplied: ", call(psm, 14, 0, 0));
    yes_no("enough power: ", out[0] & 1);
    // The doubles, compared bit for bit with 100.0.
    hex("GetRawBatteryChargePercentage: ", call(psm, 13, 0, 0));
    hex("its bits: ", out_u64());
    hex("GetBatteryAgePercentage: ", call(psm, 15, 0, 0));
    hex("its bits: ", out_u64());

    // psmBindStateChangeEvent: OpenSession, the three bools, the event.
    hex("OpenSession: ", call(psm, 7, 0, 0));
    u64 session = moved;
    yes_no("it moved a session handle: ", session != 0);
    const u8 on = 1;
    hex("SetChargerTypeChangeEventEnabled: ", call(session, 2, &on, 1));
    hex("SetPowerSupplyChangeEventEnabled: ", call(session, 3, &on, 1));
    hex("SetBatteryVoltageStateChangeEventEnabled: ", call(session, 4, &on, 1));
    hex("BindStateChangeEvent: ", call(session, 0, 0, 0));
    u64 event = copied;
    yes_no("it copied a handle: ", event != 0);
    hex("UnbindStateChangeEvent: ", call(session, 1, 0, 0));

    // A debugger notification: svcBreak returns.
    const u32 result = 345 | 33 << 9; // libnx: InitFail_FS
    hex("svcBreak, notification only: ", svc_break(0x80000000u, &result, 4));

    // fatalThrowWithPolicy(result, FatalPolicy_ErrorReport): a report only.
    u64 fatal = get_service(sm, "fatal:u");
    struct {
        u32 result, policy;
        u64 pid;
    } in = {result, 1, 0};
    hex("ThrowFatalWithPolicy, report only: ", call_with(fatal, 1, 1, &in, sizeof in));
    puts_("the program goes on\n");

    hex("close the event: ", close_handle(event));
    hex("close IPsmSession: ", close_handle(session));
    hex("close fatal:u: ", close_handle(fatal));
    hex("close psm: ", close_handle(psm));
    hex("close sm: ", close_handle(sm));
}
