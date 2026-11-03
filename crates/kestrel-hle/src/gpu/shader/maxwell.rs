//! Decoding Maxwell (GM20B, SM 5.3) instructions into kestrel's shader IR.
//!
//! Encodings from envytools' disassembler tables (`envydis/gm107.c`, MIT):
//! each entry below names its table line's opcode bits and mask, and the
//! fields it reads. Meanings from Mesa's nouveau code generator
//! (`nv50_ir_emit_gm107.cpp`, MIT) where the table only names a field.
//!
//! kestrel translates straight-line programs: decoding stops at the first
//! unconditional EXIT. A predicated instruction, a branch before it, an
//! instruction or a field value not listed here stop, named with the
//! instruction's bits (kestrel's choice: an unknown instruction is never
//! guessed at).

use super::ir::{FSrc, Interp, Mufu, Op, Operand, Program, Round, RZ};
use super::{Header, Stage, HEADER_BYTES};

fn bits(w: u64, lo: u32, n: u32) -> u32 {
    ((w >> lo) & ((1u64 << n) - 1)) as u32
}

fn bit(w: u64, n: u32) -> bool {
    w >> n & 1 == 1
}

fn reg(w: u64, lo: u32) -> u8 {
    bits(w, lo, 8) as u8
}

/// A float immediate (F20_20): bits 38:20 and the sign at 56 are the top
/// 20 bits of the float.
fn f20(w: u64) -> u32 {
    (bits(w, 20, 19) | (bits(w, 56, 1) << 19)) << 12
}

/// A signed integer immediate (S20_20): bits 38:20, the sign at 56.
fn s20(w: u64) -> u32 {
    let v = bits(w, 20, 19) | (bits(w, 56, 1) << 19);
    ((v << 12) as i32 >> 12) as u32
}

/// A constant buffer operand (C34_RZ_O14_20): the buffer in bits 38:34,
/// the word offset in 33:20 (unsigned, as nouveau's emitter writes it).
fn cbuf34(w: u64) -> Operand {
    Operand::Cbuf {
        index: bits(w, 34, 5) as u8,
        offset: (bits(w, 20, 14) * 4) as i32,
        reg: RZ,
    }
}

/// The operand forms of the ALU instructions: the top nibble of the
/// opcode says register (0x5c..), constant buffer (0x4c..) or immediate
/// (0x38..).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Reg,
    Cbuf,
    Imm,
}

fn form(w: u64) -> Form {
    match w >> 56 {
        0x5c => Form::Reg,
        0x4c => Form::Cbuf,
        _ => Form::Imm,
    }
}

/// The second operand of an ALU instruction, by form; `float` picks
/// F20_20 over S20_20 for immediates.
fn operand_b(w: u64, float: bool) -> Operand {
    match form(w) {
        Form::Reg => Operand::Reg(reg(w, 20)),
        Form::Cbuf => cbuf34(w),
        Form::Imm => Operand::Imm(if float { f20(w) } else { s20(w) }),
    }
}

/// Decode a program: `words` are the 32-bit words from its start (the
/// header, then the instructions).
pub fn decode(words: &[u32]) -> Result<Program, String> {
    let header = Header::parse(words)?;
    let code = &words[HEADER_BYTES / 4..];
    let mut ops = Vec::new();
    let mut at = 0;
    loop {
        let i = at / 2;
        if (i + 1) * 2 > code.len() {
            return Err("a shader program that runs past its end (no EXIT)".into());
        }
        let w = code[at] as u64 | (code[at + 1] as u64) << 32;
        at += 2;
        // Every fourth word is scheduling information.
        if i % 4 == 0 {
            continue;
        }
        let addr = HEADER_BYTES + i * 8;
        let named = |what: &str| format!("shader instruction {w:#018x} at {addr:#x}: {what}");
        // The predicate (bits 19:16): 7 is PT, always.
        if bits(w, 16, 4) != 7 {
            return Err(named("a predicated instruction"));
        }
        match decode_one(w, header.stage).map_err(|e| named(&e))? {
            Some(Op::Exit) => {
                ops.push(Op::Exit);
                return Ok(Program { header, ops });
            }
            Some(op) => ops.push(op),
            None => {}
        }
    }
}

/// One instruction: an operation, None for one with no effect (NOP), or
/// the reason it is not translated.
fn decode_one(w: u64, stage: Stage) -> Result<Option<Op>, String> {
    let top = w >> 48;
    let dst = reg(w, 0);
    // The condition code write (CC, bit 47) of the ALU instructions:
    // nothing kestrel decodes reads it, so it must be off.
    let no_cc = || {
        if bit(w, 47) {
            Err("a condition code write".to_string())
        } else {
            Ok(())
        }
    };
    // A float rounding mode other than to-nearest (bits 40:39).
    let round_rn = |lo: u32| {
        if bits(w, lo, 2) != 0 {
            Err(format!("rounding mode {}", bits(w, lo, 2)))
        } else {
            Ok(())
        }
    };
    // EXIT (e300, mask fff0): the condition (4:0) must be T (0xf).
    if top & 0xfff0 == 0xe300 {
        if bits(w, 0, 5) != 0xf {
            return Err(format!("EXIT on condition {}", bits(w, 0, 5)));
        }
        return Ok(Some(Op::Exit));
    }
    // BRA (e240, mask fff0): control flow is not translated yet.
    if top & 0xfff0 == 0xe240 {
        return Err("a branch".into());
    }
    let op = match top & 0xfff8 {
        // NOP (50b0): nothing to do.
        0x50b0 => return Ok(None),
        // MOV (5c98, 4c98, 38 98): REG_00, the operand, the lane mask
        // (U04_39) all four lanes.
        0x5c98 | 0x4c98 | 0x3898 => {
            if bits(w, 39, 4) != 0xf {
                return Err(format!("a lane mask of {:#x}", bits(w, 39, 4)));
            }
            Op::Mov {
                dst,
                src: operand_b(w, false),
            }
        }
        // FADD (5c58, 4c58, 3858): a REG_08 (neg 48, abs 46), b (neg 45,
        // abs 49), sat 50, ftz 44 (denormals as the host's: ignored),
        // rounding 40:39.
        0x5c58 | 0x4c58 | 0x3858 => {
            no_cc()?;
            round_rn(39)?;
            Op::FAdd {
                dst,
                a: FSrc {
                    op: Operand::Reg(reg(w, 8)),
                    neg: bit(w, 48),
                    abs: bit(w, 46),
                },
                b: FSrc {
                    op: operand_b(w, true),
                    neg: bit(w, 45),
                    abs: bit(w, 49),
                },
                sat: bit(w, 50),
            }
        }
        // FMUL (5c68, 4c68, 3868): a REG_08, b (neg 48), the scale (43:41:
        // d2 d4 d8 m8 m4 m2), sat 50, ftz/fmz 45:44, rounding 40:39.
        0x5c68 | 0x4c68 | 0x3868 => {
            no_cc()?;
            round_rn(39)?;
            let scale = match bits(w, 41, 3) {
                0 => 0,
                1 => -1,
                2 => -2,
                3 => -3,
                4 => 3,
                5 => 2,
                6 => 1,
                s => return Err(format!("FMUL scale {s}")),
            };
            Op::FMul {
                dst,
                a: FSrc::plain(Operand::Reg(reg(w, 8))),
                b: FSrc {
                    op: operand_b(w, true),
                    neg: bit(w, 48),
                    abs: false,
                },
                scale,
                sat: bit(w, 50),
            }
        }
        // MUFU (5080): the function (23:20), REG_08 (neg 48, abs 46), sat
        // 50. Sine and cosine need RRO's range reduction first: not
        // translated yet.
        0x5080 => {
            let func = match bits(w, 20, 4) {
                2 => Mufu::Ex2,
                3 => Mufu::Lg2,
                4 => Mufu::Rcp,
                5 => Mufu::Rsq,
                8 => Mufu::Sqrt,
                f => return Err(format!("MUFU function {f}")),
            };
            Op::Mufu {
                dst,
                func,
                src: FSrc {
                    op: Operand::Reg(reg(w, 8)),
                    neg: bit(w, 48),
                    abs: bit(w, 46),
                },
                sat: bit(w, 50),
            }
        }
        // SHL (5c48, 4c48, 3848): REG_08 << b; W (39) wraps the shift, X
        // (43) is the extended form.
        0x5c48 | 0x4c48 | 0x3848 => {
            no_cc()?;
            if bit(w, 43) {
                return Err("an extended shift".into());
            }
            Op::Shl {
                dst,
                a: Operand::Reg(reg(w, 8)),
                b: operand_b(w, false),
                wrap: bit(w, 39),
            }
        }
        // I2F (5cb8, 4cb8, 38b8): the float size (9:8, 2 for 32 bits), the
        // integer's size (11:10, 2 for 32 bits) and sign (13), rounding
        // 40:39, the byte select (42:41), neg 45, abs 49.
        0x5cb8 | 0x4cb8 | 0x38b8 => {
            no_cc()?;
            if bits(w, 8, 2) != 2 || bits(w, 10, 2) != 2 || bits(w, 41, 2) != 0 {
                return Err("a conversion other than 32-bit integer to 32-bit float".into());
            }
            round_rn(39)?;
            Op::I2F {
                dst,
                src: operand_b(w, false),
                signed: bit(w, 13),
                neg: bit(w, 45),
                abs: bit(w, 49),
            }
        }
        // F2I (5cb0, 4cb0, 38b0): the integer's size (9:8: 8, 16, 32 or 64
        // bits) and sign (12), the float's size (11:10), rounding 40:39
        // (none: to the nearest even, floor, ceil, trunc), neg 45, abs 49.
        0x5cb0 | 0x4cb0 | 0x38b0 => {
            no_cc()?;
            if bits(w, 8, 2) == 3 || bits(w, 10, 2) != 2 {
                return Err(
                    "a conversion other than 32-bit float to an integer of 32 bits or fewer".into(),
                );
            }
            Op::F2I {
                dst,
                src: FSrc {
                    op: operand_b(w, true),
                    neg: bit(w, 45),
                    abs: bit(w, 49),
                },
                signed: bit(w, 12),
                bits: 8 << bits(w, 8, 2),
                round: [Round::Nearest, Round::Floor, Round::Ceil, Round::Trunc]
                    [bits(w, 39, 2) as usize],
            }
        }
        // F2F (5ca8, 4ca8, 38a8): source size (11:10), destination size
        // (9:8), integer rounding when bit 42 is set (40:39: to the nearest
        // even, floor, ceil, trunc), sat 50, neg 45, abs 49.
        0x5ca8 | 0x4ca8 | 0x38a8 => {
            no_cc()?;
            if bits(w, 8, 2) != 2 || bits(w, 10, 2) != 2 {
                return Err("a conversion between float sizes".into());
            }
            let round = bit(w, 42).then(|| {
                [Round::Nearest, Round::Floor, Round::Ceil, Round::Trunc][bits(w, 39, 2) as usize]
            });
            Op::F2F {
                dst,
                src: FSrc {
                    op: operand_b(w, true),
                    neg: bit(w, 45),
                    abs: bit(w, 49),
                },
                round,
                sat: bit(w, 50),
            }
        }
        // LD a[] (efd8): REG_00, AMEM (REG_08 + 29:20), the size (48:47:
        // 1 to 4 words), O (32) reads outputs, P (31) patch attributes,
        // REG_39 the vertex (geometry and tessellation).
        0xefd8 => {
            if bit(w, 32) || bit(w, 31) || reg(w, 8) != RZ || reg(w, 39) != RZ {
                return Err("an indexed, output or patch attribute load".into());
            }
            Op::LdAttr {
                dst,
                count: bits(w, 47, 2) as u8 + 1,
                addr: bits(w, 20, 10) as u16,
            }
        }
        // ST a[] (eff0): REG_00 the first register stored, AMEM, the size,
        // P (31).
        0xeff0 => {
            if stage != Stage::Vertex || bit(w, 31) || reg(w, 8) != RZ || reg(w, 39) != RZ {
                return Err("an indexed or patch attribute store".into());
            }
            Op::StAttr {
                src: dst,
                count: bits(w, 47, 2) as u8 + 1,
                addr: bits(w, 20, 10) as u16,
            }
        }
        // LD c[] (ef90): REG_00, the buffer (40:36), REG_08 + the signed
        // byte offset (35:20), the size (50:48: 4 for 32 bits, 5 for 64),
        // the address mode (45:44).
        0xef90 => {
            if bits(w, 44, 2) != 0 {
                return Err(format!("LDC address mode {}", bits(w, 44, 2)));
            }
            if bits(w, 48, 3) != 4 {
                return Err(format!("LDC size {}", bits(w, 48, 3)));
            }
            Op::Mov {
                dst,
                src: Operand::Cbuf {
                    index: bits(w, 36, 5) as u8,
                    offset: bits(w, 20, 16) as u16 as i16 as i32,
                    reg: reg(w, 8),
                },
            }
        }
        _ => return decode_wide(w, stage),
    };
    Ok(Some(op))
}

/// The instructions whose opcodes are wider or narrower than 13 bits.
fn decode_wide(w: u64, stage: Stage) -> Result<Option<Op>, String> {
    let dst = reg(w, 0);
    let top = w >> 48;
    // MOV32I (0100, mask fff0): REG_00, the immediate (51:20), the lane
    // mask (15:12) all four lanes.
    if top & 0xfff0 == 0x0100 {
        if bits(w, 12, 4) != 0xf {
            return Err(format!("a lane mask of {:#x}", bits(w, 12, 4)));
        }
        return Ok(Some(Op::Mov {
            dst,
            src: Operand::Imm(bits(w, 20, 32)),
        }));
    }
    // FFMA (5980 reg/reg, 4980 cbuf/reg, 5180 reg/cbuf, 3280 imm/reg;
    // mask ff80, fe80 for the immediate): a REG_08, b and c by form, neg b
    // 48, neg c 49, sat 50, rounding 52:51, ftz/fmz 54:53.
    let ffma = match top & 0xff80 {
        0x5980 => Some((Operand::Reg(reg(w, 20)), Operand::Reg(reg(w, 39)))),
        0x4980 => Some((cbuf34(w), Operand::Reg(reg(w, 39)))),
        0x5180 => Some((Operand::Reg(reg(w, 39)), cbuf34(w))),
        _ if top & 0xfe80 == 0x3280 => Some((Operand::Imm(f20(w)), Operand::Reg(reg(w, 39)))),
        _ => None,
    };
    if let Some((b, c)) = ffma {
        if bit(w, 47) {
            return Err("a condition code write".into());
        }
        if bits(w, 51, 2) != 0 {
            return Err(format!("rounding mode {}", bits(w, 51, 2)));
        }
        return Ok(Some(Op::FFma {
            dst,
            a: FSrc::plain(Operand::Reg(reg(w, 8))),
            b: FSrc {
                op: b,
                neg: bit(w, 48),
                abs: false,
            },
            c: FSrc {
                op: c,
                neg: bit(w, 49),
                abs: false,
            },
            sat: bit(w, 50),
        }));
    }
    // IPA (e000, mask ff00 with REG_08 RZ and bit 38 clear): the mode
    // (55:54: pass, multiply, constant, sc), the sample position (53:52),
    // sat 51, REG_00, AMEM28 (29:28's 10 bits from 28), REG_20 the
    // multiplier, REG_39 the offset.
    if top & 0xff00 == 0xe000 {
        if stage != Stage::Fragment {
            return Err("IPA outside a pixel shader".into());
        }
        if reg(w, 8) != RZ || bit(w, 38) {
            return Err("an indexed attribute interpolation".into());
        }
        if bits(w, 52, 2) != 0 {
            return Err(format!("IPA sample position {}", bits(w, 52, 2)));
        }
        let mode = match bits(w, 54, 2) {
            0 => Interp::Linear,
            1 => Interp::Perspective,
            2 => Interp::Flat,
            m => return Err(format!("IPA mode {m}")),
        };
        let mult = reg(w, 20);
        if mode != Interp::Perspective && mult != RZ {
            return Err("a multiplier on a linear or flat interpolation".into());
        }
        return Ok(Some(Op::Ipa {
            dst,
            addr: bits(w, 28, 10) as u16,
            mode,
            mult,
            sat: bit(w, 51),
        }));
    }
    // TEXS (d0, mask f6): bit 59 clear is the 16-bit form (SM 6.0); the
    // target and LOD mode (56:53: 1 is a plain 2D sample); the component
    // mask (52:50) read with REG_28 (RZ: one register's worth); the
    // texture handle's word (48:36) in the texture constant buffer; REG_00
    // and REG_28 the destination pairs, REG_08 and REG_20 x and y.
    if top >> 8 & 0xf6 == 0xd0 {
        if !bit(w, 59) {
            return Err("a 16-bit texture sample".into());
        }
        if top >> 8 & 0x02 != 0 {
            return Err("a texture fetch other than TEXS".into());
        }
        if bits(w, 53, 4) != 1 {
            return Err(format!("TEXS target and level mode {}", bits(w, 53, 4)));
        }
        let second = reg(w, 28);
        let code = bits(w, 50, 3);
        let mask = if second == RZ {
            [1, 2, 4, 8, 3, 9, 10, 12][code as usize]
        } else {
            match code {
                0 => 7,
                1 => 11,
                2 => 13,
                3 => 14,
                4 => 15,
                m => return Err(format!("TEXS component mask {m}")),
            }
        };
        return Ok(Some(Op::Tex2d {
            dsts: [dst, second],
            mask,
            x: reg(w, 8),
            y: reg(w, 20),
            handle: bits(w, 36, 13) as u16,
        }));
    }
    Err("an instruction kestrel does not translate".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program: the header's words, then each instruction in groups of
    /// three after a scheduling word (0 here).
    fn program(header: [u32; 20], insns: &[u64]) -> Vec<u32> {
        let mut words = header.to_vec();
        for chunk in insns.chunks(3) {
            words.extend([0, 0]);
            for &i in chunk {
                words.extend([i as u32, (i >> 32) as u32]);
            }
        }
        words
    }

    fn vertex_header() -> [u32; 20] {
        let mut h = [0; 20];
        h[0] = 1 | 1 << 10;
        h
    }

    fn pixel_header() -> [u32; 20] {
        let mut h = [0; 20];
        h[0] = 2 | 5 << 10;
        h[18] = 0xf;
        h
    }

    #[test]
    fn the_triangle_examples_vertex_shader_decodes() {
        // deko_examples' basic_vsh.dksh (built from source with uam):
        // fields read from envytools' table lines. LD a[0x80] R0 (b64: R0,
        // R1); LD a[0x88] R2; MOV32I R3 = 1.0; ST a[0x70] (b128) R0..R3;
        // LD a[0x90] (b128) R0; ST a[0x80] (b128) R0; EXIT.
        let words = program(
            vertex_header(),
            &[
                0xefd8ff800807ff00,
                0xefd87f800887ff02,
                0x0103f8000007f003,
                0xeff1ff800707ff00,
                0xefd9ff800907ff00,
                0xeff1ff800807ff00,
                0xe30000000007000f,
            ],
        );
        let p = decode(&words).unwrap();
        assert_eq!(p.header.stage, Stage::Vertex);
        assert_eq!(
            p.ops,
            [
                Op::LdAttr {
                    dst: 0,
                    count: 2,
                    addr: 0x80
                },
                Op::LdAttr {
                    dst: 2,
                    count: 1,
                    addr: 0x88
                },
                Op::Mov {
                    dst: 3,
                    src: Operand::Imm(0x3f80_0000)
                },
                Op::StAttr {
                    src: 0,
                    count: 4,
                    addr: 0x70
                },
                Op::LdAttr {
                    dst: 0,
                    count: 4,
                    addr: 0x90
                },
                Op::StAttr {
                    src: 0,
                    count: 4,
                    addr: 0x80
                },
                Op::Exit,
            ]
        );
    }

    #[test]
    fn the_colour_shader_interpolates_with_one_over_w() {
        // color_fsh.dksh: IPA.PASS R0 = a[0x7c]; MUFU.RCP R2 = 1/R0; MOV32I
        // R3 = 1.0; IPA (multiply) R0 = a[0x80] * R2, R1 = a[0x84], R2 =
        // a[0x88]; EXIT.
        let words = program(
            pixel_header(),
            &[
                0xe003ff87cff7ff00,
                0x5080000000470002,
                0x0103f8000007f003,
                0xe043ff880027ff00,
                0xe043ff884027ff01,
                0xe043ff888027ff02,
                0xe30000000007000f,
            ],
        );
        let p = decode(&words).unwrap();
        let ipa = |dst, addr| Op::Ipa {
            dst,
            addr,
            mode: Interp::Perspective,
            mult: 2,
            sat: false,
        };
        assert_eq!(
            p.ops,
            [
                Op::Ipa {
                    dst: 0,
                    addr: 0x7c,
                    mode: Interp::Linear,
                    mult: RZ,
                    sat: false
                },
                Op::Mufu {
                    dst: 2,
                    func: Mufu::Rcp,
                    src: FSrc::plain(Operand::Reg(0)),
                    sat: false
                },
                Op::Mov {
                    dst: 3,
                    src: Operand::Imm(0x3f80_0000)
                },
                ipa(0, 0x80),
                ipa(1, 0x84),
                ipa(2, 0x88),
                Op::Exit,
            ]
        );
        assert_eq!(p.header.omap_targets, 0xf);
    }

    #[test]
    fn transform_and_texture_instructions_decode() {
        // transform_vsh.dksh's FMUL R2 = R0 * c[2][0] (ftz), FFMA R2 = R1 *
        // c[2][0x10] + R2, FADD R1 = R2 + c[2][0x30]; texture_fsh.dksh's
        // TEXS 2D RGBA into R0..R3 at (R1, R0), handle word 0x1a4; the
        // triangle vertex shader's SHL R7 = R0 << 4 and LDC R0 =
        // c[1][R7 + 0x30].
        let words = program(
            pixel_header(),
            &[
                0x4c68100800070002,
                0x49a0010800470102,
                0x4c58100800c70201,
                0xd8301a4020070100,
                0x3848000000470007,
                0xef94001003070700,
                0xe30000000007000f,
            ],
        );
        let p = decode(&words).unwrap();
        let cb = |index, offset| Operand::Cbuf {
            index,
            offset,
            reg: RZ,
        };
        assert_eq!(
            p.ops[..6],
            [
                Op::FMul {
                    dst: 2,
                    a: FSrc::plain(Operand::Reg(0)),
                    b: FSrc::plain(cb(2, 0)),
                    scale: 0,
                    sat: false
                },
                Op::FFma {
                    dst: 2,
                    a: FSrc::plain(Operand::Reg(1)),
                    b: FSrc::plain(cb(2, 0x10)),
                    c: FSrc::plain(Operand::Reg(2)),
                    sat: false
                },
                Op::FAdd {
                    dst: 1,
                    a: FSrc::plain(Operand::Reg(2)),
                    b: FSrc::plain(cb(2, 0x30)),
                    sat: false
                },
                Op::Tex2d {
                    dsts: [0, 2],
                    mask: 0xf,
                    x: 1,
                    y: 0,
                    handle: 0x1a4
                },
                Op::Shl {
                    dst: 7,
                    a: Operand::Reg(0),
                    b: Operand::Imm(4),
                    wrap: false
                },
                Op::Mov {
                    dst: 0,
                    src: Operand::Cbuf {
                        index: 1,
                        offset: 0x30,
                        reg: 7
                    }
                },
            ]
        );
    }

    #[test]
    fn what_is_not_translated_stops_named() {
        let e = decode(&program(vertex_header(), &[0xe2400fffff87000f])).unwrap_err();
        assert!(e.contains("a branch"), "{e}");
        // A predicated MOV32I (P0).
        let e = decode(&program(vertex_header(), &[0x0103f8000000f003])).unwrap_err();
        assert!(e.contains("a predicated instruction"), "{e}");
        let e = decode(&program(vertex_header(), &[0xffff_ffff_ffff_ffff])).unwrap_err();
        assert!(
            e.contains("does not translate") || e.contains("predicated"),
            "{e}"
        );
        assert!(decode(&[0; 4])
            .unwrap_err()
            .contains("shorter than 80 bytes"));
    }
}
