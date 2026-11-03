//! Maxwell shader programs (phase 5B): the program header, the decoder
//! into kestrel's shader IR, and the translation of that IR to SPIR-V for
//! the host GPU (PLAN.md, phase 5: one IR, a Maxwell decoder first).
//!
//! A program in GPU memory is an 80-byte shader program header (SPH), then
//! instructions of 64 bits in groups of four: a scheduling word (stalls,
//! barriers; nothing to emulate) and three instructions.
//!
//! Sources: NVIDIA's "Shader Program Header Specification" (open-gpu-doc,
//! MIT) for the header; envytools' Maxwell disassembler tables
//! (`envydis/gm107.c`, MIT) for the instruction encodings; Mesa's nouveau
//! code generator (`nv50_ir_emit_gm107.cpp`, `nv50_ir_ra.cpp`, MIT) and
//! NAK (`nak_private.h`, `nak/from_nir.rs`, MIT) for what the fields mean:
//! the attribute addresses, how TEXS spreads its results over register
//! pairs, where a pixel shader leaves its outputs.

pub mod ir;
pub mod maxwell;
pub mod spirv;

pub use ir::{Interp, Op, Operand, Program};

/// A pipeline stage kestrel translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    Vertex,
    Fragment,
}

/// What kestrel reads of a shader program header ("Shader Program Header
/// Specification": CommonWord0's SphType (4:0) is 1 for VTG programs, 2
/// for pixel programs, ShaderType (13:10) 1 VERTEX ... 5 PIXEL; a pixel
/// program's OmapTarget[8], 4 bits each from bit 576, says which
/// components of each colour target it writes, OmapSampleMask (bit 608)
/// and OmapDepth (609) whether it writes those).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Header {
    pub stage: Stage,
    pub omap_targets: u32,
    pub writes_depth: bool,
    pub writes_sample_mask: bool,
    pub kills_pixels: bool,
}

/// The header's size in bytes.
pub const HEADER_BYTES: usize = 80;

impl Header {
    pub fn parse(words: &[u32]) -> Result<Header, String> {
        if words.len() < HEADER_BYTES / 4 {
            return Err("a shader program header shorter than 80 bytes".into());
        }
        let w0 = words[0];
        let (sph_type, shader_type) = (w0 & 0x1f, w0 >> 10 & 0xf);
        let stage = match (sph_type, shader_type) {
            (1, 1) => Stage::Vertex,
            (2, 5) => Stage::Fragment,
            _ => {
                return Err(format!(
                    "a shader of SPH type {sph_type} and shader type {shader_type} (only vertex \
                     and pixel shaders are translated)"
                ))
            }
        };
        Ok(Header {
            stage,
            omap_targets: if stage == Stage::Fragment {
                words[18]
            } else {
                0
            },
            writes_sample_mask: stage == Stage::Fragment && words[19] & 1 == 1,
            writes_depth: stage == Stage::Fragment && words[19] >> 1 & 1 == 1,
            kills_pixels: w0 >> 15 & 1 == 1,
        })
    }
}

/// Attribute addresses (NAK's `nak_attr`, the same as the SPH's maps):
/// the vertex position, the generic vectors (16 bytes each), system
/// values.
pub mod attr {
    pub const POINT_SIZE: u16 = 0x06c;
    pub const POSITION: u16 = 0x070;
    pub const GENERIC: u16 = 0x080;
    pub const GENERIC_END: u16 = 0x280;
    pub const INSTANCE_ID: u16 = 0x2f8;
    pub const VERTEX_ID: u16 = 0x2fc;
    pub const FRONT_FACE: u16 = 0x3fc;
}
