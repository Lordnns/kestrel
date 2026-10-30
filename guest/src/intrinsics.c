// build: simd-only
// NEON intrinsics: one or more per instruction class, each printed as the
// exact bytes of its result (16-byte chunks, little-endian, as hex).
//
// Each T("asm", d, a, b, expr) line names the instruction the intrinsic
// means and the input vectors it reads: D, A, B index IN, and the assembly
// uses v0 = IN[D], v1 = IN[A], v2 = IN[B], v3 = IN[B+1], v4 = IN[B+2]. Loads
// read from &IN[A]; TS lines store into a zeroed buffer. The compiler may
// pick other instructions with the same result. scripts/gen-expected.py
// reads these lines and models each assembly line with the lane models of
// scripts/gen-neon-vectors.py.
#include <arm_neon.h>

#include "rt.h"

static volatile const u64 IN[14][2] = {
    {0x5957c4fff8720a9a, 0x4893255275cede06}, //  0: random
    {0xb890f5862e5ccbf0, 0x755f459aaf5f4e1c}, //  1: random
    {0x7fffffffffffffff, 0x8000000000000000}, //  2: 64-bit max and min
    {0x80007fffffff0001, 0x807fff00807fff00}, //  3: byte and halfword extremes
    {0x0001ffff0080ff7f, 0x7f8080017fff8000}, //  4: halfword extremes
    {0x05ff0001f707fe03, 0xf106f904fb0902fc}, //  5: small signed shift counts
    {0xc01000003fc00000, 0x3dcccccd501502f9}, //  6: f32 1.5 -2.25 1e10 0.1
    {0x40600000bf400000, 0x7fc123453b03126f}, //  7: f32 -0.75 3.5 2e-3 qNaN
    {0x400921fb54442d18, 0xfe37e43c8800759c}, //  8: f64 pi -1e300
    {0x4004000000000000, 0x01a56e1fc2f8f359}, //  9: f64 2.5 1e-300
    {0x7f80000080000000, 0x42c90000c06ccccd}, // 10: f32 -0 inf -3.7 100.5
    {0xbff8000000000000, 0x41e2a05f20000000}, // 11: f64 -1.5 2.5e9
    {0xffffff9c00000007, 0xffffffff075bcd15}, // 12: i32 7 -100 123456789 -1
    {0xbf00000040200000, 0xba83126f40e80000}, // 13: f32 2.5 -0.5 7.25 -1e-3
};
// The general register value used by DUP and INS (general).
static volatile const u64 X3 = 0x8899aabbccddeeff;

static uint8x16_t in(int i) {
    return vcombine_u8(vcreate_u8(IN[i % 14][0]), vcreate_u8(IN[i % 14][1]));
}
// The u8 "reinterpret" is the identity; arm_neon.h has none.
static inline uint8x16_t vreinterpretq_u8_u8(uint8x16_t x) { return x; }
#define Q(t, i) vreinterpretq_##t##_u8(in(i))
#define H(t, i) vget_low_##t(Q(t, i))
#define MEM(i) ((const void *)&IN[i][0])

static unsigned count;

// Print "NNN asm = " and the bytes as hex, 16-byte chunks, little-endian.
static void out(const char *asm_, const void *p, unsigned size) {
    static const char hex[] = "0123456789abcdef";
    const u8 *b = p;
    char num[4] = {(char)('0' + count / 100 % 10), (char)('0' + count / 10 % 10),
                   (char)('0' + count % 10), ' '};
    count++;
    svc_output_debug_string(num, 4);
    puts_(asm_);
    puts_(" =");
    for (unsigned at = 0; at < size; at += 16) {
        unsigned n = size - at < 16 ? size - at : 16;
        char buf[34];
        buf[0] = ' ';
        for (unsigned k = 0; k < n; k++) {
            u8 v = b[at + n - 1 - k];
            buf[1 + 2 * k] = hex[v >> 4];
            buf[2 + 2 * k] = hex[v & 15];
        }
        svc_output_debug_string(buf, 1 + 2 * n);
    }
    puts_("\n");
}

#define T(asm_, d, a, b, expr)                                                                  \
    do {                                                                                        \
        int D = d, A = a, B = b;                                                                \
        (void)D, (void)A, (void)B;                                                              \
        __typeof__(expr) r_ = expr;                                                             \
        out(asm_, &r_, sizeof r_);                                                              \
    } while (0)

#define TS(asm_, a, b, size, stmt)                                                              \
    do {                                                                                        \
        int A = a, B = b;                                                                       \
        (void)A, (void)B;                                                                       \
        u8 buf_[64];                                                                            \
        __builtin_memset(buf_, 0, sizeof buf_);                                                 \
        stmt;                                                                                   \
        out(asm_, buf_, size);                                                                  \
    } while (0)

int main(void) {
    // Three same, integer.
    T("add v0.4s, v1.4s, v2.4s", 0, 0, 1, vaddq_s32(Q(s32, A), Q(s32, B)));
    T("sub v0.8h, v1.8h, v2.8h", 0, 0, 1, vsubq_u16(Q(u16, A), Q(u16, B)));
    T("sqadd v0.16b, v1.16b, v2.16b", 0, 3, 4, vqaddq_s8(Q(s8, A), Q(s8, B)));
    T("uqsub v0.4s, v1.4s, v2.4s", 0, 0, 1, vqsubq_u32(Q(u32, A), Q(u32, B)));
    T("sqsub v0.2d, v1.2d, v2.2d", 0, 2, 3, vqsubq_s64(Q(s64, A), Q(s64, B)));
    T("shadd v0.8h, v1.8h, v2.8h", 0, 0, 1, vhaddq_s16(Q(s16, A), Q(s16, B)));
    T("urhadd v0.16b, v1.16b, v2.16b", 0, 0, 1, vrhaddq_u8(Q(u8, A), Q(u8, B)));
    T("shsub v0.4s, v1.4s, v2.4s", 0, 0, 1, vhsubq_s32(Q(s32, A), Q(s32, B)));
    T("cmgt v0.4s, v1.4s, v2.4s", 0, 0, 1, vcgtq_s32(Q(s32, A), Q(s32, B)));
    T("cmhs v0.8h, v1.8h, v2.8h", 0, 3, 4, vcgeq_u16(Q(u16, A), Q(u16, B)));
    T("cmeq v0.16b, v1.16b, v2.16b", 0, 3, 4, vceqq_u8(Q(u8, A), Q(u8, B)));
    T("cmtst v0.4s, v1.4s, v2.4s", 0, 0, 1, vtstq_u32(Q(u32, A), Q(u32, B)));
    T("sshl v0.4s, v1.4s, v2.4s", 0, 0, 5, vshlq_s32(Q(s32, A), Q(s32, B)));
    T("uqshl v0.8h, v1.8h, v2.8h", 0, 0, 5, vqshlq_u16(Q(u16, A), Q(s16, B)));
    T("srshl v0.16b, v1.16b, v2.16b", 0, 0, 5, vrshlq_s8(Q(s8, A), Q(s8, B)));
    T("sqrshl v0.2d, v1.2d, v2.2d", 0, 2, 5, vqrshlq_s64(Q(s64, A), Q(s64, B)));
    T("smax v0.8h, v1.8h, v2.8h", 0, 0, 1, vmaxq_s16(Q(s16, A), Q(s16, B)));
    T("umin v0.4s, v1.4s, v2.4s", 0, 0, 1, vminq_u32(Q(u32, A), Q(u32, B)));
    T("sabd v0.16b, v1.16b, v2.16b", 0, 0, 1, vabdq_s8(Q(s8, A), Q(s8, B)));
    T("uaba v0.8h, v1.8h, v2.8h", 4, 0, 1, vabaq_u16(Q(u16, D), Q(u16, A), Q(u16, B)));
    T("mul v0.4s, v1.4s, v2.4s", 0, 0, 1, vmulq_s32(Q(s32, A), Q(s32, B)));
    T("mla v0.8h, v1.8h, v2.8h", 4, 0, 1, vmlaq_u16(Q(u16, D), Q(u16, A), Q(u16, B)));
    T("mls v0.4s, v1.4s, v2.4s", 4, 0, 1, vmlsq_s32(Q(s32, D), Q(s32, A), Q(s32, B)));
    T("pmul v0.16b, v1.16b, v2.16b", 0, 0, 1, vmulq_p8(Q(p8, A), Q(p8, B)));
    T("sqdmulh v0.8h, v1.8h, v2.8h", 0, 3, 3, vqdmulhq_s16(Q(s16, A), Q(s16, B)));
    T("sqrdmulh v0.4s, v1.4s, v2.4s", 0, 0, 1, vqrdmulhq_s32(Q(s32, A), Q(s32, B)));
    T("addp v0.8h, v1.8h, v2.8h", 0, 0, 1, vpaddq_s16(Q(s16, A), Q(s16, B)));
    T("umaxp v0.8b, v1.8b, v2.8b", 0, 0, 1, vpmax_u8(H(u8, A), H(u8, B)));
    T("sminp v0.4h, v1.4h, v2.4h", 0, 0, 1, vpmin_s16(H(s16, A), H(s16, B)));
    T("and v0.16b, v1.16b, v2.16b", 0, 0, 1, vandq_u8(Q(u8, A), Q(u8, B)));
    T("orr v0.16b, v1.16b, v2.16b", 0, 0, 1, vorrq_u8(Q(u8, A), Q(u8, B)));
    T("eor v0.16b, v1.16b, v2.16b", 0, 0, 1, veorq_u8(Q(u8, A), Q(u8, B)));
    T("bic v0.16b, v1.16b, v2.16b", 0, 0, 1, vbicq_u8(Q(u8, A), Q(u8, B)));
    T("orn v0.16b, v1.16b, v2.16b", 0, 0, 1, vornq_u8(Q(u8, A), Q(u8, B)));
    T("bsl v0.16b, v1.16b, v2.16b", 4, 0, 1, vbslq_u32(Q(u32, D), Q(u32, A), Q(u32, B)));

    // Two-register misc, integer.
    T("rev64 v0.16b, v1.16b", 0, 0, 0, vrev64q_u8(Q(u8, A)));
    T("rev32 v0.8h, v1.8h", 0, 0, 0, vrev32q_u16(Q(u16, A)));
    T("rev16 v0.16b, v1.16b", 0, 0, 0, vrev16q_u8(Q(u8, A)));
    T("saddlp v0.8h, v1.16b", 0, 0, 0, vpaddlq_s8(Q(s8, A)));
    T("uadalp v0.4s, v1.8h", 4, 0, 0, vpadalq_u16(Q(u32, D), Q(u16, A)));
    T("cls v0.8h, v1.8h", 0, 0, 0, vclsq_s16(Q(s16, A)));
    T("clz v0.4s, v1.4s", 0, 4, 0, vclzq_u32(Q(u32, A)));
    T("cnt v0.16b, v1.16b", 0, 0, 0, vcntq_u8(Q(u8, A)));
    T("not v0.16b, v1.16b", 0, 0, 0, vmvnq_u8(Q(u8, A)));
    T("rbit v0.16b, v1.16b", 0, 0, 0, vrbitq_u8(Q(u8, A)));
    T("sqabs v0.16b, v1.16b", 0, 3, 0, vqabsq_s8(Q(s8, A)));
    T("sqneg v0.4s, v1.4s", 0, 3, 0, vqnegq_s32(Q(s32, A)));
    T("abs v0.2d, v1.2d", 0, 0, 0, vabsq_s64(Q(s64, A)));
    T("neg v0.8h, v1.8h", 0, 0, 0, vnegq_s16(Q(s16, A)));
    T("cmgt v0.4s, v1.4s, #0", 0, 0, 0, vcgtzq_s32(Q(s32, A)));
    T("cmeq v0.16b, v1.16b, #0", 0, 3, 0, vceqzq_u8(Q(u8, A)));
    T("cmlt v0.8h, v1.8h, #0", 0, 4, 0, vcltzq_s16(Q(s16, A)));
    T("xtn v0.8b, v1.8h", 0, 0, 0, vmovn_u16(Q(u16, A)));
    T("xtn2 v0.8h, v1.4s", 4, 0, 0, vmovn_high_s32(H(s16, D), Q(s32, A)));
    T("sqxtn v0.4h, v1.4s", 0, 3, 0, vqmovn_s32(Q(s32, A)));
    T("sqxtun v0.8b, v1.8h", 0, 4, 0, vqmovun_s16(Q(s16, A)));
    T("uqxtn2 v0.4s, v1.2d", 4, 2, 0, vqmovn_high_u64(H(u32, D), Q(u64, A)));
    T("suqadd v0.4s, v1.4s", 3, 0, 0, vuqaddq_s32(Q(s32, D), Q(u32, A)));
    T("usqadd v0.16b, v1.16b", 4, 3, 0, vsqaddq_u8(Q(u8, D), Q(s8, A)));

    // Three different.
    T("saddl v0.8h, v1.8b, v2.8b", 0, 0, 1, vaddl_s8(H(s8, A), H(s8, B)));
    T("uaddl2 v0.4s, v1.8h, v2.8h", 0, 0, 1, vaddl_high_u16(Q(u16, A), Q(u16, B)));
    T("saddw v0.2d, v1.2d, v2.2s", 0, 0, 1, vaddw_s32(Q(s64, A), H(s32, B)));
    T("usubl v0.8h, v1.8b, v2.8b", 0, 0, 1, vsubl_u8(H(u8, A), H(u8, B)));
    T("sabal v0.4s, v1.4h, v2.4h", 4, 0, 1, vabal_s16(Q(s32, D), H(s16, A), H(s16, B)));
    T("uabdl2 v0.8h, v1.16b, v2.16b", 0, 0, 1, vabdl_high_u8(Q(u8, A), Q(u8, B)));
    T("smlal v0.2d, v1.2s, v2.2s", 4, 0, 1, vmlal_s32(Q(s64, D), H(s32, A), H(s32, B)));
    T("umlsl2 v0.4s, v1.8h, v2.8h", 4, 0, 1, vmlsl_high_u16(Q(u32, D), Q(u16, A), Q(u16, B)));
    T("smull v0.4s, v1.4h, v2.4h", 0, 0, 1, vmull_s16(H(s16, A), H(s16, B)));
    T("umull2 v0.2d, v1.4s, v2.4s", 0, 0, 1, vmull_high_u32(Q(u32, A), Q(u32, B)));
    T("sqdmull v0.4s, v1.4h, v2.4h", 0, 3, 3, vqdmull_s16(H(s16, A), H(s16, B)));
    T("sqdmlal v0.2d, v1.2s, v2.2s", 2, 0, 1, vqdmlal_s32(Q(s64, D), H(s32, A), H(s32, B)));
    T("pmull v0.8h, v1.8b, v2.8b", 0, 0, 1, vmull_p8(H(p8, A), H(p8, B)));
    T("addhn v0.4h, v1.4s, v2.4s", 0, 0, 1, vaddhn_s32(Q(s32, A), Q(s32, B)));
    T("subhn v0.2s, v1.2d, v2.2d", 0, 0, 1, vsubhn_s64(Q(s64, A), Q(s64, B)));
    T("raddhn2 v0.16b, v1.8h, v2.8h", 4, 0, 1, vraddhn_high_u16(H(u8, D), Q(u16, A), Q(u16, B)));

    // Across lanes.
    T("addv s0, v1.4s", 0, 0, 0, vaddvq_s32(Q(s32, A)));
    T("addv b0, v1.16b", 0, 0, 0, vaddvq_u8(Q(u8, A)));
    T("saddlv s0, v1.8h", 0, 0, 0, vaddlvq_s16(Q(s16, A)));
    T("umaxv b0, v1.16b", 0, 0, 0, vmaxvq_u8(Q(u8, A)));
    T("sminv s0, v1.4s", 0, 0, 0, vminvq_s32(Q(s32, A)));

    // Shift by immediate.
    T("sshr v0.4s, v1.4s, #5", 0, 0, 0, vshrq_n_s32(Q(s32, A), 5));
    T("ushr v0.2d, v1.2d, #63", 0, 0, 0, vshrq_n_u64(Q(u64, A), 63));
    T("usra v0.16b, v1.16b, #3", 4, 0, 0, vsraq_n_u8(Q(u8, D), Q(u8, A), 3));
    T("srshr v0.8h, v1.8h, #1", 0, 0, 0, vrshrq_n_s16(Q(s16, A), 1));
    T("ursra v0.4s, v1.4s, #31", 4, 0, 0, vrsraq_n_u32(Q(u32, D), Q(u32, A), 31));
    T("shl v0.16b, v1.16b, #3", 0, 0, 0, vshlq_n_s8(Q(s8, A), 3));
    T("sli v0.4s, v1.4s, #13", 4, 0, 0, vsliq_n_u32(Q(u32, D), Q(u32, A), 13));
    T("sri v0.8h, v1.8h, #7", 4, 0, 0, vsriq_n_u16(Q(u16, D), Q(u16, A), 7));
    T("sqshl v0.8h, v1.8h, #4", 0, 4, 0, vqshlq_n_s16(Q(s16, A), 4));
    T("sqshlu v0.4s, v1.4s, #2", 0, 3, 0, vqshluq_n_s32(Q(s32, A), 2));
    T("shrn v0.4h, v1.4s, #7", 0, 0, 0, vshrn_n_u32(Q(u32, A), 7));
    T("rshrn2 v0.16b, v1.8h, #3", 4, 0, 0, vrshrn_high_n_s16(H(s8, D), Q(s16, A), 3));
    T("sqshrn v0.4h, v1.4s, #4", 0, 3, 0, vqshrn_n_s32(Q(s32, A), 4));
    T("sqrshrun v0.8b, v1.8h, #2", 0, 4, 0, vqrshrun_n_s16(Q(s16, A), 2));
    T("sshll v0.8h, v1.8b, #3", 0, 0, 0, vshll_n_s8(H(s8, A), 3));
    T("uxtl v0.4s, v1.4h", 0, 0, 0, vmovl_u16(H(u16, A)));
    T("ushll2 v0.2d, v1.4s, #31", 0, 0, 0, vshll_high_n_u32(Q(u32, A), 31));
    T("scvtf v0.4s, v1.4s, #8", 0, 12, 0, vcvtq_n_f32_s32(Q(s32, A), 8));
    T("fcvtzs v0.4s, v1.4s, #4", 0, 13, 0, vcvtq_n_s32_f32(Q(f32, A), 4));

    // By element.
    T("mul v0.8h, v1.8h, v2.h[5]", 0, 0, 1, vmulq_laneq_s16(Q(s16, A), Q(s16, B), 5));
    T("mla v0.4s, v1.4s, v2.s[3]", 4, 0, 1, vmlaq_laneq_u32(Q(u32, D), Q(u32, A), Q(u32, B), 3));
    T("smull v0.4s, v1.4h, v2.h[6]", 0, 0, 1, vmull_laneq_s16(H(s16, A), Q(s16, B), 6));
    T("sqdmulh v0.4s, v1.4s, v2.s[1]", 0, 0, 1, vqdmulhq_laneq_s32(Q(s32, A), Q(s32, B), 1));
    T("sqrdmulh v0.8h, v1.8h, v2.h[7]", 0, 3, 3, vqrdmulhq_laneq_s16(Q(s16, A), Q(s16, B), 7));
    T("fmla v0.4s, v1.4s, v2.s[2]", 13, 6, 7, vfmaq_laneq_f32(Q(f32, D), Q(f32, A), Q(f32, B), 2));
    T("fmul v0.2d, v1.2d, v2.d[1]", 0, 8, 11, vmulq_laneq_f64(Q(f64, A), Q(f64, B), 1));

    // Copies, permutes, tables, immediates.
    T("dup v0.4s, w3", 0, 0, 0, vdupq_n_u32((u32)X3));
    T("dup v0.16b, v1.b[13]", 0, 0, 0, vdupq_laneq_u8(Q(u8, A), 13));
    T("mov v0.h[5], w3", 4, 0, 0, vsetq_lane_u16((unsigned short)X3, Q(u16, D), 5));
    T("mov v0.s[3], v1.s[0]", 4, 0, 0, vcopyq_laneq_u32(Q(u32, D), 3, Q(u32, A), 0));
    T("zip1 v0.16b, v1.16b, v2.16b", 0, 0, 1, vzip1q_u8(Q(u8, A), Q(u8, B)));
    T("zip2 v0.8h, v1.8h, v2.8h", 0, 0, 1, vzip2q_u16(Q(u16, A), Q(u16, B)));
    T("uzp1 v0.4s, v1.4s, v2.4s", 0, 0, 1, vuzp1q_s32(Q(s32, A), Q(s32, B)));
    T("uzp2 v0.16b, v1.16b, v2.16b", 0, 0, 1, vuzp2q_u8(Q(u8, A), Q(u8, B)));
    T("trn1 v0.8h, v1.8h, v2.8h", 0, 0, 1, vtrn1q_u16(Q(u16, A), Q(u16, B)));
    T("trn2 v0.4s, v1.4s, v2.4s", 0, 0, 1, vtrn2q_s32(Q(s32, A), Q(s32, B)));
    T("ext v0.16b, v1.16b, v2.16b, #5", 0, 0, 1, vextq_u8(Q(u8, A), Q(u8, B), 5));
    T("tbl v0.16b, {v1.16b}, v0.16b", 5, 0, 0, vqtbl1q_u8(Q(u8, A), Q(u8, D)));
    T("tbl v0.16b, {v1.16b, v2.16b}, v0.16b", 5, 0, 1,
      vqtbl2q_u8((uint8x16x2_t){{Q(u8, A), Q(u8, B)}}, Q(u8, D)));
    T("tbx v0.16b, {v1.16b}, v2.16b", 4, 0, 5, vqtbx1q_u8(Q(u8, D), Q(u8, A), Q(u8, B)));
    T("tbl v0.16b, {v1.16b, v2.16b, v3.16b, v4.16b}, v0.16b", 3, 0, 1,
      vqtbl4q_u8((uint8x16x4_t){{Q(u8, A), Q(u8, B), Q(u8, B + 1), Q(u8, B + 2)}}, Q(u8, D)));
    T("movi v0.4s, #0x5a, lsl #16", 0, 0, 0, vdupq_n_u32(0x5a0000));
    T("fmov v0.4s, #1.5", 0, 0, 0, vdupq_n_f32(1.5f));

    // Floating point.
    T("fadd v0.4s, v1.4s, v2.4s", 0, 6, 7, vaddq_f32(Q(f32, A), Q(f32, B)));
    T("fsub v0.2d, v1.2d, v2.2d", 0, 8, 9, vsubq_f64(Q(f64, A), Q(f64, B)));
    T("fmul v0.4s, v1.4s, v2.4s", 0, 6, 13, vmulq_f32(Q(f32, A), Q(f32, B)));
    T("fdiv v0.2d, v1.2d, v2.2d", 0, 11, 9, vdivq_f64(Q(f64, A), Q(f64, B)));
    T("fmla v0.4s, v1.4s, v2.4s", 13, 6, 7, vfmaq_f32(Q(f32, D), Q(f32, A), Q(f32, B)));
    T("fmls v0.2d, v1.2d, v2.2d", 9, 8, 11, vfmsq_f64(Q(f64, D), Q(f64, A), Q(f64, B)));
    T("fmax v0.4s, v1.4s, v2.4s", 0, 6, 7, vmaxq_f32(Q(f32, A), Q(f32, B)));
    T("fminnm v0.4s, v1.4s, v2.4s", 0, 6, 7, vminnmq_f32(Q(f32, A), Q(f32, B)));
    T("fabd v0.4s, v1.4s, v2.4s", 0, 6, 13, vabdq_f32(Q(f32, A), Q(f32, B)));
    T("fcmeq v0.4s, v1.4s, v2.4s", 0, 6, 6, vceqq_f32(Q(f32, A), Q(f32, B)));
    T("fcmge v0.2d, v1.2d, v2.2d", 0, 8, 9, vcgeq_f64(Q(f64, A), Q(f64, B)));
    T("facgt v0.4s, v1.4s, v2.4s", 0, 6, 7, vcagtq_f32(Q(f32, A), Q(f32, B)));
    T("faddp v0.4s, v1.4s, v2.4s", 0, 6, 13, vpaddq_f32(Q(f32, A), Q(f32, B)));
    T("fmaxp v0.2d, v1.2d, v2.2d", 0, 8, 9, vpmaxq_f64(Q(f64, A), Q(f64, B)));
    T("fmulx v0.4s, v1.4s, v2.4s", 0, 10, 6, vmulxq_f32(Q(f32, A), Q(f32, B)));
    T("frecps v0.4s, v1.4s, v2.4s", 0, 6, 13, vrecpsq_f32(Q(f32, A), Q(f32, B)));
    T("frsqrts v0.4s, v1.4s, v2.4s", 0, 6, 13, vrsqrtsq_f32(Q(f32, A), Q(f32, B)));
    T("frecpe v0.4s, v1.4s", 0, 6, 0, vrecpeq_f32(Q(f32, A)));
    T("frsqrte v0.4s, v1.4s", 0, 13, 0, vrsqrteq_f32(Q(f32, A)));
    T("fsqrt v0.2d, v1.2d", 0, 9, 0, vsqrtq_f64(Q(f64, A)));
    T("fabs v0.4s, v1.4s", 0, 10, 0, vabsq_f32(Q(f32, A)));
    T("fneg v0.2d, v1.2d", 0, 11, 0, vnegq_f64(Q(f64, A)));
    T("frintn v0.4s, v1.4s", 0, 13, 0, vrndnq_f32(Q(f32, A)));
    T("frintm v0.2d, v1.2d", 0, 11, 0, vrndmq_f64(Q(f64, A)));
    T("frinta v0.4s, v1.4s", 0, 13, 0, vrndaq_f32(Q(f32, A)));
    T("fcvtzs v0.4s, v1.4s", 0, 6, 0, vcvtq_s32_f32(Q(f32, A)));
    T("fcvtnu v0.4s, v1.4s", 0, 13, 0, vcvtnq_u32_f32(Q(f32, A)));
    T("scvtf v0.2d, v1.2d", 0, 2, 0, vcvtq_f64_s64(Q(s64, A)));
    T("fcvtn v0.2s, v1.2d", 0, 8, 0, vcvt_f32_f64(Q(f64, A)));
    T("fcvtl2 v0.2d, v1.4s", 0, 7, 0, vcvt_high_f64_f32(Q(f32, A)));
    T("fcvtxn v0.2s, v1.2d", 0, 8, 0, vcvtx_f32_f64(Q(f64, A)));
    T("fcmgt v0.4s, v1.4s, #0.0", 0, 13, 0, vcgtzq_f32(Q(f32, A)));
    T("urecpe v0.2s, v1.2s", 0, 0, 0, vrecpe_u32(H(u32, A)));
    T("fmaxv s0, v1.4s", 0, 6, 0, vmaxvq_f32(Q(f32, A)));
    T("fminnmv s0, v1.4s", 0, 7, 0, vminnmvq_f32(Q(f32, A)));

    // Structure loads (from &IN[A]) and stores (into a zeroed buffer).
    T("ld1 {v0.16b}, [x1]", 0, 3, 0, vld1q_u8(MEM(A)));
    T("ld2 {v0.8h, v1.8h}, [x1]", 0, 0, 0, vld2q_u16(MEM(A)));
    T("ld3 {v0.4s, v1.4s, v2.4s}, [x1]", 0, 1, 0, vld3q_u32(MEM(A)));
    T("ld4 {v0.16b, v1.16b, v2.16b, v3.16b}, [x1]", 0, 2, 0, vld4q_u8(MEM(A)));
    T("ld1r {v0.4s}, [x1]", 0, 5, 0, vld1q_dup_u32(MEM(A)));
    T("ld1 {v0.h}[3], [x1]", 4, 6, 0, vld1q_lane_u16(MEM(A), Q(u16, D), 3));
    TS("st2 {v1.16b, v2.16b}, [x1]", 0, 1, 32,
       vst2q_u8(buf_, ((uint8x16x2_t){{Q(u8, A), Q(u8, B)}})));
    TS("st3 {v1.4s, v2.4s, v3.4s}, [x1]", 0, 1, 48,
       vst3q_u32((u32 *)buf_, ((uint32x4x3_t){{Q(u32, A), Q(u32, B), Q(u32, B + 1)}})));
    TS("st4 {v1.8h, v2.8h, v3.8h, v4.8h}, [x1]", 0, 1, 64,
       vst4q_u16((unsigned short *)buf_,
                 ((uint16x8x4_t){{Q(u16, A), Q(u16, B), Q(u16, B + 1), Q(u16, B + 2)}})));
    TS("st1 {v1.s}[2], [x1]", 0, 0, 4, vst1q_lane_u32((u32 *)buf_, Q(u32, A), 2));
    return 0;
}
