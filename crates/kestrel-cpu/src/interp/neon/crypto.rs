//! The cryptographic extension (FEAT_AES, FEAT_SHA1, FEAT_SHA256): AESE,
//! AESD, AESMC, AESIMC, the SHA-1 and SHA-256 hash and schedule
//! instructions. The 64-bit PMULL is in `int.rs` with the other
//! three-different forms.
//!
//! Written from the Arm ARM pseudocode (`shared/functions/crypto`: AESSubBytes,
//! AESShiftRows, AESMixColumns and their inverses, SHAchoose, SHAparity,
//! SHAmajority, SHA256hash) and FIPS 197 for the AES S-box, which is
//! computed here from its definition (the inverse in GF(2^8), then the
//! affine transformation) rather than copied as a table.

use super::super::bits::*;
use super::super::{Ctx, Flow, Step};
use super::*;

/// Multiply in GF(2^8) modulo the AES polynomial x^8 + x^4 + x^3 + x + 1.
const fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 };
        b >>= 1;
    }
    r
}

/// The AES S-box (FIPS 197, 5.1.1) and its inverse.
const SBOX: [[u8; 256]; 2] = {
    let mut fwd = [0u8; 256];
    let mut inv = [0u8; 256];
    let mut x = 0;
    while x < 256 {
        // The multiplicative inverse (0 maps to 0): x^254.
        let mut p = 1u8;
        let mut k = 0;
        while k < 254 {
            p = gf_mul(p, x as u8);
            k += 1;
        }
        let b = if x == 0 { 0 } else { p };
        let s =
            b ^ b.rotate_left(1) ^ b.rotate_left(2) ^ b.rotate_left(3) ^ b.rotate_left(4) ^ 0x63;
        fwd[x] = s;
        inv[s as usize] = x as u8;
        x += 1;
    }
    [fwd, inv]
};

fn bytes(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

/// AESShiftRows and AESInvShiftRows. Byte i of the register is row i % 4,
/// column i / 4 of the state; row r rotates left (or right) by r columns.
fn shift_rows(v: u128, inverse: bool) -> u128 {
    let b = bytes(v);
    let out: [u8; 16] = std::array::from_fn(|i| {
        let r = i % 4;
        let src = if inverse { i + 16 - 4 * r } else { i + 4 * r };
        b[src % 16]
    });
    u128::from_le_bytes(out)
}

fn sub_bytes(v: u128, inverse: bool) -> u128 {
    u128::from_le_bytes(bytes(v).map(|x| SBOX[inverse as usize][x as usize]))
}

/// AESMixColumns and AESInvMixColumns: each column times a fixed matrix.
fn mix_columns(v: u128, inverse: bool) -> u128 {
    let row: [u8; 4] = if inverse {
        [14, 11, 13, 9]
    } else {
        [2, 3, 1, 1]
    };
    let b = bytes(v);
    let out: [u8; 16] = std::array::from_fn(|i| {
        let (r, col) = (i % 4, i - i % 4);
        (0..4).fold(0, |acc, k| acc ^ gf_mul(row[(k + 4 - r) % 4], b[col + k]))
    });
    u128::from_le_bytes(out)
}

/// AESE, AESD, AESMC, AESIMC (bits 16..12 = 001xx, size 00).
pub fn aes(c: &mut Ctx) -> Step {
    let i = c.insn;
    // Q = 1, U = 0, size = 00.
    if !bit(i, 30) || bit(i, 29) || field(i, 23, 22) != 0 || field(i, 16, 14) != 0b001 {
        return Err(c.undef());
    }
    let (n, d) = (vreg(c, rn(i)), vreg(c, rd(i)));
    let r = match field(i, 13, 12) {
        0b00 => sub_bytes(shift_rows(d ^ n, false), false), // AESE
        0b01 => sub_bytes(shift_rows(d ^ n, true), true),   // AESD
        0b10 => mix_columns(n, false),                      // AESMC
        _ => mix_columns(n, true),                          // AESIMC
    };
    write(c, rd(i), r, 128);
    Ok(Flow::Next)
}

fn word(v: u128, e: u32) -> u32 {
    (v >> (32 * e)) as u32
}

fn with_word(v: u128, e: u32, x: u32) -> u128 {
    let sh = 32 * e;
    (v & !(0xffff_ffffu128 << sh)) | ((x as u128) << sh)
}

fn choose(x: u32, y: u32, z: u32) -> u32 {
    ((y ^ z) & x) ^ z
}

fn parity(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

fn majority(x: u32, y: u32, z: u32) -> u32 {
    (x & y) | ((x | y) & z)
}

/// SHA1C, SHA1P, SHA1M: four rounds on the hash state X (Qd) and E (Sn).
fn sha1_rounds(mut x: u128, mut y: u32, w: u128, f: fn(u32, u32, u32) -> u32) -> u128 {
    for e in 0..4 {
        let t = f(word(x, 1), word(x, 2), word(x, 3));
        y = y
            .wrapping_add(word(x, 0).rotate_left(5))
            .wrapping_add(t)
            .wrapping_add(word(w, e));
        x = with_word(x, 1, word(x, 1).rotate_left(30));
        // <Y, X> = ROL(Y : X, 32)
        let top = word(x, 3);
        x = (x << 32) | y as u128;
        y = top;
    }
    x
}

/// SHA256H (part 1) and SHA256H2: four rounds; returns X or Y.
fn sha256_hash(mut x: u128, mut y: u128, w: u128, part1: bool) -> u128 {
    let sigma0 = |v: u32| v.rotate_right(2) ^ v.rotate_right(13) ^ v.rotate_right(22);
    let sigma1 = |v: u32| v.rotate_right(6) ^ v.rotate_right(11) ^ v.rotate_right(25);
    for e in 0..4 {
        let chs = choose(word(y, 0), word(y, 1), word(y, 2));
        let maj = majority(word(x, 0), word(x, 1), word(x, 2));
        let t = word(y, 3)
            .wrapping_add(sigma1(word(y, 0)))
            .wrapping_add(chs)
            .wrapping_add(word(w, e));
        x = with_word(x, 3, t.wrapping_add(word(x, 3)));
        y = with_word(y, 3, t.wrapping_add(sigma0(word(x, 0))).wrapping_add(maj));
        // <Y, X> = ROL(Y : X, 32)
        let (xt, yt) = (word(x, 3), word(y, 3));
        x = (x << 32) | yt as u128;
        y = (y << 32) | xt as u128;
    }
    if part1 {
        x
    } else {
        y
    }
}

/// Cryptographic three-register SHA (scalar encoding space, size 00).
pub fn sha_three(c: &mut Ctx) -> Step {
    let i = c.insn;
    if bit(i, 29) {
        return Err(c.undef());
    }
    let (n, m, d) = (vreg(c, rn(i)), vreg(c, rm(i)), vreg(c, rd(i)));
    let r = match field(i, 14, 12) {
        0b000 => sha1_rounds(d, word(n, 0), m, choose), // SHA1C
        0b001 => sha1_rounds(d, word(n, 0), m, parity), // SHA1P
        0b010 => sha1_rounds(d, word(n, 0), m, majority), // SHA1M
        0b011 => ((n << 64) | (d >> 64)) ^ d ^ m,       // SHA1SU0
        0b100 => sha256_hash(d, n, m, true),            // SHA256H
        0b101 => sha256_hash(n, d, m, false),           // SHA256H2
        0b110 => {
            // SHA256SU1
            let t0 = (m << 96) | (n >> 32);
            let sig = |v: u32| v.rotate_right(17) ^ v.rotate_right(19) ^ (v >> 10);
            let mut r = 0u128;
            for e in 0..4 {
                // The upper two words use the two just computed.
                let src = if e < 2 {
                    word(m, e + 2)
                } else {
                    word(r, e - 2)
                };
                let x = sig(src).wrapping_add(word(d, e)).wrapping_add(word(t0, e));
                r = with_word(r, e, x);
            }
            r
        }
        _ => return Err(c.undef()),
    };
    write(c, rd(i), r, 128);
    Ok(Flow::Next)
}

/// Cryptographic two-register SHA (scalar encoding space, size 00):
/// SHA1H, SHA1SU1, SHA256SU0.
pub fn sha_two(c: &mut Ctx) -> Step {
    let i = c.insn;
    if bit(i, 29) || field(i, 23, 22) != 0 {
        return Err(c.undef());
    }
    let (n, d) = (vreg(c, rn(i)), vreg(c, rd(i)));
    let r = match field(i, 16, 12) {
        0b00000 => word(n, 0).rotate_left(30) as u128, // SHA1H
        0b00001 => {
            // SHA1SU1
            let t = d ^ (n >> 32);
            let r = (0..4).fold(0, |acc, e| with_word(acc, e, word(t, e).rotate_left(1)));
            with_word(r, 3, word(r, 3) ^ word(t, 0).rotate_left(2))
        }
        0b00010 => {
            // SHA256SU0
            let t = (n << 96) | (d >> 32);
            (0..4).fold(0, |acc, e| {
                let x = word(t, e);
                let s = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
                with_word(acc, e, s.wrapping_add(word(d, e)))
            })
        }
        _ => return Err(c.undef()),
    };
    write(c, rd(i), r, 128);
    Ok(Flow::Next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 197, 5.1.1 and Figure 7: S(0x53) = 0xed, and the corners.
    #[test]
    fn the_sbox_matches_fips_197() {
        assert_eq!(SBOX[0][0x00], 0x63);
        assert_eq!(SBOX[0][0x53], 0xed);
        assert_eq!(SBOX[0][0xff], 0x16);
        assert_eq!(SBOX[1][0x63], 0x00);
        assert_eq!(SBOX[1][0xed], 0x53);
    }
}
