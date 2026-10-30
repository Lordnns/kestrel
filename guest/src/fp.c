// build: simd-only
// Scalar floating point: every operation prints its result as an exact bit
// pattern. Built with -ffp-contract=off -fno-math-errno, and every operand is
// read from a volatile table, so the compiler emits one FP instruction per
// operation and folds nothing.
//
// Each OP(kind, fpcr, a, b, c) line runs `kind` on operands a, b, c (indices
// into DV, SV or IV) with FPCR set to `fpcr`. scripts/gen-expected.py reads
// the same tables and models every line from the Arm rules.
#include "rt.h"

#define RN 0u
#define RP (1u << 22)
#define RM (2u << 22)
#define RZ (3u << 22)
#define FZ (1u << 24)
#define DN (1u << 25)

static volatile const u64 DV[] = {
    0x3ff8000000000000, //  0: 1.5
    0x4002000000000000, //  1: 2.25
    0x3fb999999999999a, //  2: 0.1
    0xc008000000000000, //  3: -3.0
    0x0000000000000000, //  4: +0
    0x8000000000000000, //  5: -0
    0x7ff0000000000000, //  6: +inf
    0xfff0000000000000, //  7: -inf
    0x7ff8000000000123, //  8: quiet NaN with payload
    0x7ff0000000000456, //  9: signalling NaN
    0x0000000000000001, // 10: smallest subnormal
    0x0010000000000000, // 11: smallest normal
    0x7fefffffffffffff, // 12: largest finite
    0x4004000000000000, // 13: 2.5
    0xc004000000000000, // 14: -2.5
    0x7e37e43c8800759c, // 15: 1e300
    0x400c000000000000, // 16: 3.5
    0x43e0000000000000, // 17: 2^63
    0xc3e0000000000001, // 18: just below -2^63
    0x3fd5555555555555, // 19: 1/3
    0x000012688b70e62b, // 20: 1e-310 (subnormal)
    0x3fe0000000000000, // 21: 0.5
    0x3ff0000000000001, // 22: 1 + 2^-52
    0xbffc000000000000, // 23: -1.75
    0x41f0000000080000, // 24: 2^32 + 0.5
    0x0018000000000000, // 25: 1.5 * 2^-1022
};

static volatile const u32 SV[] = {
    0x3fc00000, //  0: 1.5
    0x40100000, //  1: 2.25
    0x3dcccccd, //  2: 0.1
    0xc0400000, //  3: -3.0
    0x00000000, //  4: +0
    0x80000000, //  5: -0
    0x7f800000, //  6: +inf
    0xff800000, //  7: -inf
    0x7fc00123, //  8: quiet NaN with payload
    0x7f800456, //  9: signalling NaN
    0x00000001, // 10: smallest subnormal
    0x00800000, // 11: smallest normal
    0x7f7fffff, // 12: largest finite
    0x40200000, // 13: 2.5
    0xc0200000, // 14: -2.5
    0x7149f2ca, // 15: 1e30
    0x40600000, // 16: 3.5
    0x4f000000, // 17: 2^31
    0x000116c2, // 18: 1e-40 (subnormal)
    0x00c00000, // 19: 1.5 * 2^-126
};

static volatile const u64 IV[] = {
    1,                  // 0
    0xffffffffffffffff, // 1: -1
    0x0020000000000001, // 2: 2^53 + 1
    0x8000000000000000, // 3: -2^63
    0x7fffffffffffffff, // 4: 2^63 - 1
    0x0000000001000001, // 5: 2^24 + 1
    0x00000000075bcd15, // 6: 123456789
    0xfffffffffeffffff, // 7: -(2^24 + 1)
};

enum {
    DADD, DSUB, DMUL, DDIV, DSQRT, DFMA, DNEG, DABS, DMIN, DMAX,
    DFLOOR, DCEIL, DTRUNC, DROUND, DRINT, DNEARBY,
    DTOI32, DTOU32, DTOI64, DTOU64, DLROUND, DTOS,
    DLT, DLE, DEQ, DUNORD,
    SADD, SSUB, SMUL, SDIV, SSQRT, SFMA, SMIN, SMAX, SRINT, STOI32, STOD,
    I64TOD, U64TOD, I32TOS, U64TOS,
};

static const char *const NAMES[] = {
    "DADD", "DSUB", "DMUL", "DDIV", "DSQRT", "DFMA", "DNEG", "DABS", "DMIN", "DMAX",
    "DFLOOR", "DCEIL", "DTRUNC", "DROUND", "DRINT", "DNEARBY",
    "DTOI32", "DTOU32", "DTOI64", "DTOU64", "DLROUND", "DTOS",
    "DLT", "DLE", "DEQ", "DUNORD",
    "SADD", "SSUB", "SMUL", "SDIV", "SSQRT", "SFMA", "SMIN", "SMAX", "SRINT", "STOI32", "STOD",
    "I64TOD", "U64TOD", "I32TOS", "U64TOS",
};

struct op {
    u8 kind, a, b, c;
    u32 fpcr;
};

#define OP(k, f, x, y, z) {k, x, y, z, f},
static const struct op OPS[] = {
    // Double precision arithmetic, round to nearest.
    OP(DADD, RN, 0, 1, 0)    // 1.5 + 2.25
    OP(DADD, RN, 2, 19, 0)   // 0.1 + 1/3, rounded
    OP(DADD, RN, 4, 5, 0)    // +0 + -0 = +0
    OP(DADD, RN, 5, 5, 0)    // -0 + -0 = -0
    OP(DADD, RN, 6, 7, 0)    // inf - inf: default NaN
    OP(DADD, RN, 12, 12, 0)  // overflow to inf
    OP(DADD, RN, 8, 9, 0)    // signalling NaN wins, quietened
    OP(DADD, RN, 8, 0, 0)    // quiet NaN propagates
    OP(DSUB, RN, 0, 0, 0)    // x - x = +0
    OP(DSUB, RN, 22, 0, 0)   // 1+2^-52 - 1.5
    OP(DMUL, RN, 2, 3, 0)    // 0.1 * -3
    OP(DMUL, RN, 4, 6, 0)    // 0 * inf: default NaN
    OP(DMUL, RN, 11, 21, 0)  // smallest normal / 2: subnormal
    OP(DMUL, RN, 15, 15, 0)  // overflow
    OP(DMUL, RN, 10, 21, 0)  // subnormal underflow to zero (tie to even)
    OP(DDIV, RN, 0, 3, 0)    // 1.5 / -3
    OP(DDIV, RN, 0, 4, 0)    // x / +0 = inf
    OP(DDIV, RN, 3, 5, 0)    // -3 / -0 = +inf
    OP(DDIV, RN, 4, 4, 0)    // 0 / 0: default NaN
    OP(DDIV, RN, 2, 19, 0)   // 0.1 / (1/3)
    OP(DSQRT, RN, 1, 0, 0)   // sqrt(2.25)
    OP(DSQRT, RN, 2, 0, 0)   // sqrt(0.1)
    OP(DSQRT, RN, 5, 0, 0)   // sqrt(-0) = -0
    OP(DSQRT, RN, 3, 0, 0)   // sqrt(-3): default NaN
    OP(DSQRT, RN, 20, 0, 0)  // sqrt of a subnormal
    OP(DFMA, RN, 2, 3, 22)   // 0.1 * -3 + (1+2^-52), one rounding
    OP(DFMA, RN, 22, 22, 23) // (1+2^-52)^2 - 1.75
    OP(DFMA, RN, 4, 6, 8)    // 0 * inf + qNaN: default NaN
    OP(DFMA, RN, 0, 1, 9)    // sNaN addend
    OP(DNEG, RN, 8, 0, 0)    // negation flips the sign of a NaN
    OP(DABS, RN, 23, 0, 0)
    OP(DMIN, RN, 4, 5, 0)    // min(+0, -0) = -0
    OP(DMAX, RN, 5, 4, 0)    // max(-0, +0) = +0
    OP(DMIN, RN, 8, 0, 0)    // minNum(qNaN, 1.5) = 1.5
    OP(DMAX, RN, 9, 0, 0)    // maxNum(sNaN, 1.5): quietened NaN
    OP(DMIN, RN, 7, 3, 0)
    // Rounding to integral.
    OP(DFLOOR, RN, 14, 0, 0)
    OP(DCEIL, RN, 14, 0, 0)
    OP(DTRUNC, RN, 23, 0, 0)
    OP(DROUND, RN, 13, 0, 0)  // ties away: 3
    OP(DROUND, RN, 14, 0, 0)  // ties away: -3
    OP(DRINT, RN, 13, 0, 0)   // ties to even: 2
    OP(DRINT, RN, 21, 0, 0)   // 0.5 -> +0
    OP(DCEIL, RN, 23, 0, 0)   // -1.75 -> -1
    OP(DTRUNC, RN, 21, 0, 0)  // 0.5 -> +0
    OP(DCEIL, RN, 21, 0, 0)   // 0.5 -> 1
    OP(DFLOOR, RN, 8, 0, 0)   // NaN stays NaN
    OP(DRINT, RP, 13, 0, 0)   // FPCR rounding: up
    OP(DNEARBY, RM, 13, 0, 0) // FPCR rounding: down
    OP(DNEARBY, RZ, 14, 0, 0) // FPCR rounding: toward zero
    OP(DRINT, RN, 15, 0, 0)   // already integral
    // Conversions to integers (round toward zero, saturating).
    OP(DTOI32, RN, 23, 0, 0)
    OP(DTOI32, RN, 15, 0, 0)  // saturates to INT32_MAX
    OP(DTOI32, RN, 7, 0, 0)   // -inf saturates to INT32_MIN
    OP(DTOI32, RN, 8, 0, 0)   // NaN converts to 0
    OP(DTOU32, RN, 3, 0, 0)   // negative saturates to 0
    OP(DTOU32, RN, 24, 0, 0)  // 2^32 + 0.5 saturates
    OP(DTOI64, RN, 17, 0, 0)  // 2^63 saturates
    OP(DTOI64, RN, 18, 0, 0)  // below -2^63 saturates
    OP(DTOU64, RN, 24, 0, 0)
    OP(DLROUND, RN, 13, 0, 0) // FCVTAS: ties away
    OP(DLROUND, RN, 14, 0, 0)
    // Precision conversions.
    OP(DTOS, RN, 2, 0, 0)     // 0.1 to single
    OP(DTOS, RZ, 2, 0, 0)     // ... rounded toward zero
    OP(DTOS, RP, 19, 0, 0)    // 1/3 rounded up
    OP(DTOS, RN, 15, 0, 0)    // 1e300 overflows to inf
    OP(DTOS, RZ, 15, 0, 0)    // ... or the largest single toward zero
    OP(DTOS, RN, 8, 0, 0)     // NaN payload moves down
    OP(DTOS, RN, 20, 0, 0)    // tiny value underflows to zero
    OP(STOD, RN, 2, 0, 0)
    OP(STOD, RN, 8, 0, 0)     // NaN payload moves up
    OP(STOD, RN, 18, 0, 0)    // subnormal single is a normal double
    // Compares.
    OP(DLT, RN, 3, 0, 0)
    OP(DLE, RN, 0, 0, 0)
    OP(DEQ, RN, 4, 5, 0)      // +0 == -0
    OP(DLT, RN, 8, 0, 0)      // NaN compares false
    OP(DUNORD, RN, 0, 8, 0)
    OP(DUNORD, RN, 0, 6, 0)
    // Single precision.
    OP(SADD, RN, 0, 1, 0)
    OP(SADD, RN, 2, 2, 0)
    OP(SADD, RN, 12, 12, 0)   // overflow
    OP(SSUB, RN, 3, 2, 0)
    OP(SMUL, RN, 2, 3, 0)
    OP(SMUL, RN, 11, 2, 0)    // underflow to a subnormal
    OP(SDIV, RN, 0, 3, 0)
    OP(SDIV, RN, 3, 4, 0)     // -3 / +0 = -inf
    OP(SSQRT, RN, 2, 0, 0)
    OP(SSQRT, RN, 18, 0, 0)   // sqrt of a subnormal
    OP(SFMA, RN, 2, 3, 0)     // 0.1 * -3 + 1.5
    OP(SFMA, RN, 2, 2, 2)
    OP(SMIN, RN, 9, 0, 0)     // minNum(sNaN, 1.5): quietened NaN
    OP(SMAX, RN, 0, 8, 0)     // maxNum(1.5, qNaN) = 1.5
    OP(SRINT, RN, 14, 0, 0)   // -2.5 -> -2
    OP(SRINT, RP, 2, 0, 0)    // 0.1 -> 1
    OP(STOI32, RN, 15, 0, 0)  // 1e30 saturates
    OP(STOI32, RN, 14, 0, 0)
    // Integers to floating point, rounded by FPCR.
    OP(I64TOD, RN, 2, 0, 0)   // 2^53 + 1: tie to even
    OP(I64TOD, RP, 2, 0, 0)
    OP(I64TOD, RN, 3, 0, 0)   // -2^63 is exact
    OP(I64TOD, RM, 4, 0, 0)
    OP(U64TOD, RN, 1, 0, 0)   // 2^64 - 1 rounds to 2^64
    OP(U64TOD, RZ, 1, 0, 0)
    OP(I32TOS, RN, 5, 0, 0)   // 2^24 + 1: tie to even
    OP(I32TOS, RP, 5, 0, 0)
    OP(I32TOS, RM, 7, 0, 0)   // -(2^24 + 1) toward -inf
    OP(I32TOS, RN, 6, 0, 0)
    OP(U64TOS, RN, 4, 0, 0)
    // Flush-to-zero: subnormal inputs and results become signed zero.
    OP(DADD, FZ, 10, 4, 0)    // subnormal + 0 = +0
    OP(DMUL, FZ, 11, 21, 0)   // result below the smallest normal
    OP(DMUL, FZ, 25, 23, 0)   // a normal result is kept
    OP(DMAX, FZ, 10, 5, 0)    // maxNum(flushed +0, -0) = +0
    OP(DSQRT, FZ, 20, 0, 0)
    OP(DTOS, FZ, 11, 0, 0)    // underflows, flushed
    OP(STOD, FZ, 18, 0, 0)    // input flushed
    OP(DLT, FZ, 5, 10, 0)     // -0 < flushed subnormal: false
    OP(SMUL, FZ, 11, 2, 0)    // single result below the smallest normal
    OP(SADD, FZ, 18, 19, 0)   // subnormal input flushed
    OP(SFMA, FZ, 11, 2, 19)   // fused: exact sum is normal
    OP(DTOI32, FZ, 20, 0, 0)
    // Default NaN: every NaN result is the default NaN.
    OP(DADD, DN, 8, 0, 0)
    OP(DMUL, DN, 9, 0, 0)
    OP(DMIN, DN, 9, 0, 0)
    OP(DFLOOR, DN, 8, 0, 0)
    OP(DTOS, DN, 8, 0, 0)
    OP(SSQRT, DN, 8, 0, 0)
    OP(DNEG, DN, 8, 0, 0)     // negation does not process NaNs
    OP(DADD, FZ | DN, 9, 10, 0)
};

static double dget(int i) {
    union { u64 u; double d; } x = {DV[i]};
    return x.d;
}
static float sget(int i) {
    union { u32 u; float f; } x = {SV[i]};
    return x.f;
}
static u64 dbits(double d) {
    union { double d; u64 u; } x = {d};
    return x.u;
}
static u64 sbits(float f) {
    union { float f; u32 u; } x = {f};
    return x.u;
}

static void set_fpcr(u64 v) { __asm__ volatile("msr fpcr, %0" ::"r"(v) : "memory"); }

static volatile u64 sink;

static void put_dec3(int n) {
    char b[3] = {(char)('0' + n / 100 % 10), (char)('0' + n / 10 % 10), (char)('0' + n % 10)};
    svc_output_debug_string(b, 3);
}

int main(void) {
    for (unsigned i = 0; i < sizeof OPS / sizeof OPS[0]; i++) {
        const struct op *o = &OPS[i];
        set_fpcr(o->fpcr);
        // Operands are loaded after the FPCR write and the result is stored
        // before the next one, so the operation runs under this FPCR.
        // Each case reads only the table it uses.
#define A dget(o->a)
#define B dget(o->b)
#define C dget(o->c)
#define FA sget(o->a)
#define FB sget(o->b)
#define FC sget(o->c)
#define IA IV[o->a]
        u64 r;
        switch (o->kind) {
        case DADD: r = dbits(A + B); break;
        case DSUB: r = dbits(A - B); break;
        case DMUL: r = dbits(A * B); break;
        case DDIV: r = dbits(A / B); break;
        case DSQRT: r = dbits(__builtin_sqrt(A)); break;
        case DFMA: r = dbits(__builtin_fma(A, B, C)); break;
        case DNEG: r = dbits(-A); break;
        case DABS: r = dbits(__builtin_fabs(A)); break;
        case DMIN: r = dbits(__builtin_fmin(A, B)); break;
        case DMAX: r = dbits(__builtin_fmax(A, B)); break;
        case DFLOOR: r = dbits(__builtin_floor(A)); break;
        case DCEIL: r = dbits(__builtin_ceil(A)); break;
        case DTRUNC: r = dbits(__builtin_trunc(A)); break;
        case DROUND: r = dbits(__builtin_round(A)); break;
        case DRINT: r = dbits(__builtin_rint(A)); break;
        case DNEARBY: r = dbits(__builtin_nearbyint(A)); break;
        case DTOI32: r = (u32)(int)A; break;
        case DTOU32: r = (u32)A; break;
        case DTOI64: r = (u64)(long)A; break;
        case DTOU64: r = (u64)A; break;
        case DLROUND: r = (u64)__builtin_lround(A); break;
        case DTOS: r = sbits((float)A); break;
        case DLT: r = A < B; break;
        case DLE: r = A <= B; break;
        case DEQ: r = A == B; break;
        case DUNORD: r = __builtin_isunordered(A, B); break;
        case SADD: r = sbits(FA + FB); break;
        case SSUB: r = sbits(FA - FB); break;
        case SMUL: r = sbits(FA * FB); break;
        case SDIV: r = sbits(FA / FB); break;
        case SSQRT: r = sbits(__builtin_sqrtf(FA)); break;
        case SFMA: r = sbits(__builtin_fmaf(FA, FB, FC)); break;
        case SMIN: r = sbits(__builtin_fminf(FA, FB)); break;
        case SMAX: r = sbits(__builtin_fmaxf(FA, FB)); break;
        case SRINT: r = sbits(__builtin_rintf(FA)); break;
        case STOI32: r = (u32)(int)FA; break;
        case STOD: r = dbits((double)FA); break;
        case I64TOD: r = dbits((double)(long)IA); break;
        case U64TOD: r = dbits((double)IA); break;
        case I32TOS: r = sbits((float)(int)IA); break;
        default: r = sbits((float)IA); break; // U64TOS
        }
#undef A
#undef B
#undef C
#undef FA
#undef FB
#undef FC
#undef IA
        sink = r;
        set_fpcr(0);
        put_dec3((int)i);
        puts_(" ");
        puts_(NAMES[o->kind]);
        puts_(" = ");
        put_hex(sink);
        puts_("\n");
    }
    return 0;
}
