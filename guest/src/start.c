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

#include "libc.h"
