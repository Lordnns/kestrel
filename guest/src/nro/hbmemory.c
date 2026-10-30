// A homebrew-format (NRO) test program for the memory system calls
// (Switchbrew, "SVC"): svcQueryMemory, svcSetHeapSize, svcSetMemoryPermission,
// svcSetMemoryAttribute, svcMapMemory/svcUnmapMemory and
// svcMapPhysicalMemory/svcUnmapPhysicalMemory. It prints memory types and
// result codes where Switchbrew documents them, and yes/no where it only
// describes the behaviour. The output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define CUR_PROCESS 0xffff8001ull

typedef struct {
    u64 base, size;
    u32 type, attr, perm, ipc_refs, device_refs, padding;
} memory_info;

// Each wrapper returns W0, the result code.
static u64 svc2(u64 n, u64 a, u64 b) {
    register u64 x0 __asm__("x0") = a;
    register u64 x1 __asm__("x1") = b;
    switch (n) {
    case 0x2c: __asm__ volatile("svc #0x2c" : "+r"(x0), "+r"(x1) : : "memory"); break;
    case 0x2d: __asm__ volatile("svc #0x2d" : "+r"(x0), "+r"(x1) : : "memory"); break;
    }
    return x0;
}

static u64 svc3(u64 n, u64 a, u64 b, u64 c) {
    register u64 x0 __asm__("x0") = a;
    register u64 x1 __asm__("x1") = b;
    register u64 x2 __asm__("x2") = c;
    switch (n) {
    case 0x02: __asm__ volatile("svc #0x02" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory"); break;
    case 0x04: __asm__ volatile("svc #0x04" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory"); break;
    case 0x05: __asm__ volatile("svc #0x05" : "+r"(x0), "+r"(x1), "+r"(x2) : : "memory"); break;
    }
    return x0;
}

static u64 set_attribute(u64 addr, u64 size, u64 mask, u64 value) {
    register u64 x0 __asm__("x0") = addr;
    register u64 x1 __asm__("x1") = size;
    register u64 x2 __asm__("x2") = mask;
    register u64 x3 __asm__("x3") = value;
    __asm__ volatile("svc #0x03" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3) : : "memory");
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
    if (!(m.base <= addr && addr - m.base < m.size)) fail("block does not hold the address");
    return m;
}

static u64 info(u32 kind) {
    u64 v;
    if (svc_get_info(&v, kind, CUR_PROCESS, 0) != 0) fail("svcGetInfo failed");
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

extern char nro_start[] __attribute__((visibility("hidden")));
static volatile u64 data_word = 1; // written below, so it stays in .data

void hb_main(void) {
    puts_("homebrew memory test\n");
    data_word++;
    block("image text:", query((u64)nro_start));
    block("image data:", query((u64)&data_word));
    memory_info none = query(info(2) + (1ull << 30)); // inside the alias region
    block("unmapped address:", none);

    // Heap.
    u64 heap;
    hex("heap 2 MiB: ", heap_size(0x200000, &heap));
    block("heap:", query(heap));
    u64 unused; // X1 is not an address when the call fails
    hex("heap unaligned size: ", heap_size(0x1000, &unused));
    hex("heap over 4 GiB: ", heap_size(0x100200000ull, &unused));
    u8 *h = (u8 *)heap;
    for (int i = 0; i < 64; i++) h[i] = (u8)i;
    hex("heap 4 MiB: ", heap_size(0x400000, &heap));
    yes_no("growing kept the contents: ", h[0] == 0 && h[63] == 63);

    // Permission and attribute.
    hex("set r--: ", svc3(0x02, heap, 0x1000, 1));
    memory_info ro = query(heap);
    block("heap page:", ro);
    yes_no("only that page changed: ", ro.size == 0x1000 && query(heap + 0x1000).perm == 3);
    yes_no("still readable: ", h[5] == 5);
    hex("set ---: ", svc3(0x02, heap, 0x1000, 0));
    hex("set rw-: ", svc3(0x02, heap, 0x1000, 3));
    hex("set -w-: ", svc3(0x02, heap, 0x1000, 2));
    hex("set r-x: ", svc3(0x02, heap, 0x1000, 5));
    hex("set uncached: ", set_attribute(heap, 0x1000, 8, 8));
    block("heap page:", query(heap));
    hex("clear uncached: ", set_attribute(heap, 0x1000, 8, 0));
    yes_no("other attribute rejected: ", set_attribute(heap, 0x1000, 1, 1) != 0);
    memory_info whole = query(heap);
    yes_no("heap is one block again: ", whole.base == heap && whole.size == 0x400000);

    // svcMapMemory into the stack region.
    u64 stack_region = info(14);
    u64 dst = stack_region;
    hex("map memory: ", svc3(0x04, dst, heap, 0x2000));
    block("destination:", query(dst));
    block("source:", query(heap));
    u8 *d = (u8 *)dst;
    yes_no("destination holds the source bytes: ", d[7] == 7 && d[63] == 63);
    d[8] = 0xee;
    yes_no("map into the alias region rejected: ", svc3(0x04, info(2), heap + 0x2000, 0x1000) != 0);
    hex("unmap with the wrong source: ", svc3(0x05, dst, heap + 0x1000, 0x1000));
    hex("unmap memory: ", svc3(0x05, dst, heap, 0x2000));
    block("source after unmap:", query(heap));
    yes_no("write through the destination kept: ", h[8] == 0xee);

    // svcMapPhysicalMemory in the alias region.
    u64 alias = info(2);
    hex("map physical: ", svc2(0x2c, alias, 0x10000));
    memory_info phys = query(alias);
    puts_("physical block: ");
    put_hex(phys.size);
    puts_(" bytes, ");
    puts_(phys.perm == 3 ? "rw-\n" : "not rw-\n");
    u64 used = info(7);
    hex("map a larger range over it: ", svc2(0x2c, alias, 0x20000));
    yes_no("only the new part counted: ", info(7) == used + 0x10000);
    ((u64 *)alias)[1] = 42;
    yes_no("physical memory writable: ", ((u64 *)alias)[1] == 42);
    hex("map physical unaligned size: ", svc2(0x2c, alias, 0x1800));
    hex("map physical outside alias: ", svc2(0x2c, heap, 0x1000));
    hex("unmap physical: ", svc2(0x2d, alias, 0x20000));
    block("after unmap:", query(alias));

    hex("heap shrink to 2 MiB: ", heap_size(0x200000, &heap));
    yes_no("shrinking kept the rest: ", h[8] == 0xee && h[63] == 63);
}
