//! Textures (phase 5B): a texture handle names a texture image control
//! block (TIC, the image) and a texture sampler control block (TSC, how it
//! is sampled), 32 bytes each, in the pools SET_TEX_HEADER_POOL_A-C and
//! SET_TEX_SAMPLER_POOL_A-C point to (`clb197.h`).
//!
//! Sources: envytools' rnndb (`graph/gm200_texture.xml` for Maxwell's
//! TIC, `graph/g80_texture.xml` for the TSC, `g80_defs.xml` for the data
//! types and component sources; MIT); deko3d (zlib) for a handle's
//! packing (`dkMakeTextureHandle`: the image in bits 19:0, the sampler
//! from bit 20) and for reading the handles from the constant buffer
//! SET_BINDLESS_TEXTURE selects.

use super::clear::Layout;
use super::surface::Image;

/// The image formats kestrel reads, by the TIC's COMPONENTS_SIZES, and
/// their element: bytes, and pixels a side (4 for block-compressed).
pub fn element(components: u32) -> Option<(u32, u32)> {
    Some(match components {
        0x01 => (16, 1), // R32_G32_B32_A32
        0x03 => (8, 1),  // R16_G16_B16_A16
        0x08 => (4, 1),  // A8B8G8R8
        0x09 => (4, 1),  // A2B10G10R10
        0x15 => (2, 1),  // B5G6R5
        0x18 => (2, 1),  // G8R8
        0x1d => (1, 1),  // R8
        0x24 => (8, 4),  // DXT1 (BC1)
        0x25 => (16, 4), // DXT23 (BC2)
        0x26 => (16, 4), // DXT45 (BC3)
        _ => return None,
    })
}

/// A texture as the host samples it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    /// The TIC's COMPONENTS_SIZES.
    pub components: u32,
    /// The data type of the first component (G80_TIC_TYPE: 2 UNORM, 7
    /// FLOAT); kestrel reads formats whose components share one.
    pub data_type: u32,
    pub srgb: bool,
    /// X, Y, Z, W's sources (G80_TIC_SOURCE: 0 ZERO, 2 R ... 5 A, 7
    /// ONE_FLOAT).
    pub swizzle: [u32; 4],
    /// The image's elements, row by row.
    pub data: Vec<u8>,
    /// TSC: MAG_FILTER and MIN_FILTER LINEAR (2), else NEAREST.
    pub mag_linear: bool,
    pub min_linear: bool,
    /// TSC: ADDRESS_U and _V (G80_TSC_WRAP: 0 WRAP, 1 MIRROR, 2
    /// CLAMP_TO_EDGE, 3 BORDER ...).
    pub wrap: [u32; 2],
}

/// A TIC entry's image (level 0, layer 0), or what kestrel does not read
/// in it.
pub fn tic_image(tic: &[u32; 8]) -> Result<(Image, u32, u32, bool, [u32; 4]), String> {
    let components = tic[0] & 0x7f;
    let (bytes, side) =
        element(components).ok_or_else(|| format!("texture format {components:#x}"))?;
    let types = [
        tic[0] >> 7 & 7,
        tic[0] >> 10 & 7,
        tic[0] >> 13 & 7,
        tic[0] >> 16 & 7,
    ];
    let data_type = types[0];
    if !matches!(data_type, 2 | 7) {
        return Err(format!("texture data type {data_type}"));
    }
    let swizzle = [
        tic[0] >> 19 & 7,
        tic[0] >> 22 & 7,
        tic[0] >> 25 & 7,
        tic[0] >> 28 & 7,
    ];
    let version = tic[2] >> 21 & 7;
    let texture_type = tic[4] >> 23 & 0xf;
    // TEXTURE_TYPE 1 is 2D, 7 2D without mipmaps.
    if !matches!(texture_type, 1 | 7) {
        return Err(format!("texture type {texture_type} (only 2D is read)"));
    }
    let width = (tic[4] & 0xffff) + 1;
    let height = (tic[5] & 0xffff) + 1;
    let high = ((tic[2] & 0xffff) as u64) << 32;
    let srgb = tic[4] >> 22 & 1 == 1;
    let (address, layout) = match version {
        // PITCH: the address in 31:5, the pitch's bits 20:5 in word 3.
        2 => (
            high | (tic[1] & !0x1f) as u64,
            Layout::Pitch((tic[3] & 0xffff) << 5),
        ),
        // BLOCKLINEAR: the address in 31:9, GOBS_PER_BLOCK_WIDTH,
        // _HEIGHT and _DEPTH in word 3.
        3 => {
            if tic[3] >> 13 & 1 == 1 {
                return Err("a texture with 3D GOBs".into());
            }
            (
                high | (tic[1] & !0x1ff) as u64,
                Layout::Blocks([tic[3] & 7, tic[3] >> 3 & 7, tic[3] >> 6 & 7]),
            )
        }
        v => return Err(format!("texture header version {v}")),
    };
    let (w, h) = (width.div_ceil(side), height.div_ceil(side));
    Ok((
        Image {
            address,
            width: w,
            height: h,
            bytes,
            layout,
            surface_height: h,
            z: 0,
        },
        width,
        height,
        srgb,
        swizzle,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_linear_bc1_header_reads() {
        // DXT1 (0x24), UNORM components (2), sources R G B ONE_FLOAT; the
        // address 0x12_3456_7a00; BLOCKLINEAR (3); blocks 4 GOBs high (2);
        // 2D (1); 256x128.
        let tic = [
            0x24 | 2 << 7 | 2 << 10 | 2 << 13 | 2 << 16 | 2 << 19 | 3 << 22 | 4 << 25 | 7 << 28,
            0x3456_7a00,
            0x12 | 3 << 21,
            2 << 3,
            255 | 1 << 23,
            127,
            0,
            0,
        ];
        let (img, w, h, srgb, swizzle) = tic_image(&tic).unwrap();
        assert_eq!((w, h, srgb, swizzle), (256, 128, false, [2, 3, 4, 7]));
        assert_eq!(
            img,
            Image {
                address: 0x12_3456_7a00,
                width: 64,
                height: 32,
                bytes: 8,
                layout: Layout::Blocks([0, 2, 0]),
                surface_height: 32,
                z: 0,
            }
        );
        let mut bad = tic;
        bad[0] = (bad[0] & !0x7f) | 0x40;
        assert!(tic_image(&bad).unwrap_err().contains("texture format 0x40"));
    }
}
