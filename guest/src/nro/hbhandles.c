// A homebrew-format (NRO) test program for handles and the first kernel
// objects (Switchbrew, "SVC"): the MainThreadHandle loader config entry,
// svcGetThreadId, svcGetProcessId, svcCloseHandle, the event calls and
// svcCreateTransferMemory. It prints result codes and memory types, and
// yes/no for behaviour. Thread and process IDs, handle values and the size
// of the handle table are kestrel's own choices, so only their properties
// are printed. The output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_THREAD 0xffff8000ull
#define CUR_PROCESS 0xffff8001ull

typedef struct {
    u64 base, size;
    u32 type, attr, perm, ipc_refs, device_refs, padding;
} memory_info;

// Calls taking one handle in W0 and returning only the result in W0.
#define SVC_HANDLE(name, n)                                                  \
    static u64 name(u64 h) {                                                 \
        register u64 x0 __asm__("x0") = h;                                   \
        __asm__ volatile("svc #" #n : "+r"(x0) : : "memory");                \
        return x0;                                                           \
    }
SVC_HANDLE(signal_event, 0x11)
SVC_HANDLE(clear_event, 0x12)
SVC_HANDLE(close_handle, 0x16)
SVC_HANDLE(reset_signal, 0x17)

// svcGetProcessId and svcGetThreadId: handle in W1, ID out in X1.
#define SVC_ID(name, n)                                                      \
    static u64 name(u64 *id, u64 h) {                                        \
        register u64 x0 __asm__("x0");                                       \
        register u64 x1 __asm__("x1") = h;                                   \
        __asm__ volatile("svc #" #n : "=r"(x0), "+r"(x1) : : "memory");      \
        *id = x1;                                                            \
        return x0;                                                           \
    }
SVC_ID(get_process_id, 0x24)
SVC_ID(get_thread_id, 0x25)

static u64 create_event(u64 *writable, u64 *readable) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1");
    register u64 x2 __asm__("x2");
    __asm__ volatile("svc #0x45" : "=r"(x0), "=r"(x1), "=r"(x2) : : "memory");
    *writable = x1;
    *readable = x2;
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

static u64 set_permission(u64 addr, u64 size, u64 perm) {
    register u64 x0 __asm__("x0") = addr;
    register u64 x1 __asm__("x1") = size;
    register u64 x2 __asm__("x2") = perm;
    __asm__ volatile("svc #0x02" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory");
    return x0;
}

static u64 heap_size(u64 size, u64 *addr) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = size;
    __asm__ volatile("svc #0x01" : "=r"(x0), "+r"(x1) : : "memory");
    *addr = x1;
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

static void hex(const char *label, u64 v) {
    puts_(label);
    put_hex(v);
    puts_("\n");
}

static void yes_no(const char *label, int v) {
    puts_(label);
    puts_(v ? "yes\n" : "no\n");
}

static void block(const char *label, memory_info m) {
    static const char *const perms[] = {"---", "r--", "-w-", "rw-", "--x", "r-x", "-wx", "rwx"};
    puts_(label);
    puts_(" type ");
    put_hex(m.type);
    puts_(" attr ");
    put_u64(m.attr);
    puts_(" ");
    puts_(perms[m.perm & 7]);
    puts_("\n");
}

static u64 handles[2048]; // enough to fill kestrel's table

void hb_main(const config_entry *config, u64 x1) {
    (void)x1;
    puts_("homebrew handle test\n");

    // Threads and the process.
    u64 main_thread = 0;
    for (const config_entry *e = config; e->key != 0; e++)
        if (e->key == 1) main_thread = e->value[0];
    yes_no("main thread handle is a real handle: ",
           main_thread != 0 && main_thread != CUR_THREAD && main_thread != CUR_PROCESS);
    u64 tid, current_tid, pid, unused;
    hex("thread id of the main thread handle: ", get_thread_id(&tid, main_thread));
    hex("thread id of the current thread: ", get_thread_id(&current_tid, CUR_THREAD));
    yes_no("both name the same thread: ", tid == current_tid);
    hex("process id of the current process: ", get_process_id(&pid, CUR_PROCESS));
    hex("thread id of handle 0: ", get_thread_id(&unused, 0));
    hex("close handle 0: ", close_handle(0));

    // Events.
    u64 w, r;
    hex("create event: ", create_event(&w, &r));
    yes_no("two different handles: ", w != 0 && r != 0 && w != r);
    hex("reset an unsignaled event: ", reset_signal(r));
    hex("signal through the readable side: ", signal_event(r));
    hex("signal: ", signal_event(w));
    hex("reset: ", reset_signal(r));
    hex("reset again: ", reset_signal(r));
    hex("signal: ", signal_event(w));
    hex("clear through the readable side: ", clear_event(r));
    hex("reset after clearing: ", reset_signal(r));
    hex("clear a thread handle: ", clear_event(main_thread));
    hex("close the writable side: ", close_handle(w));
    hex("signal through the closed handle: ", signal_event(w));
    hex("clear through the readable side: ", clear_event(r));
    hex("close the readable side: ", close_handle(r));
    hex("close it again: ", close_handle(r));

    // Transfer memory.
    u64 heap, tm;
    hex("heap 2 MiB: ", heap_size(0x200000, &heap));
    volatile u8 *h = (volatile u8 *)heap;
    h[0x10] = 0x5a;
    hex("lend 8 KiB read-only: ", create_transfer_memory(&tm, heap, 0x2000, 1));
    memory_info lent = query(heap);
    block("lent:", lent);
    yes_no("lent block is 8 KiB: ", lent.base == heap && lent.size == 0x2000);
    yes_no("still readable: ", h[0x10] == 0x5a);
    hex("reprotect lent memory: ", set_permission(heap, 0x1000, 3));
    hex("lend it again: ", create_transfer_memory(&unused, heap, 0x1000, 0));
    hex("lend with execute: ", create_transfer_memory(&unused, heap + 0x2000, 0x1000, 5));
    hex("lend an unaligned size: ", create_transfer_memory(&unused, heap + 0x2000, 0x800, 0));
    u64 local = 0;
    u64 stack_page = (u64)&local & ~0xfffull;
    hex("lend the stack: ", create_transfer_memory(&unused, stack_page, 0x1000, 0));
    hex("close the transfer memory: ", close_handle(tm));
    block("returned:", query(heap));
    yes_no("heap is one block again: ", query(heap).size == 0x200000);
    h[0x10] = 1;
    yes_no("writable again: ", h[0x10] == 1);

    // Fill the handle table with events, then empty it.
    u64 n = 0, rc;
    while ((rc = create_event(&handles[n], &handles[n + 1])) == 0) {
        n += 2;
        if (n + 2 > sizeof handles / sizeof handles[0]) fail("the handle table never filled");
    }
    hex("create events until the table is full: ", rc);
    int closed = 1;
    for (u64 i = 0; i < n; i++) closed &= close_handle(handles[i]) == 0;
    yes_no("closed every event handle: ", closed);
    hex("create event after closing them: ", create_event(&w, &r));
    yes_no("old handles stay closed: ",
           signal_event(handles[0]) == 0xe401 && clear_event(handles[1]) == 0xe401);
    close_handle(w);
    close_handle(r);
    hex("close the main thread handle: ", close_handle(main_thread));
    hex("thread id of the current thread: ", get_thread_id(&current_tid, CUR_THREAD));
    yes_no("unchanged: ", current_tid == tid);
}
