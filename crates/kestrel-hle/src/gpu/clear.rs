//! Render targets and clears (phase 5B): the 3D engine's colour targets
//! (SET_COLOR_TARGET_*), and CLEAR_SURFACE, which fills one of them with
//! the clear colour.
//!
//! Sources: NVIDIA's `clb197.h` (MIT) for the methods, their fields and
//! the colour formats' names; deko3d (zlib) for how a program drives them
//! and what the names mean: its format table (`image_formats.h`,
//! `format_traits.inc`) gives A8B8G8R8 (0xD5) as RGBA8, the first-named
//! channel in the high bits of the pixel; `gpu_3d_base.cpp` binds a target
//! with its width in pixels when block linear and in bytes (its pitch)
//! when pitch linear, and its array pitch in units of 4 bytes; its clear
//! (`clear_buffers.mme`) clears each layer by RT_ARRAY_INDEX, after
//! choosing the area with scissor 0 (SET_CLEAR_SURFACE_CONTROL).

use super::transfer::Surface;
use super::{class, Engine};
use crate::display::Blocks;
use crate::service::IpcError;
use crate::Process;

/// How a colour format's channels are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Unsigned normalised: 0.0 to 1.0 as 0 to the largest value.
    Unorm,
    /// Unorm with the colour channels sRGB-encoded (alpha linear).
    Srgb,
    /// IEEE floating point, 16 or 32 bits.
    Float,
}

/// A colour format: its pixel size and where R, G, B and A are (bit
/// offset, bits; 0 bits for a channel it does not have).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorFormat {
    pub bytes: usize,
    pub channels: [(u32, u32); 4],
    pub encoding: Encoding,
}

/// The colour formats kestrel writes, by SET_COLOR_TARGET_FORMAT code
/// (`clb197.h`). A name gives the channels from the high bits down; L
/// marks sRGB ("RL8"), X unused bits, F floats. Others stop, named.
pub fn color_format(code: u32) -> Option<ColorFormat> {
    use Encoding::*;
    let f = |bytes, channels, encoding| {
        Some(ColorFormat {
            bytes,
            channels,
            encoding,
        })
    };
    match code {
        // RF32_GF32_BF32_AF32, RF16_GF16_BF16_AF16
        0xc0 => f(16, [(0, 32), (32, 32), (64, 32), (96, 32)], Float),
        0xca => f(8, [(0, 16), (16, 16), (32, 16), (48, 16)], Float),
        // A8R8G8B8, A8RL8GL8BL8
        0xcf => f(4, [(16, 8), (8, 8), (0, 8), (24, 8)], Unorm),
        0xd0 => f(4, [(16, 8), (8, 8), (0, 8), (24, 8)], Srgb),
        // A2B10G10R10
        0xd1 => f(4, [(0, 10), (10, 10), (20, 10), (30, 2)], Unorm),
        // A8B8G8R8, A8BL8GL8RL8
        0xd5 => f(4, [(0, 8), (8, 8), (16, 8), (24, 8)], Unorm),
        0xd6 => f(4, [(0, 8), (8, 8), (16, 8), (24, 8)], Srgb),
        // X8R8G8B8, X8RL8GL8BL8
        0xe6 => f(4, [(16, 8), (8, 8), (0, 8), (0, 0)], Unorm),
        0xe7 => f(4, [(16, 8), (8, 8), (0, 8), (0, 0)], Srgb),
        // R5G6B5
        0xe8 => f(2, [(11, 5), (5, 6), (0, 5), (0, 0)], Unorm),
        // G8R8, R8
        0xea => f(2, [(0, 8), (8, 8), (0, 0), (0, 0)], Unorm),
        0xf3 => f(1, [(0, 8), (0, 0), (0, 0), (0, 0)], Unorm),
        // X8B8G8R8, X8BL8GL8RL8
        0xf9 => f(4, [(0, 8), (8, 8), (16, 8), (0, 0)], Unorm),
        0xfa => f(4, [(0, 8), (8, 8), (16, 8), (0, 0)], Srgb),
        _ => None,
    }
}

/// `v` as an unsigned normalised value of `bits` bits: clamped to 0.0 to
/// 1.0, scaled and rounded to the nearest, halves up (kestrel's choice;
/// NaN gives 0).
fn unorm(v: f64, bits: u32) -> u128 {
    let max = ((1u64 << bits) - 1) as f64;
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    (v * max + 0.5).floor() as u128
}

/// The sRGB encoding of a linear value (IEC 61966-2-1).
fn srgb(l: f64) -> f64 {
    if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// `v` as an IEEE half-precision float, rounded to the nearest, ties to
/// even.
pub fn f16_bits(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        // Infinity, or NaN (kept quiet, with a mantissa bit set).
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    // The mantissa with its leading one, shifted to 10 bits (or fewer for
    // subnormal halves), rounded to the nearest, ties to even.
    let (full, shift) = if e <= 0 {
        if e < -10 {
            return sign;
        }
        (man | 0x80_0000, (14 - e) as u32)
    } else {
        (man, 13)
    };
    let half = 1u32 << (shift - 1);
    let mut m = full >> shift;
    let rest = full & ((1 << shift) - 1);
    if rest > half || (rest == half && m & 1 == 1) {
        m += 1;
    }
    // A carry out of the mantissa moves into the exponent, as it should.
    sign | if e <= 0 {
        m as u16
    } else {
        (((e as u32) << 10) + m) as u16
    }
}

impl ColorFormat {
    /// The pixel for clear colour `rgba` (the clear value registers'
    /// floats), as bits, low bits first.
    pub fn pixel(&self, rgba: [f32; 4]) -> u128 {
        let mut p = 0u128;
        for (c, &(shift, bits)) in self.channels.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            let v = rgba[c];
            let raw = match (self.encoding, bits) {
                (Encoding::Float, 32) => v.to_bits() as u128,
                (Encoding::Float, _) => f16_bits(v) as u128,
                (Encoding::Srgb, _) if c < 3 => unorm(srgb(v as f64), bits),
                _ => unorm(v as f64, bits),
            };
            p |= raw << shift;
        }
        p
    }

    /// The bits of the channels `enables` (R, G, B, A in bits 0 to 3)
    /// names that this format has.
    pub fn mask(&self, enables: u32) -> u128 {
        let mut m = 0u128;
        for (c, &(shift, bits)) in self.channels.iter().enumerate() {
            if bits != 0 && enables >> c & 1 == 1 {
                m |= ((1u128 << bits) - 1) << shift;
            }
        }
        m
    }
}

/// A colour target as SET_COLOR_TARGET_A-LAYER(n) describe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorTarget {
    pub address: u64,
    /// Pixels wide.
    pub width: u32,
    pub height: u32,
    pub format: ColorFormat,
    /// Block linear: the block size; pitch linear: the pitch in bytes.
    pub layout: Layout,
    /// The layers (or slices) after the first: bytes apart, for arrays.
    pub array_pitch: u64,
    /// Whether the third dimension is a 3D surface's depth (slices of
    /// the block-linear layout) rather than an array's layers.
    pub depth: bool,
    pub layers: u32,
    pub first_layer: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Blocks([u32; 3]),
    Pitch(u32),
}

/// The 3D methods read here (`clb197.h`).
mod m {
    pub const COLOR_TARGET: u32 = 0x800;
    pub const CLEAR_RECT_HORIZONTAL: u32 = 0xd6c;
    pub const CLEAR_RECT_VERTICAL: u32 = 0xd70;
    pub const COLOR_CLEAR_VALUE: u32 = 0xd80;
    pub const WINDOW_OFFSET_X: u32 = 0xdf8;
    pub const WINDOW_OFFSET_Y: u32 = 0xdfc;
    pub const SCISSOR_ENABLE: u32 = 0xe00;
    pub const SCISSOR_HORIZONTAL: u32 = 0xe04;
    pub const SCISSOR_VERTICAL: u32 = 0xe08;
    pub const SURFACE_CLIP_HORIZONTAL: u32 = 0xff4;
    pub const SURFACE_CLIP_VERTICAL: u32 = 0xff8;
    pub const CLEAR_SURFACE_CONTROL: u32 = 0x10f8;
    pub const CT_SELECT: u32 = 0x121c;
}

/// Colour target `n`'s registers, or what kestrel does not read in them.
fn color_target(e: &Engine, n: u32) -> Result<Option<ColorTarget>, String> {
    let r = |i: u32| e.reg(m::COLOR_TARGET + n * 64 + i * 4);
    let code = r(4) & 0xff;
    if code == 0 {
        // DISABLED.
        return Ok(None);
    }
    let format = color_format(code).ok_or_else(|| format!("colour format {code:#x}"))?;
    let memory = r(5);
    let width = r(2) & 0xfff_ffff;
    let layout = if memory >> 12 & 1 == 1 {
        Layout::Pitch(width)
    } else {
        let log2 = [memory & 0xf, memory >> 4 & 0xf, memory >> 8 & 0xf];
        if log2.iter().any(|&l| l > 5) {
            return Err(format!("a block size of {:#x}", memory & 0xfff));
        }
        Layout::Blocks(log2)
    };
    let width = match layout {
        Layout::Pitch(pitch) => pitch / format.bytes as u32,
        Layout::Blocks(_) => width,
    };
    Ok(Some(ColorTarget {
        address: ((r(0) & 0xff) as u64) << 32 | r(1) as u64,
        width,
        height: r(3) & 0x1_ffff,
        format,
        layout,
        array_pitch: r(7) as u64 * 4,
        depth: memory >> 16 & 1 == 1,
        layers: r(6) & 0xfff_ffff,
        first_layer: r(8) & 0xffff,
    }))
}

/// A rectangle, minimum inclusive, maximum exclusive, from a register
/// with the minimum in 15:0 and the maximum in 31:16.
fn span(v: u32) -> (u32, u32) {
    (v & 0xffff, v >> 16)
}

impl Process {
    /// CLEAR_SURFACE (`clb197.h`): fill the colour target MRT_SELECT
    /// (9:6) names, its layer RT_ARRAY_INDEX (25:10), in the channels R-A
    /// (2-5) enable, with SET_COLOR_CLEAR_VALUE(0-3). Z (0) and STENCIL
    /// (1) clear the depth target: not run yet.
    pub(super) fn clear_surface(&mut self, fd: u32, value: u32) -> Result<(), IpcError> {
        let named = |what: &str| {
            IpcError::Unimplemented(format!("GPU 3D method CLEAR_SURFACE = {value:#x}: {what}"))
        };
        if value & 3 != 0 {
            return Err(named("a depth or stencil clear"));
        }
        let enables = value >> 2 & 0xf;
        let state = self.channel_state(fd);
        let e = &state.engines[&class::THREED];
        // MRT_SELECT picks one of the targets SET_CT_SELECT lists:
        // TARGET_COUNT (3:0), then TARGETn (3 bits each from bit 4)
        // naming a SET_COLOR_TARGET_* (kestrel's reading; deko3d lists them
        // in order).
        let select = value >> 6 & 0xf;
        let ct = e.reg(m::CT_SELECT);
        if select >= (ct & 0xf).max(1) {
            return Err(named(&format!(
                "render target {select} of the {} selected",
                ct & 0xf
            )));
        }
        let n = ct >> (4 + 3 * select) & 7;
        let Some(target) = color_target(e, n).map_err(|w| named(&w))? else {
            return Ok(());
        };
        // The area: the target, within the surface clip; with
        // SET_CLEAR_SURFACE_CONTROL's USE_CLEAR_RECT (4) the clear
        // rectangle, USE_SCISSOR0 (8) scissor 0 if enabled. USE_VIEWPORT_
        // CLIP0 (12) and window offsets are not read yet.
        let control = e.reg(m::CLEAR_SURFACE_CONTROL);
        if control >> 12 & 1 == 1 {
            return Err(named("a clear limited by viewport 0"));
        }
        if e.reg(m::WINDOW_OFFSET_X) != 0 || e.reg(m::WINDOW_OFFSET_Y) != 0 {
            return Err(named("a window offset"));
        }
        let (mut x0, mut x1) = (0, target.width);
        let (mut y0, mut y1) = (0, target.height);
        let mut limit = |(a0, a1): (u32, u32), (b0, b1): (u32, u32)| {
            x0 = x0.max(a0);
            x1 = x1.min(a1);
            y0 = y0.max(b0);
            y1 = y1.min(b1);
        };
        let (cx, cw) = span(e.reg(m::SURFACE_CLIP_HORIZONTAL));
        let (cy, ch) = span(e.reg(m::SURFACE_CLIP_VERTICAL));
        limit((cx, cx + cw), (cy, cy + ch));
        if control >> 4 & 1 == 1 {
            limit(
                span(e.reg(m::CLEAR_RECT_HORIZONTAL)),
                span(e.reg(m::CLEAR_RECT_VERTICAL)),
            );
        }
        if control >> 8 & 1 == 1 && e.reg(m::SCISSOR_ENABLE) & 1 == 1 {
            limit(
                span(e.reg(m::SCISSOR_HORIZONTAL)),
                span(e.reg(m::SCISSOR_VERTICAL)),
            );
        }
        let layer = (value >> 10 & 0xffff) + target.first_layer;
        if layer >= target.layers.max(1) {
            return Err(named(&format!(
                "layer {layer} of a target with {}",
                target.layers
            )));
        }
        let rgba = [0, 1, 2, 3].map(|i| f32::from_bits(e.reg(m::COLOR_CLEAR_VALUE + i * 4)));
        if x0 >= x1 || y0 >= y1 || enables == 0 {
            return Ok(());
        }
        let f = target.format;
        let (pixel, mask) = (f.pixel(rgba), f.mask(enables));
        let full = (1u128 << (f.bytes * 8 - 1) << 1).wrapping_sub(1);
        // The surface, and the bytes the clear touches: the rows of blocks
        // (or of pixels) it falls in, read once, filled, written back.
        let (surface, range) = match target.layout {
            Layout::Pitch(pitch) => {
                let base = target.address + layer as u64 * target.array_pitch;
                let first = y0 as u64 * pitch as u64;
                (
                    Surface::Pitch {
                        address: base,
                        pitch,
                    },
                    first..(y1 as u64 - 1) * pitch as u64 + target.width as u64 * f.bytes as u64,
                )
            }
            Layout::Blocks(log2) => {
                let blocks = Blocks {
                    width: target.width as usize * f.bytes,
                    height: target.height as usize,
                    log2,
                };
                let (base, z) = if target.depth {
                    (target.address, layer)
                } else {
                    (target.address + layer as u64 * target.array_pitch, 0)
                };
                let rows = blocks.rows(y0 as usize, y1 as usize, z as usize);
                (
                    Surface::Blocks {
                        address: base,
                        blocks,
                        origin: [0, 0, z],
                    },
                    rows.start as u64..rows.end as u64,
                )
            }
        };
        let base = match surface {
            Surface::Pitch { address, .. } | Surface::Blocks { address, .. } => address,
        };
        // Pixels are 1, 2, 4, 8 or 16 bytes: a 16-byte run (the most a GOB
        // keeps contiguous) starts on a pixel and holds whole pixels.
        let mut pattern = [0u8; 16];
        for px in pattern.chunks_exact_mut(f.bytes) {
            px.copy_from_slice(&pixel.to_le_bytes()[..f.bytes]);
        }
        let len = (range.end - range.start) as usize;
        // All of every pixel across whole rows (of blocks, for block
        // linear): the bytes are the pattern throughout, the alignment
        // padding past the target's edges included (no one reads it).
        let whole = mask == full
            && x0 == 0
            && x1 == target.width
            && match target.layout {
                Layout::Pitch(pitch) => pitch as usize == target.width as usize * f.bytes,
                Layout::Blocks([_, h, _]) => {
                    let by = 8 << h;
                    y0 % by == 0 && (y1 % by == 0 || y1 == target.height)
                }
            };
        if whole && len % 16 == 0 {
            let bytes = pattern.repeat(len / 16);
            return self.gpu_write_bytes(fd, base + range.start, &bytes);
        }
        let mut bytes = self.gpu_read_bytes(fd, base + range.start, len)?;
        let fill = |run: &mut [u8]| {
            if mask == full {
                run.copy_from_slice(&pattern[..run.len()]);
                return;
            }
            for px in run.chunks_exact_mut(f.bytes) {
                let mut old = [0u8; 16];
                old[..f.bytes].copy_from_slice(px);
                let new = (u128::from_le_bytes(old) & !mask) | (pixel & mask);
                px.copy_from_slice(&new.to_le_bytes()[..f.bytes]);
            }
        };
        let (xb0, xb1) = (x0 as usize * f.bytes, x1 as usize * f.bytes);
        let start = range.start as usize;
        for y in y0 as usize..y1 as usize {
            match surface {
                Surface::Pitch { pitch, .. } => {
                    let at = y * pitch as usize + xb0 - start;
                    for run in bytes[at..at + xb1 - xb0].chunks_mut(16) {
                        fill(run);
                    }
                }
                Surface::Blocks { blocks, origin, .. } => {
                    let mut x = xb0;
                    while x < xb1 {
                        let n = (16 - x % 16).min(xb1 - x);
                        let at = blocks.offset(x, y, origin[2] as usize) - start;
                        fill(&mut bytes[at..at + n]);
                        x += n;
                    }
                }
            }
        }
        self.gpu_write_bytes(fd, base + range.start, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_colours_are_packed_as_the_formats_name_them() {
        let rgba8 = color_format(0xd5).unwrap();
        // 0.0, 0.25, 0.5, 1.0: 0, 63.75 and 127.5 rounded up, 255.
        assert_eq!(rgba8.pixel([0.0, 0.25, 0.5, 1.0]), 0xff80_4000);
        // Out of range values are clamped.
        assert_eq!(rgba8.pixel([-1.0, 2.0, 0.0, 0.0]), 0x0000_ff00);
        let bgra8 = color_format(0xcf).unwrap();
        assert_eq!(bgra8.pixel([1.0, 0.0, 0.0, 0.0]), 0x00ff_0000);
        let rgb565 = color_format(0xe8).unwrap();
        assert_eq!(rgb565.pixel([1.0, 0.0, 1.0, 1.0]), 0xf81f);
        let rgb10a2 = color_format(0xd1).unwrap();
        assert_eq!(rgb10a2.pixel([1.0, 0.0, 0.0, 1.0]), 0xc000_03ff);
        // sRGB: 0.5 linear is 0.7354, 187.5 rounded up; alpha linear.
        let srgb = color_format(0xd6).unwrap();
        assert_eq!(srgb.pixel([0.5, 0.0, 1.0, 0.5]), 0x80ff_00bc);
        let f32x4 = color_format(0xc0).unwrap();
        assert_eq!(f32x4.pixel([1.0, 0.0, 0.0, -2.0]) >> 96, 0xc000_0000);
        let f16x4 = color_format(0xca).unwrap();
        assert_eq!(
            f16x4.pixel([1.0, 0.5, -2.0, 65504.0]),
            0x7bff_c000_3800_3c00
        );
        assert_eq!(rgba8.mask(0b0101), 0x00ff_00ff);
        assert_eq!(color_format(0xf9).unwrap().mask(0xf), 0x00ff_ffff);
    }

    #[test]
    fn halves_round_to_the_nearest_even() {
        assert_eq!(f16_bits(1.0), 0x3c00);
        assert_eq!(f16_bits(-0.0), 0x8000);
        assert_eq!(f16_bits(65520.0), 0x7c00, "past the largest half");
        assert_eq!(f16_bits(f32::INFINITY), 0x7c00);
        assert_eq!(f16_bits(f32::NAN) & 0x7e00, 0x7e00);
        // 1 + 2^-11 is halfway between 1 and the next half: to even (1).
        assert_eq!(f16_bits(1.0 + 1.0 / 2048.0), 0x3c00);
        assert_eq!(f16_bits(1.0 + 3.0 / 2048.0), 0x3c02);
        // The smallest subnormal half, 2^-24, and half of it (to even, 0).
        assert_eq!(f16_bits(2f32.powi(-24)), 1);
        assert_eq!(f16_bits(2f32.powi(-25)), 0);
        // The largest subnormal rounds up into the smallest normal.
        assert_eq!(f16_bits(2f32.powi(-14) - 2f32.powi(-26)), 0x0400);
    }
}
