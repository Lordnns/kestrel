// Entry point and the libc functions compilers may emit calls to.
#include "rt.h"

int main(void);

__attribute__((noreturn, used)) void _start_c(void) {
    main();
    svc_exit_process();
}

// The loader sets SP; just align it and jump into C.
__asm__(".globl _start\n"
        "_start:\n"
        "  mov x29, #0\n"
        "  mov x30, #0\n"
        "  bl _start_c\n");

void *memset(void *d, int c, size_t n) {
    u8 *p = d;
    while (n--) *p++ = (u8)c;
    return d;
}

void *memcpy(void *d, const void *s, size_t n) {
    u8 *a = d;
    const u8 *b = s;
    while (n--) *a++ = *b++;
    return d;
}

void *memmove(void *d, const void *s, size_t n) {
    u8 *a = d;
    const u8 *b = s;
    if (a < b) {
        while (n--) *a++ = *b++;
    } else {
        while (n--) a[n] = b[n];
    }
    return d;
}

int memcmp(const void *x, const void *y, size_t n) {
    const u8 *a = x, *b = y;
    for (size_t i = 0; i < n; i++)
        if (a[i] != b[i]) return a[i] - b[i];
    return 0;
}
