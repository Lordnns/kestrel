// A homebrew-format (NRO) test program for the process layout that
// svcGetInfo reports (Switchbrew, "SVC"). It prints properties the ABI
// promises rather than kestrel's addresses: the regions fit the 39-bit
// address space without overlapping, the heap and alias regions have the
// 39-bit default sizes (Switchbrew, "16.0.0"), the stack, the image and the
// heap sit in their regions, and bad requests give the documented results.
// The output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001ull

enum {
    ALIAS_ADDR = 2,
    ALIAS_SIZE = 3,
    HEAP_ADDR = 4,
    HEAP_SIZE = 5,
    TOTAL_MEMORY = 6,
    USED_MEMORY = 7,
    DEBUGGER = 8,
    ENTROPY = 11,
    ASLR_ADDR = 12,
    ASLR_SIZE = 13,
    STACK_ADDR = 14,
    STACK_SIZE = 15,
    PROGRAM_ID = 18,
    IS_APPLICATION = 23,
};

extern char nro_start[] __attribute__((visibility("hidden")));

typedef struct {
    u64 start, end;
} region;

static u64 info(u32 kind) {
    u64 v;
    if (svc_get_info(&v, kind, CUR_PROCESS, 0) != 0) fail("svcGetInfo failed");
    return v;
}

static region get_region(u32 addr, u32 size) {
    region r = {info(addr), 0};
    r.end = r.start + info(size);
    return r;
}

static int inside(region r, u64 lo, u64 hi) { return r.start <= lo && hi <= r.end; }
static int overlap(region a, region b) { return a.start < b.end && b.start < a.end; }

static void yes_no(const char *label, int v) {
    puts_(label);
    puts_(v ? "yes\n" : "no\n");
}

static void code(const char *label, u32 kind, u64 handle, u64 sub) {
    u64 v;
    puts_(label);
    put_hex(svc_get_info(&v, kind, handle, sub));
    puts_("\n");
}

void hb_main(void) {
    puts_("homebrew layout test\n");
    region aslr = get_region(ASLR_ADDR, ASLR_SIZE);
    region alias = get_region(ALIAS_ADDR, ALIAS_SIZE);
    region heap = get_region(HEAP_ADDR, HEAP_SIZE);
    region stack = get_region(STACK_ADDR, STACK_SIZE);

    yes_no("aslr region inside the 39-bit space: ",
           aslr.start > 0 && aslr.start < aslr.end && aslr.end <= 1ull << 39);
    yes_no("alias, heap and stack regions inside the aslr region: ",
           inside(aslr, alias.start, alias.end) && inside(aslr, heap.start, heap.end) &&
               inside(aslr, stack.start, stack.end));
    yes_no("any two regions overlap: ",
           overlap(alias, heap) || overlap(alias, stack) || overlap(heap, stack));
    puts_("alias region GiB: ");
    put_u64((alias.end - alias.start) >> 30);
    puts_("\nheap region GiB: ");
    put_u64((heap.end - heap.start) >> 30);
    puts_("\n");

    u64 image = (u64)nro_start;
    yes_no("image inside the aslr region, outside the others: ",
           inside(aslr, image, image + 1) && !inside(alias, image, image + 1) &&
               !inside(heap, image, image + 1) && !inside(stack, image, image + 1));
    u64 sp;
    __asm__ volatile("mov %0, sp" : "=r"(sp));
    yes_no("stack pointer inside the stack region: ", inside(stack, sp - 1, sp));

    u64 used = info(USED_MEMORY);
    u8 *h = svc_set_heap_size(0x200000);
    yes_no("heap at the start of the heap region: ", (u64)h == heap.start);
    h[0] = 0x5a;
    h[0x1fffff] = 0xa5;
    yes_no("heap readable and writable: ", h[0] == 0x5a && h[0x1fffff] == 0xa5);
    yes_no("used memory grew by the heap size: ", info(USED_MEMORY) == used + 0x200000);
    yes_no("used memory within total: ", info(USED_MEMORY) <= info(TOTAL_MEMORY));

    for (u64 sub = 0; sub < 4; sub++) {
        u64 v;
        puts_("entropy subtype ");
        put_u64(sub);
        puts_(": ");
        put_hex(svc_get_info(&v, ENTROPY, 0, sub));
        puts_("\n");
    }
    code("entropy subtype 4: ", ENTROPY, 0, 4);
    code("heap region with handle 0: ", HEAP_ADDR, 0, 0);
    code("heap region with subtype 1: ", HEAP_ADDR, CUR_PROCESS, 1);
    code("debugger attached with the process handle: ", DEBUGGER, CUR_PROCESS, 0);
    u64 dbg;
    svc_get_info(&dbg, DEBUGGER, 0, 0);
    puts_("debugger attached: ");
    put_u64(dbg);
    puts_("\nis application: ");
    put_u64(info(IS_APPLICATION));
    puts_("\n");
    yes_no("program id nonzero: ", info(PROGRAM_ID) != 0);
}
