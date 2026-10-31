// A homebrew-format (NRO) test program for synchronisation (Switchbrew,
// "SVC"): svcWaitSynchronization on events and threads, timeouts,
// svcCancelSynchronization, mutexes and condition variables built on
// svcArbitrateLock, svcArbitrateUnlock, svcWaitProcessWideKeyAtomic and
// svcSignalProcessWideKey the way libnx uses them (a mutex word holds the
// owner's thread handle, bit 30 marks waiters), and address arbitration.
// Four threads on four cores update shared data under a mutex while time
// slices interrupt them, and a producer and a consumer hand over items
// through condition variables. The output is modelled in
// scripts/gen-expected.py.
#include "hb.h"

#define CUR_THREAD 0xffff8000ull
#define CUR_PROCESS 0xffff8001ull
#define FOREVER (-1ll)

// ---- system calls ---------------------------------------------------------

#define SVC_HANDLE(name, n)                                                  \
    static u64 name(u64 h) {                                                 \
        register u64 x0 __asm__("x0") = h;                                   \
        __asm__ volatile("svc #" #n : "+r"(x0) : : "memory");                \
        return x0;                                                           \
    }
SVC_HANDLE(start_thread, 0x09)
SVC_HANDLE(signal_event, 0x11)
SVC_HANDLE(close_handle, 0x16)
SVC_HANDLE(cancel_sync, 0x19)

static u64 create_thread(u64 *h, void (*entry)(u64), u64 arg, void *stack_top, i64 core) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)entry;
    register u64 x2 __asm__("x2") = arg;
    register u64 x3 __asm__("x3") = (u64)stack_top;
    register u64 x4 __asm__("x4") = 0x2c;
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

static u64 create_event(u64 *w, u64 *r) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1");
    register u64 x2 __asm__("x2");
    __asm__ volatile("svc #0x45" : "=r"(x0), "=r"(x1), "=r"(x2) : : "memory");
    *w = x1;
    *r = x2;
    return x0;
}

// svcWaitSynchronization; *index is written only on success.
static u64 wait_sync(u64 *index, const u32 *handles, i64 count, i64 timeout) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)handles;
    register u64 x2 __asm__("x2") = (u64)count;
    register u64 x3 __asm__("x3") = (u64)timeout;
    __asm__ volatile("svc #0x18" : "=r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    if (x0 == 0) *index = x1;
    return x0;
}

static u64 wait_one(u64 h, i64 timeout) {
    u32 list[1] = {(u32)h};
    u64 index;
    return wait_sync(&index, list, 1, timeout);
}

static u64 arbitrate_lock(u32 owner, volatile u32 *word, u32 me) {
    register u64 x0 __asm__("x0") = owner;
    register u64 x1 __asm__("x1") = (u64)word;
    register u64 x2 __asm__("x2") = me;
    __asm__ volatile("svc #0x1a" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory");
    return x0;
}

static u64 arbitrate_unlock(volatile u32 *word) {
    register u64 x0 __asm__("x0") = (u64)word;
    __asm__ volatile("svc #0x1b" : "+r"(x0) : : "memory");
    return x0;
}

static u64 wait_key(volatile u32 *mutex, volatile u32 *cv, u32 me, i64 timeout) {
    register u64 x0 __asm__("x0") = (u64)mutex;
    register u64 x1 __asm__("x1") = (u64)cv;
    register u64 x2 __asm__("x2") = me;
    register u64 x3 __asm__("x3") = (u64)timeout;
    __asm__ volatile("svc #0x1c" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

static void signal_key(volatile u32 *cv, i64 count) {
    register u64 x0 __asm__("x0") = (u64)cv;
    register u64 x1 __asm__("x1") = (u64)count;
    __asm__ volatile("svc #0x1d" : "+r"(x0), "+r"(x1) : : "memory");
}

static u64 wait_address(volatile int *a, u64 kind, i64 value, i64 timeout) {
    register u64 x0 __asm__("x0") = (u64)a;
    register u64 x1 __asm__("x1") = kind;
    register u64 x2 __asm__("x2") = (u64)value;
    register u64 x3 __asm__("x3") = (u64)timeout;
    __asm__ volatile("svc #0x34" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

static u64 signal_address(volatile int *a, u64 kind, i64 value, i64 count) {
    register u64 x0 __asm__("x0") = (u64)a;
    register u64 x1 __asm__("x1") = kind;
    register u64 x2 __asm__("x2") = (u64)value;
    register u64 x3 __asm__("x3") = (u64)count;
    __asm__ volatile("svc #0x35" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
    return x0;
}

// ---- a mutex and a condition variable on the kernel's arbitration -------

#define WAIT_BIT 0x40000000u

static void mutex_lock(volatile u32 *m, u32 me) {
    for (;;) {
        u32 seen = 0;
        if (__atomic_compare_exchange_n(m, &seen, me, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
            return;
        // Held: make sure the owner sees a waiter, then let the kernel wait.
        if (!(seen & WAIT_BIT) &&
            !__atomic_compare_exchange_n(m, &seen, seen | WAIT_BIT, 0, __ATOMIC_RELAXED,
                                         __ATOMIC_RELAXED))
            continue;
        if (arbitrate_lock(seen & ~WAIT_BIT, m, me) != 0) fail("svcArbitrateLock failed");
        if ((__atomic_load_n(m, __ATOMIC_ACQUIRE) & ~WAIT_BIT) == me) return;
    }
}

static void mutex_unlock(volatile u32 *m, u32 me) {
    u32 expect = me;
    if (__atomic_compare_exchange_n(m, &expect, 0, 0, __ATOMIC_RELEASE, __ATOMIC_RELAXED))
        return;
    if (arbitrate_unlock(m) != 0) fail("svcArbitrateUnlock failed");
}

// Wait on cv with m held; holds m again on return (also after a timeout).
static u64 cond_wait(volatile u32 *cv, volatile u32 *m, u32 me, i64 timeout) {
    u64 rc = wait_key(m, cv, me, timeout);
    if (rc == 0xea01) mutex_lock(m, me);
    return rc;
}

// ---- output --------------------------------------------------------------

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

// ---- threads ---------------------------------------------------------------

#define STACK 0x4000
static u8 stacks[4][STACK] __attribute__((aligned(16)));
static void *top(int i) { return stacks[i] + STACK; }

// Thread handles by index, set before the threads start, as their mutex tags.
static volatile u32 handles[4];

// An event waiter: records how its wait ended.
static volatile u64 waiter_rc = 0xffff, waiter_index = 0xffff;
static volatile u32 waiter_event;
static void event_waiter(u64 unused) {
    (void)unused;
    u32 list[1] = {waiter_event};
    u64 index = 0xffff;
    waiter_rc = wait_sync(&index, list, 1, FOREVER);
    waiter_index = index;
    exit_thread();
}

// Counting under a mutex, slowly enough that time slices land inside.
#define ROUNDS 2000
static volatile u32 count_mutex;
static volatile u64 counter;
static void counting(u64 me_index) {
    u32 me = handles[me_index];
    for (int i = 0; i < ROUNDS; i++) {
        mutex_lock(&count_mutex, me);
        u64 v = counter;
        for (volatile int d = 0; d < 20; d++) {}
        counter = v + 1;
        mutex_unlock(&count_mutex, me);
    }
    exit_thread();
}

// A one-slot queue between a producer and a consumer.
#define ITEMS 100
static volatile u32 queue_mutex, not_empty, not_full;
static volatile u64 slot, slot_full, consumed_sum, consumed;
static void consumer(u64 me_index) {
    u32 me = handles[me_index];
    for (int i = 0; i < ITEMS; i++) {
        mutex_lock(&queue_mutex, me);
        while (!slot_full) cond_wait(&not_empty, &queue_mutex, me, FOREVER);
        consumed_sum += slot;
        consumed++;
        slot_full = 0;
        signal_key(&not_full, 1);
        mutex_unlock(&queue_mutex, me);
    }
    exit_thread();
}

// Address arbitration. The waiter says when it is about to wait, so the
// main thread's 1 ms sleep only has to cover the call itself, even when
// its core starts late (cores on a busy host's threads).
static volatile int flag;
static volatile u64 address_rc = 0xffff, address_ready;
static void address_waiter(u64 unused) {
    (void)unused;
    address_ready = 1;
    address_rc = wait_address(&flag, 2, 0, FOREVER); // WaitIfEqual 0
    exit_thread();
}

static u64 start(int i, void (*entry)(u64), u64 arg, i64 core) {
    u64 h;
    if (create_thread(&h, entry, arg, top(i), core) != 0) fail("svcCreateThread failed");
    handles[i] = (u32)h;
    start_thread(h);
    return h;
}

void hb_main(const config_entry *config, u64 x1) {
    (void)x1;
    puts_("homebrew synchronisation test\n");
    u32 main_handle = 0;
    for (const config_entry *e = config; e->key != 0; e++)
        if (e->key == 1) main_handle = (u32)e->value[0];

    // Waiting: arguments.
    u64 w, r, index;
    create_event(&w, &r);
    u32 list[0x41];
    for (int i = 0; i < 0x41; i++) list[i] = (u32)r;
    hex("wait on 0x41 handles: ", wait_sync(&index, list, 0x41, 0));
    hex("wait with an unreadable list: ", wait_sync(&index, (const u32 *)8, 1, 0));
    hex("wait on the current thread pseudo-handle: ", wait_one(CUR_THREAD, 0));
    hex("wait on the writable side: ", wait_one(w, 0));
    hex("poll an unsignaled event: ", wait_one(r, 0));
    u64 before = tick();
    hex("wait 1 ms on it: ", wait_one(r, 1000000));
    yes_no("the tick advanced by at least 19200: ", tick() - before >= 19200);
    u64 w2, r2;
    create_event(&w2, &r2);
    signal_event(w2);
    u32 pair[2] = {(u32)r, (u32)r2};
    hex("wait on two, the second signaled: ", wait_sync(&index, pair, 2, FOREVER));
    line("index: ", index);
    hex("again, it stays signaled: ", wait_sync(&index, pair, 2, 0));

    // A thread waiting on an event is woken by the signal; waiting on a
    // thread ends when it exits.
    waiter_event = (u32)r;
    u64 t = start(0, event_waiter, 0, 1);
    sleep_ns(1000000);
    yes_no("the waiter is still waiting: ", waiter_rc == 0xffff);
    signal_event(w);
    hex("wait for the waiter to exit: ", wait_one(t, FOREVER));
    hex("the waiter's result: ", waiter_rc);
    line("the waiter's index: ", waiter_index);
    close_handle(t);

    // Cancelling.
    u64 w3, r3;
    create_event(&w3, &r3);
    hex("cancel the current thread: ", cancel_sync(CUR_THREAD));
    hex("its next wait: ", wait_one(r3, 0));
    hex("the wait after that: ", wait_one(r3, 0));
    waiter_event = (u32)r3;
    waiter_rc = 0xffff;
    t = start(0, event_waiter, 0, 1);
    sleep_ns(1000000);
    hex("cancel the waiting thread: ", cancel_sync(t));
    wait_one(t, FOREVER);
    hex("its wait ended with: ", waiter_rc);
    close_handle(t);

    // A mutex shared by four threads on four cores.
    handles[3] = main_handle;
    u64 c1 = start(0, counting, 0, 1);
    u64 c2 = start(1, counting, 1, 2);
    u64 c3 = start(2, counting, 2, 3);
    u32 me = main_handle;
    for (int i = 0; i < ROUNDS; i++) {
        mutex_lock(&count_mutex, me);
        u64 v = counter;
        for (volatile int d = 0; d < 20; d++) {}
        counter = v + 1;
        mutex_unlock(&count_mutex, me);
    }
    if (wait_one(c1, FOREVER) || wait_one(c2, FOREVER) || wait_one(c3, FOREVER))
        fail("waiting for the counting threads failed");
    line("counter after 4 threads x 2000 locked increments: ", counter);
    hex("mutex word afterwards: ", count_mutex);
    close_handle(c1);
    close_handle(c2);
    close_handle(c3);

    // A producer and a consumer through condition variables.
    t = start(0, consumer, 0, 1);
    for (u64 i = 1; i <= ITEMS; i++) {
        mutex_lock(&queue_mutex, me);
        while (slot_full) cond_wait(&not_full, &queue_mutex, me, FOREVER);
        slot = i;
        slot_full = 1;
        signal_key(&not_empty, 1);
        mutex_unlock(&queue_mutex, me);
    }
    wait_one(t, FOREVER);
    line("items consumed: ", consumed);
    line("sum of the items: ", consumed_sum);
    close_handle(t);

    // A condition variable wait that times out gives the mutex back first.
    static volatile u32 m, cv;
    mutex_lock(&m, me);
    hex("wait 1 ms on a condition variable: ", wait_key(&m, &cv, me, 1000000));
    yes_no("the mutex was released: ", m == 0);
    hex("with timeout 0: ", (mutex_lock(&m, me), wait_key(&m, &cv, me, 0)));
    yes_no("released again: ", m == 0);

    // Address arbitration.
    hex("wait while 0 < 0: ", wait_address(&flag, 0, 0, FOREVER));
    hex("wait while 0 < 1, no time: ", wait_address(&flag, 0, 1, 0));
    hex("unknown arbitration type: ", wait_address(&flag, 3, 0, 0));
    t = start(0, address_waiter, 0, 1);
    while (!address_ready) sleep_ns(0);
    sleep_ns(1000000);
    yes_no("the address waiter is still waiting: ", address_rc == 0xffff);
    hex("signal and increment if 5: ", signal_address(&flag, 1, 5, -1));
    hex("signal and increment if 0: ", signal_address(&flag, 1, 0, -1));
    line("the word now: ", (u64)flag);
    wait_one(t, FOREVER);
    hex("the address waiter's result: ", address_rc);
    close_handle(t);
}
