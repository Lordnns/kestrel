// A homebrew-format (NRO) test program: checks what the loader and the
// homebrew launch environment (Switchbrew, "Homebrew ABI") provide.
//
// crt0.S calls hb_relocate (hb.h) to apply the image's own relocations,
// then hb_main with the entry registers X0 (loader config list) and X1. The
// output is modelled in scripts/gen-expected.py; the arguments come from
// guest/src/nro/hbtest.args, one per line.
#include "hb.h"

// Pointers that need relocating: strings and functions (volatile, so the
// compiler reads them from the tables instead of folding them).
static u64 add(u64 a, u64 b) { return a + b; }
static u64 mul(u64 a, u64 b) { return a * b; }
static const char *const volatile names[] = {"alpha", "beta", "gamma"};
static u64 (*const volatile ops[])(u64, u64) = {add, mul};

static u64 counter = 1234; // .data
static u64 zeros[512];     // .bss

static void put_line(const char *label, u64 v) {
    puts_(label);
    put_u64(v);
    puts_("\n");
}

// Split the argument string the way homebrew does: words separated by
// spaces, double quotes grouping (and allowing an empty word).
static void print_args(const char *s) {
    char word[256];
    u64 argc = 0;
    for (;;) {
        while (*s == ' ') s++;
        if (!*s) break;
        u64 n = 0;
        if (*s == '"') {
            for (s++; *s && *s != '"'; s++)
                if (n < sizeof word - 1) word[n++] = *s;
            if (*s == '"') s++;
        } else {
            for (; *s && *s != ' '; s++)
                if (n < sizeof word - 1) word[n++] = *s;
        }
        word[n] = 0;
        puts_("argv[");
        put_u64(argc++);
        puts_("] = [");
        puts_(word);
        puts_("]\n");
    }
    put_line("argc = ", argc);
}

void hb_main(const config_entry *config, u64 x1) {
    puts_("homebrew test program\n");
    if (x1 != ~(u64)0) fail("X1 is not all ones");
    const char *args = 0;
    puts_("config keys:");
    for (const config_entry *e = config;; e++) {
        puts_(" ");
        put_u64(e->key);
        if (e->key == 5) args = (const char *)e->value[1];
        if (e->key == 7) {
            puts_("(applet type ");
            put_u64(e->value[0]);
            puts_(")");
        }
        if (e->key == 0) break;
    }
    puts_("\n");
    if (!args) fail("no Argv entry");
    print_args(args);

    u64 sum = 0;
    for (u64 i = 0; i < sizeof zeros / sizeof zeros[0]; i++) sum |= zeros[i];
    put_line("bss nonzero words: ", sum);
    zeros[511] = 7;
    put_line("bss after write: ", zeros[511]);
    put_line("data counter: ", counter);
    counter++;
    put_line("data counter after increment: ", counter);
    for (int i = 0; i < 3; i++) {
        puts_("relocated string: ");
        puts_(names[i]);
        puts_("\n");
    }
    put_line("relocated call add(3, 4) = ", ops[0](3, 4));
    put_line("relocated call mul(3, 4) = ", ops[1](3, 4));
}
