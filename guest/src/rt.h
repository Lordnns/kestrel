// Tiny freestanding runtime for kestrel test programs.
// Talks to the emulator only through documented supervisor calls.
#pragma once

typedef unsigned long u64;
typedef unsigned int u32;
typedef unsigned char u8;
typedef long i64;
typedef u64 size_t;

static inline u64 svc_output_debug_string(const char *s, u64 len) {
    register u64 x0 __asm__("x0") = (u64)s;
    register u64 x1 __asm__("x1") = len;
    __asm__ volatile("svc #0x27" : "+r"(x0) : "r"(x1) : "memory");
    return x0;
}

__attribute__((noreturn)) static inline void svc_exit_process(void) {
    __asm__ volatile("svc #0x07" ::: "memory");
    __builtin_unreachable();
}

static inline void *svc_set_heap_size(u64 size) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = size;
    __asm__ volatile("svc #0x01" : "=r"(x0), "+r"(x1) : : "memory");
    return x0 == 0 ? (void *)x1 : (void *)0;
}

static inline u64 strlen_(const char *s) {
    u64 n = 0;
    while (s[n]) n++;
    return n;
}

static inline void puts_(const char *s) { svc_output_debug_string(s, strlen_(s)); }

static inline void put_u64(u64 v) {
    char buf[24];
    int i = 23;
    buf[i] = 0;
    do {
        buf[--i] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    puts_(&buf[i]);
}

static inline void put_hex(u64 v) {
    char buf[19];
    buf[0] = '0';
    buf[1] = 'x';
    for (int i = 0; i < 16; i++) buf[2 + i] = "0123456789abcdef"[(v >> (60 - 4 * i)) & 0xf];
    buf[18] = 0;
    puts_(buf);
}
