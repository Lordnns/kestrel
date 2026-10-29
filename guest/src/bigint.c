// 64- and 128-bit integer work: __int128 multiply chains (mul/umulh/madd),
// modular arithmetic near 2^64 with overflow handling, deterministic
// Miller-Rabin, and a SWAR popcount.
#include "rt.h"

typedef unsigned __int128 u128;

static u64 addmod(u64 a, u64 b, u64 m) { return a >= m - b ? a - (m - b) : a + b; }

static u64 mulmod(u64 a, u64 b, u64 m) {
    u64 r = 0;
    a %= m;
    while (b) {
        if (b & 1) r = addmod(r, a, m);
        a = addmod(a, a, m);
        b >>= 1;
    }
    return r;
}

static u64 powmod(u64 b, u64 e, u64 m) {
    u64 r = 1 % m;
    b %= m;
    while (e) {
        if (e & 1) r = mulmod(r, b, m);
        b = mulmod(b, b, m);
        e >>= 1;
    }
    return r;
}

static int is_prime(u64 n) {
    static const u64 bases[] = {2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37};
    if (n < 2) return 0;
    for (int i = 0; i < 12; i++) {
        if (n == bases[i]) return 1;
        if (n % bases[i] == 0) return 0;
    }
    u64 d = n - 1;
    int s = 0;
    while (!(d & 1)) {
        d >>= 1;
        s++;
    }
    for (int i = 0; i < 12; i++) {
        u64 x = powmod(bases[i], d, n);
        if (x == 1 || x == n - 1) continue;
        int composite = 1;
        for (int r = 1; r < s; r++) {
            x = mulmod(x, x, n);
            if (x == n - 1) {
                composite = 0;
                break;
            }
        }
        if (composite) return 0;
    }
    return 1;
}

static u64 popcount(u64 x) {
    x = x - ((x >> 1) & 0x5555555555555555ull);
    x = (x & 0x3333333333333333ull) + ((x >> 2) & 0x3333333333333333ull);
    x = (x + (x >> 4)) & 0x0f0f0f0f0f0f0f0full;
    return (x * 0x0101010101010101ull) >> 56;
}

static void check_prime(const char *label, u64 n) {
    puts_(label);
    puts_(is_prime(n) ? "prime\n" : "composite\n");
}

int main(void) {
    u128 f = 1;
    for (u64 i = 2; i <= 30; i++) f *= i;
    puts_("30! = ");
    put_hex((u64)(f >> 64));
    puts_(":");
    put_hex((u64)f);
    puts_("\n");

    u128 p = (u128)0xfedcba9876543210ull * 0x0123456789abcdefull;
    puts_("128-bit product = ");
    put_hex((u64)(p >> 64));
    puts_(":");
    put_hex((u64)p);
    puts_("\n");

    check_prime("2^61-1       : ", (1ull << 61) - 1);
    check_prime("2^64-59      : ", 18446744073709551557ull);
    check_prime("2^64-1       : ", 18446744073709551615ull);
    check_prime("561          : ", 561);
    check_prime("1e18+9       : ", 1000000000000000009ull);

    puts_("powmod(3, 2^63, 2^64-59) = ");
    put_u64(powmod(3, 1ull << 63, 18446744073709551557ull));
    puts_("\n");

    puts_("popcount(0xdeadbeefcafebabe) = ");
    put_u64(popcount(0xdeadbeefcafebabeull));
    puts_("\n");
    return 0;
}
