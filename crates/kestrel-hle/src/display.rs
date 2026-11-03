//! Frames: what a program shows, turned into pixels a frontend can paint.
//!
//! A program draws into memory it registered with nvmap and queues it to
//! the display (`service/vi.rs`). The buffer's description, from the
//! program, gives its size, pixel format and memory layout. The console's
//! display reads two layouts:
//!
//! - pitch: rows one after another, `pitch` bytes apart;
//! - block linear, the GPU's tiled layout. envytools ("Memory surface
//!   formats", `docs/hw/memory/g80-surface.rst`) describes the blocks: a
//!   surface is split into blocks one GOB wide and 2^block_height_log2
//!   GOBs high, stored left to right, then top to bottom, and a block's
//!   GOBs top to bottom. A GOB on GF100 and later is 64 bytes by 8 rows.
//!   Inside a GOB the bytes are arranged as Mesa describes the Tegra
//!   colour GOB (`src/nouveau/nil/tiling.rs`, `GOBType::TegraColor`):
//!   32-byte sectors of two 16-byte rows, numbered down the first 32-byte
//!   column in pairs (0 1 / 2 3 / 4 5 / 6 7), then the second (8 9 /
//!   ... / 14 15).

/// A frame: `width` × `height` pixels as 0x00RRGGBB, row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// Where frames go: the window, or a test that keeps them.
pub trait Screen: Send {
    fn present(&mut self, frame: &Frame);
}

/// Android's pixel formats (`PIXEL_FORMAT_*`, AOSP `graphics.h`), which a
/// Switch buffer's description uses.
pub mod format {
    pub const RGBA_8888: u32 = 1;
    pub const RGBX_8888: u32 = 2;
    pub const RGB_565: u32 = 4;
    pub const BGRA_8888: u32 = 5;
}

/// The memory layouts a buffer's surface can have (NvLayout).
pub mod layout {
    pub const PITCH: u32 = 1;
    pub const BLOCK_LINEAR: u32 = 3;
}

/// A GOB: 64 bytes by 8 rows.
const GOB_WIDTH: usize = 64;
const GOB_HEIGHT: usize = 8;
const GOB_SIZE: usize = GOB_WIDTH * GOB_HEIGHT;

/// The offset of byte `x` of row `y` in a block-linear surface `pitch`
/// bytes wide (a multiple of 64) with blocks 2^`block_height_log2` GOBs
/// high.
pub fn block_linear_offset(x: usize, y: usize, pitch: usize, block_height_log2: u32) -> usize {
    let blocks = Blocks {
        width: pitch,
        height: 0,
        log2: [0, block_height_log2, 0],
    };
    blocks.offset(x, y, 0)
}

/// A block-linear surface in three dimensions, as envytools gives it in
/// full ("Blocklinear surfaces"): `width` bytes by `height` rows (the
/// rows of one slice), blocks 2^`log2[0]` GOBs wide, 2^`log2[1]` high and
/// 2^`log2[2]` deep (a GOB is one slice deep). Blocks are stored across,
/// then down, then back through the slices; a block's GOBs across, then
/// down, then back. The surface is aligned up to whole blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocks {
    pub width: usize,
    pub height: usize,
    pub log2: [u32; 3],
}

impl Blocks {
    /// The bytes that hold rows `y0..y1` of slice `z`: the rows of blocks
    /// they fall in, all the way across.
    pub fn rows(&self, y0: usize, y1: usize, z: usize) -> std::ops::Range<usize> {
        let [w, h, d] = self.log2;
        let (by, bz) = (GOB_HEIGHT << h, 1usize << d);
        let row_bytes = self.width.div_ceil(GOB_WIDTH << w) * (GOB_SIZE << (w + h + d));
        let slab = (z / bz) * row_bytes * self.height.div_ceil(by);
        slab + (y0 / by) * row_bytes..slab + (y1.max(y0 + 1) - 1) / by * row_bytes + row_bytes
    }

    /// The offset of byte `x` of row `y` of slice `z`.
    pub fn offset(&self, x: usize, y: usize, z: usize) -> usize {
        // Every size is a power of two: shifts and masks, no divisions
        // (this runs for every 16 bytes a clear or a frame touches).
        let [w, h, d] = self.log2;
        let (sx, sy) = (6 + w, 3 + h);
        let across = (self.width + (1 << sx) - 1) >> sx;
        let down = (self.height + (1 << sy) - 1) >> sy;
        let block = (x >> sx) + (y >> sy) * across + (z >> d) * across * down;
        let (xb, yb, zb) = (x & ((1 << sx) - 1), y & ((1 << sy) - 1), z & ((1 << d) - 1));
        let gob = (xb >> 6) + ((yb >> 3) << w) + (zb << (w + h));
        let (gx, gy) = (xb & 63, yb & 7);
        let sector = (gx >> 5) * 8 + (gy >> 1) * 2 + ((gx >> 4) & 1);
        let in_gob = sector * 32 + (gy & 1) * 16 + (gx & 15);
        (block << (9 + w + h + d)) + gob * GOB_SIZE + in_gob
    }
}

/// The bytes one pixel of `format` takes.
pub fn bytes_per_pixel(format: u32) -> Option<usize> {
    match format {
        format::RGBA_8888 | format::RGBX_8888 | format::BGRA_8888 => Some(4),
        format::RGB_565 => Some(2),
        _ => None,
    }
}

/// One pixel of `format` as 0x00RRGGBB. RGB 565's 5 and 6-bit channels
/// become 8 bits with their top bits repeated below them, so full
/// intensity stays 0xff (kestrel's choice).
fn pixel(format: u32, b: &[u8]) -> u32 {
    let rgb = |r: u32, g: u32, b: u32| r << 16 | g << 8 | b;
    match format {
        format::RGBA_8888 | format::RGBX_8888 => rgb(b[0] as u32, b[1] as u32, b[2] as u32),
        format::BGRA_8888 => rgb(b[2] as u32, b[1] as u32, b[0] as u32),
        format::RGB_565 => {
            let v = u16::from_le_bytes([b[0], b[1]]) as u32;
            let (r, g, b) = (v >> 11, v >> 5 & 0x3f, v & 0x1f);
            rgb(r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)
        }
        _ => unreachable!("checked by bytes_per_pixel"),
    }
}

/// A surface as the program described it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub layout: u32,
    /// Bytes from one row to the next (pitch), or the width the blocks
    /// cover (block linear).
    pub pitch: u32,
    pub block_height_log2: u32,
}

impl Surface {
    /// The bytes the surface's pixels reach into its memory.
    pub fn size(&self) -> Result<usize, String> {
        let bpp = bytes_per_pixel(self.format)
            .ok_or_else(|| format!("unsupported: pixel format {}", self.format))?;
        let (w, h, pitch) = (
            self.width as usize,
            self.height as usize,
            self.pitch as usize,
        );
        if w == 0 || h == 0 || w * bpp > pitch {
            return Err(format!(
                "a {w}x{h} surface of {bpp}-byte pixels with a pitch of {pitch} bytes"
            ));
        }
        match self.layout {
            layout::PITCH => Ok(pitch * (h - 1) + w * bpp),
            layout::BLOCK_LINEAR if pitch % GOB_WIDTH == 0 && self.block_height_log2 <= 5 => {
                let block_rows = GOB_HEIGHT << self.block_height_log2;
                Ok(pitch * h.next_multiple_of(block_rows))
            }
            layout::BLOCK_LINEAR => Err(format!(
                "a block-linear surface with a pitch of {pitch} bytes and blocks 2^{} GOBs high",
                self.block_height_log2
            )),
            l => Err(format!("unsupported: surface layout {l}")),
        }
    }

    /// The frame in `memory`, which holds at least `size()` bytes.
    pub fn frame(&self, memory: &[u8]) -> Frame {
        let bpp = bytes_per_pixel(self.format).expect("checked by size()");
        let (w, h, pitch) = (
            self.width as usize,
            self.height as usize,
            self.pitch as usize,
        );
        let mut pixels = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let at = match self.layout {
                    layout::PITCH => y * pitch + x * bpp,
                    _ => block_linear_offset(x * bpp, y, pitch, self.block_height_log2),
                };
                pixels.push(pixel(self.format, &memory[at..at + bpp]));
            }
        }
        Frame {
            width: self.width,
            height: self.height,
            pixels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gob_holds_its_sectors_as_mesa_draws_the_tegra_gob() {
        // Mesa's TegraColor table: sector n at (column, row of sectors).
        let table = [[0, 1, 8, 9], [2, 3, 10, 11], [4, 5, 12, 13], [6, 7, 14, 15]];
        for (row, sectors) in table.iter().enumerate() {
            for (column, &sector) in sectors.iter().enumerate() {
                // A sector's two 16-byte rows follow each other.
                let (x, y) = (column * 16, row * 2);
                assert_eq!(block_linear_offset(x, y, 64, 0), sector * 32);
                assert_eq!(block_linear_offset(x + 15, y + 1, 64, 0), sector * 32 + 31);
            }
        }
        assert_eq!(block_linear_offset(63, 7, 64, 0), 511);
    }

    #[test]
    fn blocks_in_three_dimensions_follow_envytools_order() {
        // Blocks 2 GOBs wide, 2 high, 2 deep (8 GOBs, 4 KiB) on a surface
        // 200 bytes by 20 rows: 2 blocks across (128 bytes each), 2 down
        // (16 rows each), 4 a slice of blocks.
        let b = Blocks {
            width: 200,
            height: 20,
            log2: [1, 1, 1],
        };
        let block = 8 * 512;
        // Inside a block, GOBs go across, then down, then back.
        assert_eq!(b.offset(64, 0, 0), 512);
        assert_eq!(b.offset(0, 8, 0), 2 * 512);
        assert_eq!(b.offset(0, 0, 1), 4 * 512);
        assert_eq!(b.offset(64, 8, 1), 7 * 512);
        // Blocks go across, then down, then back.
        assert_eq!(b.offset(128, 0, 0), block);
        assert_eq!(b.offset(0, 16, 0), 2 * block);
        assert_eq!(b.offset(0, 0, 2), 4 * block);
        // Inside the last block: the second GOB slice, row 1, byte 2.
        assert_eq!(b.offset(130, 17, 3) - b.offset(128, 16, 2), 4 * 512 + 18);
        assert_eq!(b.offset(128, 16, 2), 7 * block);
        // Rows 3 to 17 of slice 3: both rows of blocks of the second slab.
        assert_eq!(b.rows(3, 18, 3), 4 * block..8 * block);
        assert_eq!(b.rows(16, 17, 0), 2 * block..4 * block);
    }

    #[test]
    fn blocks_go_across_then_down_and_gobs_down_a_block() {
        // Blocks of 16 GOBs (128 rows), four blocks across 256 bytes.
        let block = 16 * 512;
        assert_eq!(block_linear_offset(0, 8, 256, 4), 512, "the second GOB");
        assert_eq!(block_linear_offset(0, 120, 256, 4), 15 * 512);
        assert_eq!(block_linear_offset(64, 0, 256, 4), block, "the next block");
        assert_eq!(block_linear_offset(192, 0, 256, 4), 3 * block);
        assert_eq!(
            block_linear_offset(0, 128, 256, 4),
            4 * block,
            "the next row"
        );
        assert_eq!(
            block_linear_offset(65, 129, 256, 0),
            (16 * 4 + 1) * 512 + 17
        );
    }

    #[test]
    fn pixels_become_rgb() {
        assert_eq!(
            pixel(format::RGBA_8888, &[0x12, 0x34, 0x56, 0x78]),
            0x123456
        );
        assert_eq!(pixel(format::RGBX_8888, &[0x12, 0x34, 0x56, 0]), 0x123456);
        assert_eq!(
            pixel(format::BGRA_8888, &[0x12, 0x34, 0x56, 0x78]),
            0x563412
        );
        // RGB 565: red in the top five bits, blue in the bottom five.
        assert_eq!(pixel(format::RGB_565, &0xffffu16.to_le_bytes()), 0xffffff);
        assert_eq!(pixel(format::RGB_565, &0xf800u16.to_le_bytes()), 0xff0000);
        assert_eq!(pixel(format::RGB_565, &0x07e0u16.to_le_bytes()), 0x00ff00);
        assert_eq!(pixel(format::RGB_565, &0x001fu16.to_le_bytes()), 0x0000ff);
        // 0b10000 → 0b10000100, 0b100000 → 0b10000010.
        assert_eq!(pixel(format::RGB_565, &0x8410u16.to_le_bytes()), 0x848284);
    }

    #[test]
    fn surfaces_read_their_layout() {
        // A 2x2 pitch surface of RGBA 8888, 12 bytes a row.
        let pitch = Surface {
            width: 2,
            height: 2,
            format: format::RGBA_8888,
            layout: layout::PITCH,
            pitch: 12,
            block_height_log2: 0,
        };
        assert_eq!(pitch.size(), Ok(20));
        let mut m = vec![0u8; 20];
        m[12..16].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(pitch.frame(&m).pixels, vec![0, 0, 0x010203, 0]);
        // A 32x9 block-linear RGB 565 surface: one GOB across, two blocks
        // of one GOB down.
        let bl = Surface {
            width: 32,
            height: 9,
            format: format::RGB_565,
            layout: layout::BLOCK_LINEAR,
            pitch: 64,
            block_height_log2: 0,
        };
        assert_eq!(bl.size(), Ok(64 * 16));
        let mut m = vec![0u8; 1024];
        // Pixel (17, 8) is byte 34 of row 8: the second block's sector 8,
        // two bytes in.
        m[512 + 256 + 2..512 + 256 + 4].copy_from_slice(&0xffffu16.to_le_bytes());
        let f = bl.frame(&m);
        assert_eq!((f.width, f.height), (32, 9));
        let lit: Vec<usize> = (0..f.pixels.len()).filter(|&i| f.pixels[i] != 0).collect();
        assert_eq!(lit, vec![8 * 32 + 17]);
        // Surfaces kestrel cannot read.
        let bad = |s: Surface| s.size().unwrap_err();
        assert!(bad(Surface { format: 7, ..bl }).contains("pixel format 7"));
        assert!(bad(Surface { layout: 2, ..bl }).contains("surface layout 2"));
        assert!(bad(Surface { pitch: 48, ..bl }).contains("pitch of 48"));
        assert!(bad(Surface { width: 40, ..bl }).contains("40x9"));
    }
}
