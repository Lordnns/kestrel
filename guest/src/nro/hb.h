// Shared by the homebrew-format test programs: the runtime, the loader
// config entry layout (Switchbrew, "Homebrew ABI") and the self-relocation
// crt0.S calls before hb_main. Include in exactly one file per program.
#pragma once
#include "../libc.h"
#include "../rt.h"

typedef struct {
    u32 key, flags;
    u64 value[2];
} config_entry;

// Elf64_Dyn and Elf64_Rela.
typedef struct {
    i64 tag;
    u64 val;
} dyn;
typedef struct {
    u64 offset, info;
    i64 addend;
} rela;

#define DT_NULL 0
#define DT_RELA 7
#define DT_RELASZ 8
#define DT_RELAENT 9
#define R_AARCH64_RELATIVE 1027

static void fail(const char *why) {
    puts_("FAIL: ");
    puts_(why);
    puts_("\n");
    svc_exit_process();
}

// Runs before any relocation is applied, so it may not use absolute
// pointers; string literals are reached PC-relative.
void hb_relocate(u8 *base, const dyn *d) {
    u64 at = 0, size = 0, ent = sizeof(rela);
    for (; d->tag != DT_NULL; d++) {
        if (d->tag == DT_RELA) at = d->val;
        if (d->tag == DT_RELASZ) size = d->val;
        if (d->tag == DT_RELAENT) ent = d->val;
    }
    for (u64 i = 0; i < size; i += ent) {
        const rela *r = (const rela *)(base + at + i);
        if ((u32)r->info != R_AARCH64_RELATIVE) fail("unexpected relocation type");
        *(u64 *)(base + r->offset) = (u64)(base + r->addend);
    }
}
