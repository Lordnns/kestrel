// Exercises compiler-generated integer code: recursion, loops, division,
// 32/64-bit mixing, function pointers, heap allocation and a sort.
#include "rt.h"

static u64 fib(u64 n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

static u32 crc32(const u8 *p, u64 n) {
    u32 c = 0xffffffffu;
    for (u64 i = 0; i < n; i++) {
        c ^= p[i];
        for (int k = 0; k < 8; k++) c = (c >> 1) ^ (0xedb88320u & -(c & 1));
    }
    return ~c;
}

static u64 sieve(u8 *mark, u64 limit) {
    u64 count = 0;
    for (u64 i = 2; i <= limit; i++) {
        if (mark[i]) continue;
        count++;
        for (u64 j = i * i; j <= limit; j += i) mark[j] = 1;
    }
    return count;
}

static u64 rng_state = 0x9e3779b97f4a7c15ull;
static u64 rng(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return rng_state;
}

static int cmp_i64(const void *a, const void *b) {
    i64 x = *(const i64 *)a, y = *(const i64 *)b;
    return (x > y) - (x < y);
}

static void sort(i64 *v, long n, int (*cmp)(const void *, const void *)) {
    // Shell sort: simple, iterative, exercises signed compares and indexing.
    for (long gap = n / 2; gap > 0; gap /= 2)
        for (long i = gap; i < n; i++) {
            i64 t = v[i];
            long j = i;
            for (; j >= gap && cmp(&v[j - gap], &t) > 0; j -= gap) v[j] = v[j - gap];
            v[j] = t;
        }
}

int main(void) {
    puts_("fib(25) = ");
    put_u64(fib(25));
    puts_("\n");

    const char *msg = "The quick brown fox jumps over the lazy dog";
    puts_("crc32 = ");
    put_hex(crc32((const u8 *)msg, strlen_(msg)));
    puts_("\n");

    u8 *heap = svc_set_heap_size(0x200000);
    if (!heap) {
        puts_("heap allocation failed\n");
        return 1;
    }
    puts_("primes <= 100000 = ");
    put_u64(sieve(heap, 100000));
    puts_("\n");

    long n = 2000;
    i64 *v = (i64 *)(heap + 0x100000);
    for (long i = 0; i < n; i++) v[i] = (i64)(rng() % 100000) - 50000;
    sort(v, n, cmp_i64);
    int ok = 1;
    for (long i = 1; i < n; i++)
        if (v[i - 1] > v[i]) ok = 0;
    puts_(ok ? "sort ok\n" : "sort FAILED\n");

    i64 a = -7, b = 2;
    puts_("signed div/mod = ");
    put_u64((u64)(a / b + 100));
    puts_(" ");
    put_u64((u64)(a % b + 100));
    puts_("\n");
    return 0;
}
