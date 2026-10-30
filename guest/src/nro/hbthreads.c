// A homebrew-format (NRO) test program for threads (Switchbrew, "SVC"):
// svcCreateThread, svcStartThread, svcExitThread, svcSleepThread, the
// priority and core mask calls and svcGetCurrentProcessorNumber. Besides
// result codes it checks what threads on a multi-core console can rely
// on: a new thread gets its argument, stack and its own thread local
// region; a higher priority on the same core runs first; threads spinning
// on different cores all make progress; atomic updates from two cores do
// not lose increments; sleeping advances the system tick. The output is
// modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_THREAD 0xffff8000ull

// Calls taking one handle in W0 and returning only the result in W0.
#define SVC_HANDLE(name, n)                                                  \
    static u64 name(u64 h) {                                                 \
        register u64 x0 __asm__("x0") = h;                                   \
        __asm__ volatile("svc #" #n : "+r"(x0) : : "memory");                \
        return x0;                                                           \
    }
SVC_HANDLE(start_thread, 0x09)
SVC_HANDLE(close_handle, 0x16)

static u64 create_thread(u64 *h, void (*entry)(u64), u64 arg, void *stack_top, i64 prio, i64 core) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)entry;
    register u64 x2 __asm__("x2") = arg;
    register u64 x3 __asm__("x3") = (u64)stack_top;
    register u64 x4 __asm__("x4") = (u64)prio;
    register u64 x5 __asm__("x5") = (u64)core;
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

static void sleep_ns(i64 ns) {
    register u64 x0 __asm__("x0") = (u64)ns;
    __asm__ volatile("svc #0x0b" : "+r"(x0) : : "memory");
}

static u64 tick(void) {
    register u64 x0 __asm__("x0");
    __asm__ volatile("svc #0x1e" : "=r"(x0) : : "memory");
    return x0;
}

// svcGetThreadPriority and svcGetThreadId: handle in W1, value out in X1.
#define SVC_GET(name, n)                                                     \
    static u64 name(u64 *v, u64 h) {                                         \
        register u64 x0 __asm__("x0");                                       \
        register u64 x1 __asm__("x1") = h;                                   \
        __asm__ volatile("svc #" #n : "=r"(x0), "+r"(x1) : : "memory");      \
        *v = x1;                                                             \
        return x0;                                                           \
    }
SVC_GET(get_priority, 0x0c)
SVC_GET(get_thread_id, 0x25)

static u64 set_priority(u64 h, i64 prio) {
    register u64 x0 __asm__("x0") = h;
    register u64 x1 __asm__("x1") = (u64)prio;
    __asm__ volatile("svc #0x0d" : "+r"(x0), "+r"(x1) : : "memory");
    return x0;
}

static u64 get_core_mask(u64 *core, u64 *mask, u64 h) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1");
    register u64 x2 __asm__("x2") = h;
    __asm__ volatile("svc #0x0e" : "=r"(x0), "=r"(x1), "+r"(x2) : : "memory");
    *core = (u32)x1;
    *mask = x2;
    return x0;
}

static u64 set_core_mask(u64 h, i64 core, u64 mask) {
    register u64 x0 __asm__("x0") = h;
    register u64 x1 __asm__("x1") = (u64)core;
    register u64 x2 __asm__("x2") = mask;
    __asm__ volatile("svc #0x0f" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory");
    return x0;
}

static u64 processor(void) {
    register u64 x0 __asm__("x0");
    __asm__ volatile("svc #0x10" : "=r"(x0) : : "memory");
    return (u32)x0;
}

static u64 tls(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return v;
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

static void line(const char *label, u64 v) {
    puts_(label);
    put_u64(v);
    puts_("\n");
}

#define STACK 0x4000
static u8 stacks[6][STACK] __attribute__((aligned(16)));
static void *top(int i) { return stacks[i] + STACK; }

// What the first worker saw.
static volatile u64 seen_arg, seen_tls, seen_core, seen_id, seen_sp, worker_done;
static volatile u64 tls_zero;

static void worker(u64 arg) {
    u64 sp;
    __asm__ volatile("mov %0, sp" : "=r"(sp));
    seen_arg = arg;
    seen_tls = tls();
    tls_zero = 1;
    for (int i = 0; i < 0x200 / 8; i++)
        if (((volatile u64 *)seen_tls)[i] != 0) tls_zero = 0;
    seen_core = processor();
    get_thread_id((u64 *)&seen_id, CUR_THREAD);
    seen_sp = sp;
    worker_done = 1;
    exit_thread();
}

// A log of which thread ran when, for the priority checks.
static volatile char order[8];
static volatile u64 order_len;
static void log_char(char c) { order[order_len++] = c; }

static void high(u64 c) {
    log_char((char)c);
    exit_thread();
}

// Spinning on two cores: each waits for the other.
static volatile u64 spin_count, spin_stop;
static void spinner(u64 unused) {
    (void)unused;
    while (!spin_stop) spin_count++;
    exit_thread();
}

// Atomic increments from two cores.
#define INCREMENTS 20000
static u64 shared;
static volatile u64 adders_done;
static void adder(u64 unused) {
    (void)unused;
    for (int i = 0; i < INCREMENTS; i++) __atomic_fetch_add(&shared, 1, __ATOMIC_SEQ_CST);
    __atomic_fetch_add((u64 *)&adders_done, 1, __ATOMIC_SEQ_CST);
    exit_thread();
}

// Yield until *flag reaches want, so other cores get turns.
static void wait_for(volatile u64 *flag, u64 want, const char *what) {
    for (u64 i = 0; *flag < want; i++) {
        if (i > 1000000) fail(what);
        sleep_ns(0);
    }
}

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew thread test\n");
    u64 h, v, unused;

    // Arguments.
    hex("create with priority 0x40: ", create_thread(&unused, worker, 0, top(0), 0x40, 0));
    hex("create with priority -1: ", create_thread(&unused, worker, 0, top(0), -1, 0));
    hex("create on processor 4: ", create_thread(&unused, worker, 0, top(0), 0x2c, 4));
    hex("create on processor -1: ", create_thread(&unused, worker, 0, top(0), 0x2c, -1));

    // A worker on core 1 sees its argument, stack and own zeroed TLS.
    hex("create a worker on core 1: ", create_thread(&h, worker, 0x1234, top(0), 0x2c, 1));
    yes_no("not running before it is started: ", worker_done == 0);
    hex("start it: ", start_thread(h));
    hex("start it again: ", start_thread(h));
    wait_for(&worker_done, 1, "the worker never ran");
    yes_no("worker got its argument: ", seen_arg == 0x1234);
    yes_no("worker started on its stack: ", seen_sp == (u64)top(0));
    yes_no("worker has its own thread local region: ", seen_tls != tls() && seen_tls != 0);
    yes_no("its thread local region started zeroed: ", tls_zero);
    line("worker's processor: ", seen_core);
    hex("thread id of the exited worker: ", get_thread_id(&v, h));
    yes_no("matches what the worker saw: ", v == seen_id);
    u64 main_id;
    get_thread_id(&main_id, CUR_THREAD);
    yes_no("differs from the main thread's: ", v != main_id);
    hex("close the worker: ", close_handle(h));
    line("main thread's processor: ", processor());

    // Priorities on the main thread's core.
    u64 my_prio;
    get_priority(&my_prio, CUR_THREAD);
    hex("create a higher priority thread on this core: ",
        create_thread(&h, high, 'H', top(1), (i64)my_prio - 1, -2));
    start_thread(h);
    log_char('M');
    close_handle(h);
    hex("create a lower priority thread on this core: ",
        create_thread(&h, high, 'L', top(1), (i64)my_prio + 1, -2));
    start_thread(h);
    log_char('M');
    sleep_ns(0); // yielding only lets equal priorities run
    log_char('Y');
    sleep_ns(1000000); // sleeping lets it run
    log_char('S');
    close_handle(h);
    puts_("run order: ");
    for (u64 i = 0; i < order_len; i++) {
        char s[2] = {order[i], 0};
        puts_(s);
    }
    puts_("\n");

    // Priority and core mask calls on a thread never started.
    hex("create a thread on core 3: ", create_thread(&h, worker, 0, top(2), 0x30, 3));
    hex("get priority: ", get_priority(&v, h));
    hex("priority: ", v);
    hex("set priority 0x3f: ", set_priority(h, 0x3f));
    get_priority(&v, h);
    hex("priority now: ", v);
    hex("set priority 0x40: ", set_priority(h, 0x40));
    u64 core, mask;
    hex("get core mask: ", get_core_mask(&core, &mask, h));
    line("ideal core: ", core);
    hex("affinity mask: ", mask);
    hex("set core 2 in mask 0b0110: ", set_core_mask(h, 2, 6));
    get_core_mask(&core, &mask, h);
    line("ideal core now: ", core);
    hex("affinity mask now: ", mask);
    hex("set core 1 in mask 0b0100: ", set_core_mask(h, 1, 4));
    hex("set core 4: ", set_core_mask(h, 4, 0x10));
    hex("close it unstarted: ", close_handle(h));

    // Two threads spinning on different cores both progress.
    hex("create a spinner on core 2: ", create_thread(&h, spinner, 0, top(3), 0x2c, 2));
    start_thread(h);
    u64 i = 0;
    while (spin_count < 100000) // no yield: only time slices let it run
        if (++i > 100000000) fail("the spinner never ran");
    spin_stop = 1;
    yes_no("main and spinner both ran without yielding: ", 1);
    close_handle(h);

    // Atomic increments from two cores across many time slices.
    u64 a, b;
    create_thread(&a, adder, 0, top(4), 0x2c, 1);
    create_thread(&b, adder, 0, top(5), 0x2c, 2);
    start_thread(a);
    start_thread(b);
    wait_for(&adders_done, 2, "the adders never finished");
    line("atomic counter from two threads: ", shared);
    close_handle(a);
    close_handle(b);

    // Sleeping advances the system tick: 1 ms is 19200 ticks at 19.2 MHz.
    u64 before = tick();
    sleep_ns(1000000);
    yes_no("sleeping 1 ms advanced the tick by at least 19200: ", tick() - before >= 19200);
}
