//! Arm floating-point semantics for scalar single and double precision.
//!
//! Follows the shared pseudocode of the Arm Architecture Reference Manual
//! (DDI 0487, `shared/functions/float`): FPUnpack, FPProcessNaN(s), FPRound,
//! FPToFixed, FixedToFP, FPRoundInt, FPConvert, FPRecipEstimate,
//! FPRSqrtEstimate, FPRecpX and the reciprocal step functions. Values travel
//! as raw bit patterns so results are bit-identical on every host.
//!
//! Arithmetic that needs IEEE rounding (add, subtract, multiply, divide,
//! square root, fused multiply-add) runs on the host in round-to-nearest-even,
//! which gives the Arm result for every finite, non-NaN case. NaN inputs are
//! processed before the host computes, host-generated NaNs are replaced by
//! the Arm default NaN, and flush-to-zero is applied on both sides.
//! Conversions and round-to-integral are done in integer arithmetic here, in
//! every rounding mode.
//!
//! Not modelled yet (see docs/ROADMAP.md): the FPSR cumulative exception
//! flags, half precision, the FEAT_AFP controls (FPCR.AH, FIZ, NEP), and
//! directed FPCR rounding modes for host-computed arithmetic, which report
//! an unsupported error instead of guessing.

/// Why an operation was not computed.
pub use crate::Unmodelled as Unsupported;

/// Scalar floating-point format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt {
    S,
    D,
}

impl Fmt {
    pub const fn width(self) -> u32 {
        match self {
            Fmt::S => 32,
            Fmt::D => 64,
        }
    }
    const fn ebits(self) -> u32 {
        match self {
            Fmt::S => 8,
            Fmt::D => 11,
        }
    }
    const fn fbits(self) -> u32 {
        match self {
            Fmt::S => 23,
            Fmt::D => 52,
        }
    }
    /// All bits of a value in this format.
    pub const fn mask(self) -> u64 {
        match self {
            Fmt::S => 0xffff_ffff,
            Fmt::D => u64::MAX,
        }
    }
    pub const fn sign(self) -> u64 {
        1 << (self.width() - 1)
    }
    const fn frac_mask(self) -> u64 {
        (1 << self.fbits()) - 1
    }
    const fn exp_max(self) -> u64 {
        (1 << self.ebits()) - 1
    }
    const fn quiet(self) -> u64 {
        1 << (self.fbits() - 1)
    }
    const fn bias(self) -> i32 {
        (1 << (self.ebits() - 1)) - 1
    }
    /// Bit pattern of the smallest positive normal number.
    const fn min_normal(self) -> u64 {
        1 << self.fbits()
    }
    const fn sign_if(self, neg: bool) -> u64 {
        if neg {
            self.sign()
        } else {
            0
        }
    }
    pub const fn default_nan(self) -> u64 {
        (self.exp_max() << self.fbits()) | self.quiet()
    }
    pub const fn inf(self, neg: bool) -> u64 {
        self.sign_if(neg) | (self.exp_max() << self.fbits())
    }
    pub const fn zero(self, neg: bool) -> u64 {
        self.sign_if(neg)
    }
    const fn max_normal(self, neg: bool) -> u64 {
        self.sign_if(neg) | ((self.exp_max() - 1) << self.fbits()) | self.frac_mask()
    }
    const fn exp_field(self, x: u64) -> u64 {
        (x >> self.fbits()) & self.exp_max()
    }
    /// The exact value as a host double (single precision widens exactly).
    fn value(self, x: u64) -> f64 {
        match self {
            Fmt::S => f32::from_bits(x as u32) as f64,
            Fmt::D => f64::from_bits(x),
        }
    }
    fn host_bits(self, v: f64) -> u64 {
        match self {
            Fmt::S => (v as f32).to_bits() as u64,
            Fmt::D => v.to_bits(),
        }
    }
}

/// Rounding modes of the pseudocode (FPRounding).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    TieEven,
    PosInf,
    NegInf,
    Zero,
    TieAway,
}

impl Rounding {
    /// The FPCR.RMode / instruction rmode encoding (0..=3).
    pub const fn from_rmode(rmode: u32) -> Rounding {
        match rmode & 3 {
            0 => Rounding::TieEven,
            1 => Rounding::PosInf,
            2 => Rounding::NegInf,
            _ => Rounding::Zero,
        }
    }
}

/// The FPCR controls this module honours.
#[derive(Clone, Copy, Debug)]
pub struct Fpcr {
    /// Default NaN (FPCR.DN, bit 25).
    pub dn: bool,
    /// Flush-to-zero (FPCR.FZ, bit 24).
    pub fz: bool,
    /// FPCR.RMode, bits 23..22.
    pub rmode: Rounding,
}

impl Fpcr {
    pub const fn from_bits(v: u64) -> Fpcr {
        Fpcr {
            dn: (v >> 25) & 1 != 0,
            fz: (v >> 24) & 1 != 0,
            rmode: Rounding::from_rmode((v >> 22) as u32),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Zero,
    Finite,
    Inf,
    QNaN,
    SNaN,
}

fn kind(x: u64, f: Fmt) -> Kind {
    let (e, m) = (f.exp_field(x), x & f.frac_mask());
    if e == f.exp_max() {
        if m == 0 {
            Kind::Inf
        } else if m & f.quiet() != 0 {
            Kind::QNaN
        } else {
            Kind::SNaN
        }
    } else if e == 0 && m == 0 {
        Kind::Zero
    } else {
        Kind::Finite
    }
}

fn is_nan(k: Kind) -> bool {
    matches!(k, Kind::QNaN | Kind::SNaN)
}

/// FPUnpack: classify, flushing subnormal inputs to signed zero under FZ.
fn unpack(x: u64, f: Fmt, fpcr: Fpcr) -> (Kind, u64) {
    let x = x & f.mask();
    if fpcr.fz && f.exp_field(x) == 0 {
        return (Kind::Zero, x & f.sign());
    }
    (kind(x, f), x)
}

/// FPProcessNaN: quieten, or the default NaN under FPCR.DN.
fn process_nan(x: u64, f: Fmt, fpcr: Fpcr) -> u64 {
    if fpcr.dn {
        f.default_nan()
    } else {
        x | f.quiet()
    }
}

/// FPProcessNaNs / FPProcessNaNs3: the first signalling NaN in operand
/// order wins, then the first quiet NaN.
fn process_nans(ops: &[(Kind, u64)], f: Fmt, fpcr: Fpcr) -> Option<u64> {
    let first = |want| ops.iter().find(|(k, _)| *k == want);
    first(Kind::SNaN)
        .or_else(|| first(Kind::QNaN))
        .map(|&(_, x)| process_nan(x, f, fpcr))
}

/// The host rounds to nearest-even; other FPCR modes are not modelled for
/// host-computed arithmetic yet.
fn host_rounding(fpcr: Fpcr) -> Result<(), Unsupported> {
    if fpcr.rmode == Rounding::TieEven {
        Ok(())
    } else {
        Err(Unsupported::FpRoundingMode)
    }
}

/// Finish a host result: generated NaNs become the default NaN, and under FZ
/// results whose exact value is below the smallest normal become signed zero.
/// Rounding can carry such a value up to exactly the smallest normal, so in
/// that one case `below_min` is asked whether the exact result was smaller.
fn finish(
    r: u64,
    f: Fmt,
    fpcr: Fpcr,
    below_min: impl FnOnce() -> Result<bool, Unsupported>,
) -> Result<u64, Unsupported> {
    let r = r & f.mask();
    if is_nan(kind(r, f)) {
        return Ok(f.default_nan());
    }
    if fpcr.fz {
        let mag = r & !f.sign();
        if (mag != 0 && mag < f.min_normal()) || (mag == f.min_normal() && below_min()?) {
            return Ok(r & f.sign());
        }
    }
    Ok(r)
}

/// Two-operand arithmetic computed on the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op2 {
    Add,
    Sub,
    Mul,
    Div,
}

/// FPAdd, FPSub, FPMul, FPDiv.
pub fn arith(op: Op2, a: u64, b: u64, f: Fmt, fpcr: Fpcr) -> Result<u64, Unsupported> {
    let (ka, a) = unpack(a, f, fpcr);
    let (kb, b) = unpack(b, f, fpcr);
    if let Some(n) = process_nans(&[(ka, a), (kb, b)], f, fpcr) {
        return Ok(n);
    }
    host_rounding(fpcr)?;
    let r = match f {
        Fmt::S => {
            let (x, y) = (f32::from_bits(a as u32), f32::from_bits(b as u32));
            let r = match op {
                Op2::Add => x + y,
                Op2::Sub => x - y,
                Op2::Mul => x * y,
                Op2::Div => x / y,
            };
            r.to_bits() as u64
        }
        Fmt::D => {
            let (x, y) = (f64::from_bits(a), f64::from_bits(b));
            let r = match op {
                Op2::Add => x + y,
                Op2::Sub => x - y,
                Op2::Mul => x * y,
                Op2::Div => x / y,
            };
            r.to_bits()
        }
    };
    finish(r, f, fpcr, || {
        let (x, y) = (f.value(a).abs(), f.value(b).abs());
        Ok(match (op, f) {
            // A sum this small is a multiple of the smallest subnormal and
            // fits the format, so it was exact.
            (Op2::Add | Op2::Sub, _) => false,
            // Single-precision products are exact in double precision, and
            // a quotient of 24-bit significands cannot round onto 2^-126.
            (Op2::Mul, Fmt::S) => x * y < f32::MIN_POSITIVE as f64,
            (Op2::Div, Fmt::S) => x / y < f32::MIN_POSITIVE as f64,
            // Sign of the exact residual. An underflowing residual keeps its
            // sign (it becomes -0.0), and an exact zero gives +0.0.
            (Op2::Mul, Fmt::D) => x.mul_add(y, -f64::MIN_POSITIVE).is_sign_negative(),
            (Op2::Div, Fmt::D) => (-f64::MIN_POSITIVE).mul_add(y, x).is_sign_negative(),
        })
    })
}

/// FPSqrt.
pub fn sqrt(a: u64, f: Fmt, fpcr: Fpcr) -> Result<u64, Unsupported> {
    let (ka, a) = unpack(a, f, fpcr);
    if is_nan(ka) {
        return Ok(process_nan(a, f, fpcr));
    }
    host_rounding(fpcr)?;
    let r = match f {
        Fmt::S => f32::from_bits(a as u32).sqrt().to_bits() as u64,
        Fmt::D => f64::from_bits(a).sqrt().to_bits(),
    };
    // Square roots never land near the smallest normal.
    finish(r, f, fpcr, || Ok(false))
}

/// FPMulAdd: `addend + n * m` with a single rounding.
pub fn mul_add(addend: u64, n: u64, m: u64, f: Fmt, fpcr: Fpcr) -> Result<u64, Unsupported> {
    let (ka, a) = unpack(addend, f, fpcr);
    let (kn, n) = unpack(n, f, fpcr);
    let (km, m) = unpack(m, f, fpcr);
    let inf_times_zero =
        (kn == Kind::Inf && km == Kind::Zero) || (kn == Kind::Zero && km == Kind::Inf);
    if let Some(r) = process_nans(&[(ka, a), (kn, n), (km, m)], f, fpcr) {
        // A quiet NaN addend with an invalid product gives the default NaN.
        return Ok(if ka == Kind::QNaN && inf_times_zero {
            f.default_nan()
        } else {
            r
        });
    }
    host_rounding(fpcr)?;
    let r = match f {
        Fmt::S => {
            let [a, n, m] = [a, n, m].map(|v| f32::from_bits(v as u32));
            n.mul_add(m, a).to_bits() as u64
        }
        Fmt::D => f64::from_bits(n)
            .mul_add(f64::from_bits(m), f64::from_bits(a))
            .to_bits(),
    };
    finish(r, f, fpcr, || {
        match f {
            Fmt::S => {
                // The product is exact in double precision; TwoSum then gives the
                // exact sum as s + err.
                let (p, c) = (f.value(n) * f.value(m), f.value(a));
                let s = p + c;
                let bb = s - p;
                let err = (p - (s - bb)) + (c - bb);
                let min = f32::MIN_POSITIVE as f64;
                Ok(s.abs() < min || (s.abs() == min && err != 0.0 && (err < 0.0) != (s < 0.0)))
            }
            Fmt::D => Err(Unsupported::FpFlushedFmaEdge),
        }
    })
}

/// FPMax, FPMin, and with `num` FPMaxNum, FPMinNum.
pub fn max_min(a: u64, b: u64, f: Fmt, fpcr: Fpcr, max: bool, num: bool) -> u64 {
    let (mut ka, mut a) = unpack(a, f, fpcr);
    let (mut kb, mut b) = unpack(b, f, fpcr);
    if num {
        // One quiet NaN loses against a number: replace it by the infinity
        // that the other operand always beats.
        let fill = f.inf(max);
        if ka == Kind::QNaN && kb != Kind::QNaN {
            (ka, a) = (Kind::Inf, fill);
        } else if ka != Kind::QNaN && kb == Kind::QNaN {
            (kb, b) = (Kind::Inf, fill);
        }
    }
    if let Some(n) = process_nans(&[(ka, a), (kb, b)], f, fpcr) {
        return n;
    }
    let (va, vb) = (f.value(a), f.value(b));
    let pick_a = if max { va > vb } else { va < vb };
    let (k, x) = if pick_a { (ka, a) } else { (kb, b) };
    if k == Kind::Zero {
        // max(+0, -0) = +0 and min(+0, -0) = -0 in either order.
        let (sa, sb) = (a & f.sign() != 0, b & f.sign() != 0);
        f.zero(if max { sa && sb } else { sa || sb })
    } else {
        x
    }
}

/// FPCompare, as NZCV in bits 31..28.
pub fn compare(a: u64, b: u64, f: Fmt, fpcr: Fpcr) -> u64 {
    let (ka, a) = unpack(a, f, fpcr);
    let (kb, b) = unpack(b, f, fpcr);
    let nzcv: u64 = if is_nan(ka) || is_nan(kb) {
        0b0011
    } else {
        let (va, vb) = (f.value(a), f.value(b));
        if va == vb {
            0b0110
        } else if va < vb {
            0b1000
        } else {
            0b0010
        }
    };
    nzcv << 28
}

/// A finite, non-zero value as (negative, significand, exponent), where the
/// value is significand * 2^exponent.
fn decompose(x: u64, f: Fmt) -> (bool, u64, i32) {
    let neg = x & f.sign() != 0;
    let e = f.exp_field(x) as i32;
    let m = x & f.frac_mask();
    let shift = f.bias() + f.fbits() as i32;
    if e == 0 {
        (neg, m, 1 - shift)
    } else {
        (neg, m | (1 << f.fbits()), e - shift)
    }
}

/// Where the discarded fraction lies, relative to one half.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Frac {
    Zero,
    Below,
    Half,
    Above,
}

/// mag * 2^exp split into its integer part (None if it does not fit 128 bits)
/// and the class of its fraction. `mag` is below 2^64.
fn split(mag: u128, exp: i32) -> (Option<u128>, Frac) {
    if exp >= 0 {
        if exp >= 128 || (mag.leading_zeros() as i32) < exp {
            return (None, Frac::Zero);
        }
        return (Some(mag << exp), Frac::Zero);
    }
    let sh = exp.unsigned_abs();
    if sh >= 128 {
        let frac = if mag == 0 { Frac::Zero } else { Frac::Below };
        return (Some(0), frac);
    }
    let rem = mag & ((1u128 << sh) - 1);
    let half = 1u128 << (sh - 1);
    let frac = match rem {
        0 => Frac::Zero,
        r if r < half => Frac::Below,
        r if r == half => Frac::Half,
        _ => Frac::Above,
    };
    (Some(mag >> sh), frac)
}

/// Round a magnitude to an integer: the pseudocode's round_up decision,
/// restated for magnitude and sign.
fn round_mag(int: u128, frac: Frac, neg: bool, rounding: Rounding) -> u128 {
    let up = match rounding {
        Rounding::TieEven => frac > Frac::Half || (frac == Frac::Half && int & 1 == 1),
        Rounding::TieAway => frac >= Frac::Half,
        Rounding::Zero => false,
        Rounding::PosInf => frac != Frac::Zero && !neg,
        Rounding::NegInf => frac != Frac::Zero && neg,
    };
    int + up as u128
}

/// FPRound: round (-1)^neg * mant * 2^exp, with mant != 0, to format `f`.
fn round(neg: bool, mant: u128, exp: i32, f: Fmt, rounding: Rounding, fz: bool) -> u64 {
    let fb = f.fbits() as i32;
    // Unbiased exponent e: the value lies in [2^e, 2^(e+1)).
    let e = exp + 127 - mant.leading_zeros() as i32;
    let min_exp = 1 - f.bias();
    if fz && e < min_exp {
        return f.zero(neg);
    }
    let mut biased = (e - min_exp + 1).max(0);
    // Weight of the last significand bit: fixed for subnormals.
    let quantum = if biased == 0 { min_exp - fb } else { e - fb };
    let (int, frac) = split(mant, exp - quantum);
    let mut int = round_mag(int.expect("significand fits"), frac, neg, rounding);
    if int == 1 << (fb + 1) {
        int >>= 1;
        biased += 1;
    } else if biased == 0 && int == 1 << fb {
        biased = 1;
    }
    if biased as u64 >= f.exp_max() {
        let to_inf = match rounding {
            Rounding::TieEven | Rounding::TieAway => true,
            Rounding::PosInf => !neg,
            Rounding::NegInf => neg,
            Rounding::Zero => false,
        };
        return if to_inf {
            f.inf(neg)
        } else {
            f.max_normal(neg)
        };
    }
    f.sign_if(neg) | ((biased as u64) << f.fbits()) | (int as u64 & f.frac_mask())
}

const fn int_mask(width: u32) -> u64 {
    if width == 64 {
        u64::MAX
    } else {
        (1 << width) - 1
    }
}

/// FPToFixed: convert to a `width`-bit integer with `fbits` fraction bits,
/// saturating. NaNs convert to zero.
pub fn to_fixed(
    x: u64,
    f: Fmt,
    fpcr: Fpcr,
    fbits: u32,
    unsigned: bool,
    width: u32,
    rounding: Rounding,
) -> u64 {
    let (k, x) = unpack(x, f, fpcr);
    let neg = x & f.sign() != 0;
    let mag = match k {
        Kind::Zero | Kind::QNaN | Kind::SNaN => Some(0),
        Kind::Inf => None,
        Kind::Finite => {
            let (_, m, e) = decompose(x, f);
            let (int, frac) = split(m as u128, e + fbits as i32);
            int.map(|i| round_mag(i, frac, neg, rounding))
        }
    };
    let top = 1u128 << (width - 1);
    let (max_pos, max_neg) = if unsigned {
        ((top << 1) - 1, 0)
    } else {
        (top - 1, top)
    };
    let v: i128 = match (mag, neg) {
        (None, false) => max_pos as i128,
        (None, true) => -(max_neg as i128),
        (Some(m), false) => m.min(max_pos) as i128,
        (Some(m), true) => -(m.min(max_neg) as i128),
    };
    v as u64 & int_mask(width)
}

/// FixedToFP: `v` is a `width`-bit integer with `fbits` fraction bits.
pub fn from_fixed(v: u64, width: u32, unsigned: bool, fbits: u32, f: Fmt, fpcr: Fpcr) -> u64 {
    let v = v & int_mask(width);
    let (neg, mag) = if unsigned {
        (false, v)
    } else {
        let s = if width == 32 {
            v as u32 as i32 as i64
        } else {
            v as i64
        };
        (s < 0, s.unsigned_abs())
    };
    if mag == 0 {
        return f.zero(false);
    }
    round(neg, mag as u128, -(fbits as i32), f, fpcr.rmode, fpcr.fz)
}

/// FPRoundInt: round to an integral value in format `f`.
pub fn round_int(x: u64, f: Fmt, fpcr: Fpcr, rounding: Rounding) -> u64 {
    let (k, x) = unpack(x, f, fpcr);
    match k {
        Kind::QNaN | Kind::SNaN => return process_nan(x, f, fpcr),
        Kind::Zero | Kind::Inf => return x,
        Kind::Finite => {}
    }
    let (neg, m, e) = decompose(x, f);
    if e >= 0 {
        return x;
    }
    let (int, frac) = split(m as u128, e);
    let int = round_mag(int.expect("small"), frac, neg, rounding);
    if int == 0 {
        f.zero(neg)
    } else {
        round(neg, int, 0, f, Rounding::Zero, false)
    }
}

/// FPConvert between single and double precision.
pub fn convert(x: u64, from: Fmt, to: Fmt, fpcr: Fpcr) -> u64 {
    let (k, x) = unpack(x, from, fpcr);
    let neg = x & from.sign() != 0;
    match k {
        Kind::QNaN | Kind::SNaN if fpcr.dn => to.default_nan(),
        Kind::QNaN | Kind::SNaN => {
            // FPConvertNaN: keep the sign and the top fraction bits below the
            // quiet bit, and set the quiet bit.
            let payload = x & (from.quiet() - 1);
            let payload = if to.fbits() > from.fbits() {
                payload << (to.fbits() - from.fbits())
            } else {
                payload >> (from.fbits() - to.fbits())
            };
            to.inf(neg) | to.quiet() | payload
        }
        Kind::Inf => to.inf(neg),
        Kind::Zero => to.zero(neg),
        Kind::Finite => {
            let (_, m, e) = decompose(x, from);
            round(neg, m as u128, e, to, fpcr.rmode, fpcr.fz)
        }
    }
}

/// VFPExpandImm: the 8-bit immediate of FMOV (scalar, immediate).
pub fn expand_imm(imm8: u32, f: Fmt) -> u64 {
    let imm8 = imm8 as u64;
    let e = f.ebits();
    let b6 = (imm8 >> 6) & 1;
    let rep = if b6 == 1 { (1 << (e - 3)) - 1 } else { 0 };
    let exp = ((b6 ^ 1) << (e - 1)) | (rep << 2) | ((imm8 >> 4) & 3);
    ((imm8 >> 7) << (f.width() - 1)) | (exp << f.fbits()) | ((imm8 & 0xf) << (f.fbits() - 4))
}

/// 2^e as a double, exactly (subnormal results included).
fn pow2(e: i32) -> f64 {
    if e >= -1022 {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else {
        f64::from_bits(1 << (e + 1074))
    }
}

/// Exponent field and fraction widened to 52 bits, as the estimate
/// pseudocode lays them out.
fn estimate_fields(x: u64, f: Fmt) -> (u64, i32) {
    let frac = (x & f.frac_mask()) << (52 - f.fbits());
    (frac, f.exp_field(x) as i32)
}

const FRAC52: u64 = (1 << 52) - 1;

/// RecipEstimate: `a` in 256..512 represents [0.5, 1.0).
fn recip_estimate_int(a: u32) -> u32 {
    let a = a * 2 + 1;
    let b = (1 << 19) / a;
    // (b + 1) DIV 2: round to nearest.
    b.div_ceil(2)
}

/// RecipSqrtEstimate: `a` in 128..512 represents [0.25, 1.0).
fn rsqrt_estimate_int(a: u32) -> u32 {
    let a = if a < 256 {
        a * 2 + 1
    } else {
        (((a >> 1) << 1) + 1) * 2
    } as u64;
    let mut b = 512u64;
    while a * (b + 1) * (b + 1) < 1 << 28 {
        b += 1;
    }
    // (b + 1) DIV 2: round to nearest.
    b.div_ceil(2) as u32
}

/// FPRecipEstimate (FRECPE).
pub fn recip_estimate(x: u64, f: Fmt, fpcr: Fpcr) -> u64 {
    let (k, _) = unpack(x, f, fpcr);
    let x = x & f.mask();
    let neg = x & f.sign() != 0;
    match k {
        Kind::QNaN | Kind::SNaN => return process_nan(x, f, fpcr),
        Kind::Inf => return f.zero(neg),
        Kind::Zero => return f.inf(neg),
        Kind::Finite => {}
    }
    let v = f.value(x).abs();
    let (tiny, huge) = match f {
        Fmt::S => (pow2(-128), pow2(126)),
        Fmt::D => (pow2(-1024), pow2(1022)),
    };
    if v < tiny {
        let to_inf = match fpcr.rmode {
            Rounding::TieEven | Rounding::TieAway => true,
            Rounding::PosInf => !neg,
            Rounding::NegInf => neg,
            Rounding::Zero => false,
        };
        return if to_inf {
            f.inf(neg)
        } else {
            f.max_normal(neg)
        };
    }
    if fpcr.fz && v >= huge {
        return f.zero(neg);
    }
    let (mut frac, mut exp) = estimate_fields(x, f);
    if exp == 0 {
        if (frac >> 51) & 1 == 0 {
            exp = -1;
            frac = (frac << 2) & FRAC52;
        } else {
            frac = (frac << 1) & FRAC52;
        }
    }
    let scaled = 256 | (frac >> 44) as u32;
    let mut result_exp = match f {
        Fmt::S => 253 - exp,
        Fmt::D => 2045 - exp,
    };
    let estimate = recip_estimate_int(scaled);
    let mut frac = ((estimate & 0xff) as u64) << 44;
    if result_exp == 0 {
        frac = (1 << 51) | (frac >> 1);
    } else if result_exp == -1 {
        frac = (1 << 50) | (frac >> 2);
        result_exp = 0;
    }
    f.sign_if(neg) | ((result_exp as u64) << f.fbits()) | (frac >> (52 - f.fbits()))
}

/// FPRSqrtEstimate (FRSQRTE).
pub fn rsqrt_estimate(x: u64, f: Fmt, fpcr: Fpcr) -> u64 {
    let (k, _) = unpack(x, f, fpcr);
    let x = x & f.mask();
    let neg = x & f.sign() != 0;
    match k {
        Kind::QNaN | Kind::SNaN => return process_nan(x, f, fpcr),
        Kind::Zero => return f.inf(neg),
        _ if neg => return f.default_nan(),
        Kind::Inf => return f.zero(false),
        Kind::Finite => {}
    }
    let (mut frac, mut exp) = estimate_fields(x, f);
    if exp == 0 {
        while (frac >> 51) & 1 == 0 {
            frac = (frac << 1) & FRAC52;
            exp -= 1;
        }
        frac = (frac << 1) & FRAC52;
    }
    // Scale into [0.25, 1.0) keeping the exponent's parity.
    let scaled = if exp & 1 == 0 {
        256 | (frac >> 44) as u32
    } else {
        128 | (frac >> 45) as u32
    };
    let result_exp = match f {
        Fmt::S => (380 - exp) / 2,
        Fmt::D => (3068 - exp) / 2,
    };
    let estimate = (rsqrt_estimate_int(scaled) & 0xff) as u64;
    ((result_exp as u64) << f.fbits()) | (estimate << (f.fbits() - 8))
}

/// FPRecpX (FRECPX): reciprocal exponent.
pub fn recpx(x: u64, f: Fmt, fpcr: Fpcr) -> u64 {
    let (k, _) = unpack(x, f, fpcr);
    let x = x & f.mask();
    if is_nan(k) {
        return process_nan(x, f, fpcr);
    }
    let e = f.exp_field(x);
    let exp = if e == 0 {
        f.exp_max() - 1
    } else {
        !e & f.exp_max()
    };
    (x & f.sign()) | (exp << f.fbits())
}

/// FPRecipStepFused (FRECPS, 2 - a*b) or FPRSqrtStepFused (FRSQRTS,
/// (3 - a*b) / 2), each rounded once.
pub fn recip_step(a: u64, b: u64, f: Fmt, fpcr: Fpcr, rsqrt: bool) -> Result<u64, Unsupported> {
    // The pseudocode negates the first operand before anything else.
    let (ka, a) = unpack((a & f.mask()) ^ f.sign(), f, fpcr);
    let (kb, b) = unpack(b, f, fpcr);
    if let Some(n) = process_nans(&[(ka, a), (kb, b)], f, fpcr) {
        return Ok(n);
    }
    if (ka == Kind::Inf && kb == Kind::Zero) || (ka == Kind::Zero && kb == Kind::Inf) {
        return Ok(f.host_bits(if rsqrt { 1.5 } else { 2.0 }));
    }
    if ka == Kind::Inf || kb == Kind::Inf {
        return Ok(f.inf((a ^ b) & f.sign() != 0));
    }
    host_rounding(fpcr)?;
    let r = match f {
        Fmt::S => {
            let (x, y) = (f32::from_bits(a as u32), f32::from_bits(b as u32));
            let r = if rsqrt {
                x.mul_add(y, 3.0) / 2.0
            } else {
                x.mul_add(y, 2.0)
            };
            r.to_bits() as u64
        }
        Fmt::D => {
            let (x, y) = (f64::from_bits(a), f64::from_bits(b));
            let r = if rsqrt {
                x.mul_add(y, 3.0) / 2.0
            } else {
                x.mul_add(y, 2.0)
            };
            r.to_bits()
        }
    };
    // 2 - a*b and 3 - a*b are never tiny: a*b has no bits below about 2^-104
    // when it is close to 2 or 3.
    finish(r, f, fpcr, || Ok(false))
}
