//! Scalar floating-point tests.
//!
//! Encodings come from clang 21 (`--target=aarch64`) and llvm-objdump, from
//! the assembly in each comment. Expected values are IEEE results computed
//! in Python (`struct`, `math.fma`, `fractions`), or, for NaN handling,
//! flush-to-zero and the estimate instructions, worked by hand from the Arm
//! pseudocode as each comment says. None comes from running kestrel.

use super::tests::setup;
use super::Interpreter;
use crate::{CpuBackend, CpuState, Exit, Unmodelled};

const FZ: u64 = 1 << 24;
const DN: u64 = 1 << 25;
const RP: u64 = 1 << 22;
const RM: u64 = 2 << 22;
const RZ: u64 = 3 << 22;
const BRK: u32 = 0xd4200000;

/// Run `insn` with FPCR = `fpcr` after `init`, and return the state at BRK.
fn exec(insn: u32, fpcr: u64, init: impl FnOnce(&mut CpuState)) -> CpuState {
    let (mut s, mut m) = setup(&[insn, BRK]);
    s.fpcr = fpcr;
    init(&mut s);
    let exit = Interpreter::new().run(&mut s, &mut m, 10);
    assert_eq!(exit, Exit::Breakpoint(0), "{insn:08x}");
    s
}

/// Check V0 (all 128 bits, so the upper part must be cleared) after
/// running `insn` with V1, V2, V3 = `ins` and V0 initially all ones.
#[track_caller]
fn check(insn: u32, fpcr: u64, ins: &[u64], want: u64) {
    let s = exec(insn, fpcr, |s| {
        s.v[0] = u128::MAX;
        for (k, &v) in ins.iter().enumerate() {
            s.v[k + 1] = v as u128;
        }
    });
    assert_eq!(
        s.v[0], want as u128,
        "{insn:08x} inputs {ins:x?} fpcr {fpcr:#x}: got {:#x}, want {want:#x}",
        s.v[0]
    );
}

/// X0 after running `insn` with V1 = `v1`.
#[track_caller]
fn to_int(insn: u32, fpcr: u64, v1: u64, want: u64) {
    let s = exec(insn, fpcr, |s| {
        s.x[0] = 0xdead_dead_dead_dead;
        s.v[1] = v1 as u128;
    });
    assert_eq!(s.x[0], want, "{insn:08x} input {v1:#x}: got {:#x}", s.x[0]);
}

/// V0 after running `insn` with X1 = `x1`.
#[track_caller]
fn from_int(insn: u32, fpcr: u64, x1: u64, want: u64) {
    let s = exec(insn, fpcr, |s| s.x[1] = x1);
    assert_eq!(
        s.v[0], want as u128,
        "{insn:08x} input {x1:#x}: got {:#x}",
        s.v[0]
    );
}

/// NZCV (as 4 bits) after running `insn` with flags `nzcv` and V1, V2 = `ins`.
fn flags(insn: u32, nzcv: u64, ins: [u64; 2]) -> u64 {
    let s = exec(insn, 0, |s| {
        s.flags = crate::Flags::from_nzcv(nzcv << 28);
        s.v[1] = ins[0] as u128;
        s.v[2] = ins[1] as u128;
    });
    s.flags.to_nzcv() >> 28
}

fn unsupported(insn: u32, fpcr: u64) -> Unmodelled {
    let (mut s, mut m) = setup(&[insn, BRK]);
    s.fpcr = fpcr;
    match Interpreter::new().run(&mut s, &mut m, 10) {
        Exit::Unsupported { what, .. } => what,
        other => panic!("{insn:08x}: expected Unsupported, got {other:?}"),
    }
}

// Single and double precision bit patterns used below.
const S1: u64 = 0x3f80_0000; // 1.0f
const S2: u64 = 0x4000_0000; // 2.0f
const D1: u64 = 0x3ff0_0000_0000_0000; // 1.0
const SQNAN1: u64 = 0x7fc0_0001;
const SSNAN2: u64 = 0x7f80_0002;
const S_DEFAULT_NAN: u64 = 0x7fc0_0000;
const D_DEFAULT_NAN: u64 = 0x7ff8_0000_0000_0000;

#[test]
fn arithmetic() {
    check(0x1e222820, 0, &[0x3fc0_0000, 0x4010_0000], 0x4070_0000); // fadd s: 1.5 + 2.25
    check(
        0x1e623820,
        0,
        &[D1, 0x3fb9_9999_9999_999a],
        0x3fec_cccc_cccc_cccd,
    ); // fsub d: 1 - 0.1
    check(0x1e220820, 0, &[0x4040_0000, 0x40e0_0000], 0x41a8_0000); // fmul s: 3 * 7
    check(
        0x1e621820,
        0,
        &[D1, 0x4008_0000_0000_0000],
        0x3fd5_5555_5555_5555,
    ); // fdiv d: 1/3
    check(0x1e221820, 0, &[S1, 0x4040_0000], 0x3eaa_aaab); // fdiv s: 1/3
    check(
        0x1e628820,
        0,
        &[0x4000_0000_0000_0000, 0x4008_0000_0000_0000],
        0xc018_0000_0000_0000,
    ); // fnmul d: -(2*3)
    check(
        0x1e621820,
        0,
        &[0xbff0_0000_0000_0000, 0],
        0xfff0_0000_0000_0000,
    ); // fdiv d: -1/0 = -inf
    check(
        0x1e61c020,
        0,
        &[0x4000_0000_0000_0000],
        0x3ff6_a09e_667f_3bcd,
    ); // fsqrt d: sqrt 2
}

#[test]
fn nan_rules() {
    // fadd s: a quiet NaN propagates unchanged.
    check(0x1e222820, 0, &[SQNAN1, S1], SQNAN1);
    // fadd s: a signalling NaN beats an earlier quiet NaN and is quietened.
    check(0x1e222820, 0, &[SQNAN1, SSNAN2], 0x7fc0_0002);
    // ...and FPCR.DN replaces it with the default NaN.
    check(0x1e222820, DN, &[SQNAN1, SSNAN2], S_DEFAULT_NAN);
    // Generated NaNs are the positive default NaN on every host
    // (x86 hardware would produce 0xffc00000 here).
    check(0x1e222820, 0, &[0x7f80_0000, 0xff80_0000], S_DEFAULT_NAN); // fadd s: inf + -inf
    check(0x1e620820, 0, &[0, 0x7ff0_0000_0000_0000], D_DEFAULT_NAN); // fmul d: 0 * inf
    check(0x1e21c020, 0, &[0xbf80_0000], S_DEFAULT_NAN); // fsqrt s: sqrt -1
    check(
        0x1e61c020,
        0,
        &[0x8000_0000_0000_0000],
        0x8000_0000_0000_0000,
    ); // fsqrt d: sqrt -0 = -0
       // fnmul negates NaN results too (FPNeg after FPMul).
    check(
        0x1e628820,
        0,
        &[0, 0x7ff0_0000_0000_0000],
        0xfff8_0000_0000_0000,
    );
    // fabs / fneg only touch the sign: no quietening.
    check(0x1e20c020, 0, &[0xff80_0000], 0x7f80_0000);
    check(
        0x1e614020,
        0,
        &[0x7ff0_0000_0000_0001],
        0xfff0_0000_0000_0001,
    );
    // fmov s0, s1 copies 32 bits and clears the rest of V0.
    check(0x1e204020, 0, &[0xaaaa_aaaa_aaaa_aaaa], 0xaaaa_aaaa);
}

#[test]
fn flush_to_zero() {
    // fmul s: 2^-100 * 2^-30 = 2^-130 is subnormal, flushed to +0 under FZ.
    check(0x1e220820, 0, &[0x0d80_0000, 0x3080_0000], 0x0008_0000);
    check(0x1e220820, FZ, &[0x0d80_0000, 0x3080_0000], 0);
    // fmul s: a subnormal input (2^-127) is flushed before the multiply.
    check(0x1e220820, 0, &[0x0040_0000, S2], 0x0080_0000);
    check(0x1e220820, FZ, &[0x0040_0000, S2], 0);
    // fmul d: (1 - 2^-53) * 2^-1022 is below the smallest normal but rounds
    // up to it. Arm flushes on the exact value, so FZ still gives +0.
    let (a, b) = (0x3fef_ffff_ffff_ffff, 0x0010_0000_0000_0000);
    check(0x1e620820, 0, &[a, b], 0x0010_0000_0000_0000);
    check(0x1e620820, FZ, &[a, b], 0);
    // Same edge in single precision: (1 - 2^-24) * 2^-126.
    check(0x1e220820, 0, &[0x3f7f_ffff, 0x0080_0000], 0x0080_0000);
    check(0x1e220820, FZ, &[0x3f7f_ffff, 0x0080_0000], 0);
}

#[test]
fn max_min() {
    check(0x1e224820, 0, &[0x8000_0000, 0], 0); // fmax s: max(-0, +0) = +0
    check(0x1e225820, 0, &[0, 0x8000_0000], 0x8000_0000); // fmin s: min(+0, -0) = -0
    check(
        0x1e624820,
        0,
        &[0x7ff8_0000_0000_0001, D1],
        0x7ff8_0000_0000_0001,
    ); // fmax d: NaN wins
    check(0x1e626820, 0, &[0x7ff8_0000_0000_0001, D1], D1); // fmaxnm d: number wins
    check(0x1e227820, 0, &[SSNAN2, S1], 0x7fc0_0002); // fminnm s: SNaN still propagates
}

#[test]
fn fused_multiply_add() {
    let (d2, d3) = (0x4000_0000_0000_0000, 0x4008_0000_0000_0000);
    check(0x1f420c20, 0, &[d2, d3, D1], 0x401c_0000_0000_0000); // fmadd d: 1 + 2*3
    check(0x1f028c20, 0, &[S2, 0x4040_0000, 0x4120_0000], 0x4080_0000); // fmsub s: 10 - 2*3
    check(0x1f620c20, 0, &[d2, d3, D1], 0xc01c_0000_0000_0000); // fnmadd d: -1 - 2*3
    check(0x1f228c20, 0, &[S2, 0x4040_0000, S1], 0x40a0_0000); // fnmsub s: -1 + 2*3
                                                               // fmadd d: n*n + a with n = 1 + 2^-30, a = -(1 + 2^-29). Rounded once the
                                                               // result is 2^-60; rounding the product first would give 0.
    let (n, a) = (0x3ff0_0000_0040_0000, 0xbff0_0000_0080_0000);
    check(0x1f420c20, 0, &[n, n, a], 0x3c30_0000_0000_0000);
    // fmadd d: quiet NaN addend with inf * 0 gives the default NaN.
    check(
        0x1f420c20,
        0,
        &[0x7ff0_0000_0000_0000, 0, 0x7ff8_0000_0000_0001],
        D_DEFAULT_NAN,
    );
    // fmsub d negates n before NaN processing, so a NaN n comes out negated.
    check(
        0x1f428c20,
        0,
        &[0x7ff8_0000_0000_0005, D1, D1],
        0xfff8_0000_0000_0005,
    );
}

#[test]
fn precision_conversion() {
    check(0x1e22c020, 0, &[0x3dcc_cccd], 0x3fb9_9999_a000_0000); // fcvt d, s: 0.1f exactly
    check(0x1e624020, 0, &[0x3fb9_9999_9999_999a], 0x3dcc_cccd); // fcvt s, d: 0.1, nearest
    check(0x1e624020, RZ, &[0x3fb9_9999_9999_999a], 0x3dcc_cccc); // ... toward zero
    let big = 0x7e37_e43c_8800_759c; // 1e300
    check(0x1e624020, 0, &[big], 0x7f80_0000); // overflow to +inf
    check(0x1e624020, RZ, &[big], 0x7f7f_ffff); // ... or max normal toward zero
    check(0x1e624020, 0, &[0x3730_0000_0000_0000], 0x200); // 2^-140 becomes subnormal
    check(0x1e624020, FZ, &[0x3730_0000_0000_0000], 0); // ... or +0 under FZ
                                                        // FPConvertNaN: d->s keeps fraction bits 50..29 and sets the quiet bit;
                                                        // s->d keeps the sign and shifts the payload up by 29.
    check(0x1e624020, 0, &[0x7ff4_0000_0000_0001], 0x7fe0_0000);
    check(0x1e22c020, 0, &[0xff80_0001], 0xfff8_0000_2000_0000);
    check(0x1e22c020, DN, &[0xff80_0001], D_DEFAULT_NAN);
}

#[test]
fn round_to_integral() {
    check(
        0x1e644020,
        0,
        &[0x4004_0000_0000_0000],
        0x4000_0000_0000_0000,
    ); // frintn d: 2.5 -> 2
    check(
        0x1e664020,
        0,
        &[0x4004_0000_0000_0000],
        0x4008_0000_0000_0000,
    ); // frinta d: 2.5 -> 3
    check(0x1e24c020, 0, &[0xbfc0_0000], 0xbf80_0000); // frintp s: -1.5 -> -1
    check(0x1e254020, 0, &[0xbfc0_0000], 0xc000_0000); // frintm s: -1.5 -> -2
    check(
        0x1e65c020,
        0,
        &[0xc005_9999_9999_999a],
        0xc000_0000_0000_0000,
    ); // frintz d: -2.7 -> -2
    check(0x1e244020, 0, &[0xbecc_cccd], 0x8000_0000); // frintn s: -0.4 -> -0
    check(
        0x1e674020,
        0,
        &[0x400c_0000_0000_0000],
        0x4010_0000_0000_0000,
    ); // frintx d: 3.5 -> 4
    check(
        0x1e67c020,
        RP,
        &[0x4004_0000_0000_0000],
        0x4008_0000_0000_0000,
    ); // frinti d: 2.5, +inf mode
    check(
        0x1e67c020,
        RM,
        &[0x4004_0000_0000_0000],
        0x4000_0000_0000_0000,
    ); // frinti d: 2.5, -inf mode
    check(
        0x1e644020,
        0,
        &[0x43b0_0000_0000_0000],
        0x43b0_0000_0000_0000,
    ); // frintn d: 2^60 unchanged
    check(0x1e25c020, 0, &[0x7f80_0001], 0x7fc0_0001); // frintz s: SNaN quietened
}

#[test]
fn compare_and_select() {
    assert_eq!(flags(0x1e222020, 0, [S1, S2]), 0b1000); // fcmp s: 1 < 2
    assert_eq!(
        flags(
            0x1e622020,
            0,
            [0x4008_0000_0000_0000, 0x4000_0000_0000_0000]
        ),
        0b0010
    ); // fcmp d: 3 > 2
    assert_eq!(flags(0x1e602028, 0, [0x8000_0000_0000_0000, 0]), 0b0110); // fcmp d: -0 == #0.0
    assert_eq!(flags(0x1e222030, 0, [SQNAN1, S1]), 0b0011); // fcmpe s: unordered
                                                            // fccmp s1, s2, #4, eq: compares only when Z is set, else NZCV = #4.
    assert_eq!(flags(0x1e220424, 0b0000, [S1, S1]), 0b0100);
    assert_eq!(flags(0x1e220424, 0b0100, [S1, S2]), 0b1000);
    // fcsel d0, d1, d2, eq
    let pick = |nzcv: u64| {
        let s = exec(0x1e620c20, 0, |s| {
            s.flags = crate::Flags::from_nzcv(nzcv << 28);
            s.v[1] = 1;
            s.v[2] = 2;
        });
        s.v[0]
    };
    assert_eq!(pick(0b0100), 1);
    assert_eq!(pick(0b0000), 2);
}

#[test]
fn move_immediate() {
    check(0x1e2e1000, 0, &[], 0x3f80_0000); // fmov s0, #1.0
    check(0x1e781000, 0, &[], 0xbfc0_0000_0000_0000); // fmov d0, #-0.125
    check(0x1e27f000, 0, &[], 0x41f8_0000); // fmov s0, #31.0
}

#[test]
fn to_integer() {
    to_int(0x1e380020, 0, 0xbff3_3333, 0xffff_ffff); // fcvtzs w0, s1: -1.9 -> -1 (W zero-extends)
    to_int(0x9e780020, 0, 0x4415_af1d_78b5_8c40, 0x7fff_ffff_ffff_ffff); // fcvtzs x0, d1: 1e20 saturates
    to_int(0x1e390020, 0, 0xc040_0000, 0); // fcvtzu w0, s1: -3 -> 0
    to_int(0x9e790020, 0, D_DEFAULT_NAN, 0); // fcvtzu x0, d1: NaN -> 0
    to_int(0x1e600020, 0, 0x4004_0000_0000_0000, 2); // fcvtns w0, d1: 2.5 -> 2
    to_int(0x1e600020, 0, 0x400c_0000_0000_0000, 4); // fcvtns w0, d1: 3.5 -> 4
    to_int(0x9e640020, 0, 0xc004_0000_0000_0000, (-3i64) as u64); // fcvtas x0, d1: -2.5 -> -3
    to_int(0x1e250020, 0, 0x4020_0000, 3); // fcvtau w0, s1: 2.5 -> 3
    to_int(0x1e280020, 0, 0x3f8c_cccd, 2); // fcvtps w0, s1: 1.1 -> 2
    to_int(0x9e680020, 0, 0xbff1_9999_9999_999a, u64::MAX); // fcvtps x0, d1: -1.1 -> -1
    to_int(0x9e700020, 0, 0xbff1_9999_9999_999a, (-2i64) as u64); // fcvtms x0, d1: -1.1 -> -2
    to_int(0x1e310020, 0, 0x3ff3_3333, 1); // fcvtmu w0, s1: 1.9 -> 1
    to_int(0x9e610020, 0, 0x7ff0_0000_0000_0000, u64::MAX); // fcvtnu x0, d1: +inf saturates
    to_int(0x1e780020, 0, 0xfff0_0000_0000_0000, 0x8000_0000); // fcvtzs w0, d1: -inf -> INT32_MIN
    to_int(0x1e780020, 0, 0x41e0_0000_0000_0000, 0x7fff_ffff); // fcvtzs w0, d1: 2^31 saturates
    to_int(0x1e18f020, 0, 0x3fe0_0000, 28); // fcvtzs w0, s1, #4: 1.75 * 16
    to_int(0x9e590020, 0, 0x3fe0_0000_0000_0000, 1 << 63); // fcvtzu x0, d1, #64: 0.5 * 2^64
}

#[test]
fn from_integer() {
    from_int(0x1e220020, 0, 0x1234_5678_ffff_ffff, 0xbf80_0000); // scvtf s0, w1: -1 (upper bits ignored)
    from_int(0x9e630020, 0, u64::MAX, 0x43f0_0000_0000_0000); // ucvtf d0, x1: 2^64 - 1 rounds to 2^64
    from_int(0x9e220020, 0, (1 << 24) + 1, 0x4b80_0000); // scvtf s0, x1: 2^24 + 1, nearest
    from_int(0x9e220020, RP, (1 << 24) + 1, 0x4b80_0001); // ... toward +inf
    from_int(0x9e42e020, 0, 256, D1); // scvtf d0, x1, #8: 256 / 2^8
    from_int(0x1e038020, 0, 0x8000_0000, 0x3f00_0000); // ucvtf s0, w1, #32: 2^31 / 2^32
    from_int(0x1e220020, 0, 0, 0); // scvtf s0, w1: 0 -> +0
}

#[test]
fn estimates_and_steps() {
    // FRECPE, worked from FPRecipEstimate: 1.0 scales to 256, the estimate
    // is (2^19 / 513 + 1) / 2 = 511, giving 0.998046875.
    check(0x5ea1d820, 0, &[S1], 0x3f7f_8000); // frecpe s
    check(0x5ee1d820, 0, &[D1], 0x3fef_f000_0000_0000); // frecpe d
                                                        // 3.0 scales to 384: (2^19 / 769 + 1) / 2 = 341, low byte 0x55, exponent 125.
    check(0x5ea1d820, 0, &[0x4040_0000], 0x3eaa_8000);
    check(0x5ea1d820, 0, &[0], 0x7f80_0000); // frecpe s: 1/+0 = +inf
    check(0x5ea1d820, 0, &[0xff80_0000], 0x8000_0000); // frecpe s: 1/-inf = -0
                                                       // FRSQRTE, worked from FPRSqrtEstimate: 1.0 (odd biased exponent) scales
                                                       // to 128 -> 511; 2.0 scales to 256 -> 361 (low byte 0x69); 4.0 -> 511.
    check(0x7ea1d820, 0, &[S1], 0x3f7f_8000); // frsqrte s: 1.0
    check(0x7ea1d820, 0, &[S2], 0x3f34_8000); // frsqrte s: 2.0
    check(0x7ea1d820, 0, &[0x4080_0000], 0x3eff_8000); // frsqrte s: 4.0
    check(0x7ee1d820, 0, &[0xbff0_0000_0000_0000], D_DEFAULT_NAN); // frsqrte d: negative
    check(
        0x7ee1d820,
        0,
        &[0x8000_0000_0000_0000],
        0xfff0_0000_0000_0000,
    ); // frsqrte d: -0 -> -inf
    check(0x7ea1d820, 0, &[0x7f80_0000], 0); // frsqrte s: +inf -> +0
                                             // FRECPX: sign, inverted exponent, zero fraction.
    check(0x5ee1f820, 0, &[0x4008_0000_0000_0000], D1); // frecpx d: 3.0 -> 1.0
    check(0x5ea1f820, 0, &[0], 0x7f00_0000); // frecpx s: 0 -> max exponent - 1
                                             // FRECPS: 2 - a*b; FRSQRTS: (3 - a*b) / 2; inf * 0 gives 2.0 and 1.5.
    check(0x5e22fc20, 0, &[0x3fc0_0000, 0x3f00_0000], 0x3fa0_0000); // frecps s: 2 - 1.5*0.5
    check(0x5e22fc20, 0, &[0x7f80_0000, 0], S2);
    check(0x5ee2fc20, 0, &[D1, D1], D1); // frsqrts d: (3 - 1) / 2
    check(
        0x5ee2fc20,
        0,
        &[0x7ff0_0000_0000_0000, 0],
        0x3ff8_0000_0000_0000,
    );
}

#[test]
fn unsupported_cases_are_reported() {
    assert_eq!(unsupported(0x1ee22820, 0), Unmodelled::HalfPrecision); // fadd h0, h1, h2
    assert_eq!(unsupported(0x1e23c020, 0), Unmodelled::HalfPrecision); // fcvt h0, s1
    assert_eq!(unsupported(0x1e622820, RP), Unmodelled::FpRoundingMode); // fadd d, FPCR round to +inf
}
