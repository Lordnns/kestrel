// SHA-256 (FIPS 180-4), written from the published specification.
// Lots of 32-bit rotates, adds and big-endian byte handling.
#include "rt.h"

static const u32 K[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
};

typedef struct {
    u32 h[8];
    u8 block[64];
    u64 len;
    u32 fill;
} sha256_ctx;

static inline u32 rotr(u32 x, int n) { return (x >> n) | (x << (32 - n)); }

static void compress(u32 h[8], const u8 *p) {
    u32 w[64];
    for (int i = 0; i < 16; i++)
        w[i] = (u32)p[4 * i] << 24 | (u32)p[4 * i + 1] << 16 | (u32)p[4 * i + 2] << 8 | p[4 * i + 3];
    for (int i = 16; i < 64; i++) {
        u32 s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
        u32 s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    u32 a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int i = 0; i < 64; i++) {
        u32 t1 = hh + (rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)) + ((e & f) ^ (~e & g)) + K[i] + w[i];
        u32 t2 = (rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)) + ((a & b) ^ (a & c) ^ (b & c));
        hh = g;
        g = f;
        f = e;
        e = d + t1;
        d = c;
        c = b;
        b = a;
        a = t1 + t2;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d;
    h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}

static void sha_init(sha256_ctx *c) {
    static const u32 iv[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                              0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    for (int i = 0; i < 8; i++) c->h[i] = iv[i];
    c->len = 0;
    c->fill = 0;
}

static void sha_update(sha256_ctx *c, const u8 *p, u64 n) {
    c->len += n;
    while (n--) {
        c->block[c->fill++] = *p++;
        if (c->fill == 64) {
            compress(c->h, c->block);
            c->fill = 0;
        }
    }
}

static void sha_final(sha256_ctx *c, u8 out[32]) {
    u64 bits = c->len * 8;
    u8 pad = 0x80;
    sha_update(c, &pad, 1);
    u8 zero = 0;
    while (c->fill != 56) sha_update(c, &zero, 1);
    u8 lenbuf[8];
    for (int i = 0; i < 8; i++) lenbuf[i] = (u8)(bits >> (56 - 8 * i));
    sha_update(c, lenbuf, 8);
    for (int i = 0; i < 8; i++) {
        out[4 * i] = (u8)(c->h[i] >> 24);
        out[4 * i + 1] = (u8)(c->h[i] >> 16);
        out[4 * i + 2] = (u8)(c->h[i] >> 8);
        out[4 * i + 3] = (u8)c->h[i];
    }
}

static void print_digest(const char *label, const u8 *data, u64 n) {
    sha256_ctx c;
    u8 d[32];
    sha_init(&c);
    sha_update(&c, data, n);
    sha_final(&c, d);
    char hex[65];
    for (int i = 0; i < 32; i++) {
        hex[2 * i] = "0123456789abcdef"[d[i] >> 4];
        hex[2 * i + 1] = "0123456789abcdef"[d[i] & 15];
    }
    hex[64] = 0;
    puts_(label);
    puts_(hex);
    puts_("\n");
}

int main(void) {
    print_digest("sha256('')    = ", (const u8 *)"", 0);
    print_digest("sha256('abc') = ", (const u8 *)"abc", 3);

    u8 *heap = svc_set_heap_size(0x200000);
    if (!heap) {
        puts_("heap allocation failed\n");
        return 1;
    }
    for (u64 i = 0; i < 100000; i++) heap[i] = 'a';
    print_digest("sha256(a*1e5) = ", heap, 100000);
    return 0;
}
