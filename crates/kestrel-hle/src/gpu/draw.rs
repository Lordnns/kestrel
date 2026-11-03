//! Draws (phase 5B): BEGIN, a vertex or index count, END; at END the 3D
//! engine's state becomes one `Draw`, a self-contained description the
//! host's renderer (kestrel-cli's Vulkan renderer) runs: the shaders as
//! SPIR-V, the vertices, the constant buffers and textures the shaders
//! read, the fixed-function state, and the render targets' pixels, which
//! the renderer updates and kestrel writes back to guest memory.
//!
//! Sources: NVIDIA's `clb197.h` (MIT) for the methods and fields; deko3d
//! (zlib) for how a program drives them (its draw macros in `draw.mme`:
//! BEGIN with the primitive, DRAW_VERTEX_ARRAY or DRAW_INDEX_BUFFER with
//! the count, END; one BEGIN/END pair per instance, BEGIN's INSTANCE_ID
//! saying FIRST or SUBSEQUENT; its viewport set as scale and offset).

use std::collections::BTreeMap;

use super::clear::{self, ColorTarget, Layout};
use super::shader::spirv::{self, AttrType};
use super::shader::{maxwell, Program, Stage};
use super::surface::Image;
use super::texture::{self, Texture};
use super::{class, Engine};
use crate::service::IpcError;
use crate::Process;

/// Runs draws on the host's GPU.
pub trait Renderer: Send {
    /// Run `draw`, updating its colour and depth attachments' contents.
    fn draw(&mut self, draw: &mut Draw) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topology {
    Points,
    Lines,
    LineStrip,
    Triangles,
    TriangleStrip,
    TriangleFan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vertices {
    /// `count` vertices from `first`.
    Array { first: u32, count: u32 },
    /// The index buffer's indices, widened to 32 bits.
    Indexed(Vec<u32>),
}

/// A vertex stream's bytes (from its start to its limit) and stride.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexBuffer {
    pub stride: u32,
    pub per_instance: bool,
    pub data: Vec<u8>,
}

/// A vertex attribute: SET_VERTEX_ATTRIBUTE_A's COMPONENT_BIT_WIDTHS and
/// NUMERICAL_TYPE codes, from a byte offset in a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexAttribute {
    pub location: u32,
    pub buffer: u32,
    pub offset: u32,
    pub widths: u32,
    pub numerical: u32,
}

/// A Vulkan-style viewport: x, y, width, height (negative to flip) and
/// the depth range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

/// Comparison functions, in Vulkan's order (never, less, equal, less or
/// equal, greater, not equal, greater or equal, always).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compare(pub u32);

/// A colour attachment: its SET_COLOR_TARGET_FORMAT code, the channels
/// written (bit 0 R ... bit 3 A), its pixels row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorAttachment {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub write_mask: u8,
    pub pixels: Vec<u8>,
}

/// The depth attachment, as floats (kestrel converts its format).
#[derive(Clone, Debug, PartialEq)]
pub struct DepthAttachment {
    pub width: u32,
    pub height: u32,
    pub values: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Draw {
    pub vertex_spirv: Vec<u32>,
    pub fragment_spirv: Vec<u32>,
    pub topology: Topology,
    pub vertices: Vertices,
    pub instance: u32,
    pub buffers: Vec<VertexBuffer>,
    pub attributes: Vec<VertexAttribute>,
    /// Uniform buffers by binding, each up to 64 KiB.
    pub uniforms: Vec<(u32, Vec<u8>)>,
    pub textures: Vec<(u32, Texture)>,
    pub viewport: Viewport,
    /// x, y, width, height.
    pub scissor: [u32; 4],
    /// Cull front faces (bit 0), back faces (bit 1).
    pub cull: u32,
    pub front_ccw: bool,
    pub depth_test: Option<Compare>,
    pub depth_write: bool,
    /// By fragment output location.
    pub colors: Vec<Option<ColorAttachment>>,
    pub depth: Option<DepthAttachment>,
}

/// The 3D methods read here, by name (`methods.rs`).
fn reg(e: &Engine, name: &str, index: u32) -> u32 {
    let m = super::methods::THREED
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("3D method {name}"));
    e.reg(m.base + index * m.stride)
}

fn float(v: u32) -> f32 {
    f32::from_bits(v)
}

/// A depth target (SET_ZT_*): its image and format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepthTarget {
    pub image: Image,
    pub format: u32,
}

/// The depth formats kestrel reads (SET_ZT_FORMAT) and their bytes: Z16,
/// Z24S8 (depth in 31:8, NVIDIA naming channels from the high bits;
/// Mesa's nil maps it to S8_UINT_Z24_UNORM), X8Z24 and S8Z24 (depth in
/// 23:0), ZF32, ZF32_X24S8.
fn depth_bytes(format: u32) -> Option<u32> {
    Some(match format {
        0x13 => 2,
        0x14..=0x16 | 0x0a => 4,
        0x19 => 8,
        _ => return None,
    })
}

/// A depth pixel's value as a float (0..1 for the unorm formats).
pub fn depth_value(format: u32, px: &[u8]) -> f32 {
    let w = u32::from_le_bytes([
        px[0],
        px[1],
        px.get(2).copied().unwrap_or(0),
        px.get(3).copied().unwrap_or(0),
    ]);
    match format {
        0x13 => (w & 0xffff) as f32 / 65535.0,
        0x14 => ((w >> 8) as f64 / 16_777_215.0) as f32,
        0x15 | 0x16 => ((w & 0xff_ffff) as f64 / 16_777_215.0) as f32,
        _ => f32::from_bits(w),
    }
}

/// Write depth `v` into a pixel, keeping its stencil bits.
pub fn set_depth(format: u32, px: &mut [u8], v: f32) {
    let unorm = |bits: u32| {
        let max = ((1u64 << bits) - 1) as f64;
        (v.clamp(0.0, 1.0) as f64 * max).round() as u32
    };
    let mut w = u32::from_le_bytes([
        px[0],
        px[1],
        px.get(2).copied().unwrap_or(0),
        px.get(3).copied().unwrap_or(0),
    ]);
    match format {
        0x13 => w = unorm(16),
        0x14 => w = (w & 0xff) | unorm(24) << 8,
        0x15 | 0x16 => w = (w & 0xff00_0000) | unorm(24),
        _ => w = v.to_bits(),
    }
    let n = px.len().min(4);
    px[..n].copy_from_slice(&w.to_le_bytes()[..n]);
}

/// Write stencil `s` into a pixel, `mask` bits of it.
pub fn set_stencil(format: u32, px: &mut [u8], s: u8, mask: u8) -> bool {
    let at = match format {
        0x14 => 0,
        0x16 => 3,
        0x19 => 4,
        _ => return false,
    };
    px[at] = (px[at] & !mask) | (s & mask);
    true
}

/// The depth target, if SET_ZT_SELECT enables one.
pub fn depth_target(e: &Engine) -> Result<Option<DepthTarget>, String> {
    if reg(e, "SET_ZT_SELECT", 0) & 1 == 0 {
        return Ok(None);
    }
    let format = reg(e, "SET_ZT_FORMAT", 0) & 0x1f;
    let bytes = depth_bytes(format).ok_or_else(|| format!("depth format {format:#x}"))?;
    let size = reg(e, "SET_ZT_BLOCK_SIZE", 0);
    let log2 = [size & 0xf, size >> 4 & 0xf, size >> 8 & 0xf];
    if log2.iter().any(|&l| l > 5) {
        return Err(format!("a depth block size of {size:#x}"));
    }
    let width = reg(e, "SET_ZT_SIZE_A", 0) & 0xfff_ffff;
    let height = reg(e, "SET_ZT_SIZE_B", 0) & 0x1_ffff;
    let layer = reg(e, "SET_ZT_LAYER", 0) & 0xffff;
    let pitch = reg(e, "SET_ZT_ARRAY_PITCH", 0) as u64 * 4;
    let address = ((reg(e, "SET_ZT_A", 0) & 0xff) as u64) << 32 | reg(e, "SET_ZT_B", 0) as u64;
    Ok(Some(DepthTarget {
        image: Image {
            address: address + layer as u64 * pitch,
            width,
            height,
            bytes,
            layout: Layout::Blocks(log2),
            surface_height: height,
            z: 0,
        },
        format,
    }))
}

impl ColorTarget {
    /// The image of layer `layer`.
    pub fn image(&self, layer: u32) -> Image {
        let (address, z) = if self.depth {
            (self.address, layer)
        } else {
            (self.address + layer as u64 * self.array_pitch, 0)
        };
        Image {
            address,
            width: self.width,
            height: self.height,
            bytes: self.format.bytes as u32,
            layout: self.layout,
            surface_height: self.height,
            z,
        }
    }
}

/// The draws a BEGIN ... END pair holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub begin: Option<u32>,
    pub instance: u32,
    /// (indexed, first, count) for each count method.
    pub draws: Vec<(bool, u32, u32)>,
}

/// Decoded shader programs, by GPU address: the words they were decoded
/// from (compared on each use) and the program.
pub type ProgramCache = BTreeMap<u64, (Vec<u32>, Program)>;

/// Largest program kestrel reads.
const PROGRAM_BYTES: usize = 0x1_0000;

impl Process {
    /// BEGIN, DRAW_VERTEX_ARRAY, DRAW_INDEX_BUFFER, END (`clb197.h`).
    pub(super) fn draw_method(&mut self, fd: u32, name: &str, value: u32) -> Result<(), IpcError> {
        let named = |what: &str| {
            IpcError::Unimplemented(format!("GPU 3D method {name} = {value:#x}: {what}"))
        };
        let e = self.engine3d(fd);
        match name {
            // BEGIN: OP (15:0) the primitive; INSTANCE_ID (27:26) FIRST
            // (0) starts at instance 0, SUBSEQUENT (1) the next, UNCHANGED
            // (2) the same.
            "BEGIN" => {
                if e.pending.begin.is_some() {
                    return Err(named("a BEGIN inside a BEGIN"));
                }
                match value >> 26 & 3 {
                    0 => e.pending.instance = 0,
                    1 => e.pending.instance += 1,
                    2 => {}
                    _ => return Err(named("instance mode 3")),
                }
                e.pending.begin = Some(value);
                Ok(())
            }
            // The counts: from SET_VERTEX_ARRAY_START, or from
            // SET_INDEX_BUFFER_F's FIRST.
            "DRAW_VERTEX_ARRAY" | "DRAW_INDEX_BUFFER" => {
                if e.pending.begin.is_none() {
                    return Err(named("a count outside BEGIN and END"));
                }
                let indexed = name == "DRAW_INDEX_BUFFER";
                let first = if indexed {
                    reg(e, "SET_INDEX_BUFFER_F", 0)
                } else {
                    reg(e, "SET_VERTEX_ARRAY_START", 0)
                };
                e.pending.draws.push((indexed, first, value));
                Ok(())
            }
            "END" => {
                let Some(begin) = e.pending.begin.take() else {
                    return Err(named("an END without BEGIN"));
                };
                let draws = std::mem::take(&mut e.pending.draws);
                for (indexed, first, count) in draws {
                    if count > 0 {
                        self.draw(fd, begin, indexed, first, count)?;
                    }
                }
                Ok(())
            }
            _ => Err(named("not a draw method")),
        }
    }

    fn engine3d(&mut self, fd: u32) -> &mut Engine {
        self.channel_state(fd)
            .engines
            .get_mut(&class::THREED)
            .expect("bound engines exist")
    }

    /// The program at `address`, decoded (cached by its words).
    fn program(&mut self, fd: u32, address: u64) -> Result<Program, String> {
        if let Some((words, p)) = self.channel_state(fd).programs.get(&address).cloned() {
            let now = self
                .gpu_read_bytes(fd, address, words.len() * 4)
                .map_err(|e| format!("{e:?}"))?;
            if now
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .eq(words.iter().copied())
            {
                return Ok(p);
            }
        }
        let mut len = 0x400;
        loop {
            let bytes = self
                .gpu_read_bytes(fd, address, len)
                .map_err(|e| format!("{e:?}"))?;
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            match maxwell::decode(&words) {
                Ok(p) => {
                    // Keep the words up to the program's end for the
                    // comparison: the header and the instructions read.
                    let used = (super::shader::HEADER_BYTES / 4) + program_words(&words);
                    let used = used.min(words.len());
                    self.channel_state(fd)
                        .programs
                        .insert(address, (words[..used].to_vec(), p.clone()));
                    return Ok(p);
                }
                Err(e) if e.contains("runs past its end") && len < PROGRAM_BYTES => len *= 4,
                Err(e) => return Err(e),
            }
        }
    }

    /// One draw of `count` vertices or indices from `first`.
    fn draw(
        &mut self,
        fd: u32,
        begin: u32,
        indexed: bool,
        first: u32,
        count: u32,
    ) -> Result<(), IpcError> {
        let named = |what: String| IpcError::Unimplemented(format!("a GPU draw: {what}"));
        let e = self.engine3d(fd).clone();
        let op = begin & 0xffff;
        let topology = match op {
            0 => Topology::Points,
            1 => Topology::Lines,
            3 => Topology::LineStrip,
            4 | 7 | 8 => Topology::Triangles,
            5 => Topology::TriangleStrip,
            6 | 9 => Topology::TriangleFan,
            _ => return Err(named(format!("primitive {op}"))),
        };
        // The shaders: pipeline slots 1 (VERTEX) and 5 (PIXEL) enabled,
        // the others not; SET_PIPELINE_PROGRAM offsets from
        // SET_PROGRAM_REGION_A-B.
        let region = ((reg(&e, "SET_PROGRAM_REGION_A", 0) & 0xff) as u64) << 32
            | reg(&e, "SET_PROGRAM_REGION_B", 0) as u64;
        for slot in 0..6 {
            let v = reg(&e, "SET_PIPELINE_SHADER", slot);
            let on = v & 1 == 1;
            let want = matches!(slot, 1 | 5);
            if on != want || (on && v >> 4 & 0xf != slot) {
                return Err(named(format!(
                    "pipeline slot {slot} = {v:#x} (only a vertex and a pixel shader run)"
                )));
            }
        }
        let address = |slot: u32| region + reg(&e, "SET_PIPELINE_PROGRAM", slot) as u64;
        let vs = self
            .program(fd, address(1))
            .map_err(|w| named(format!("vertex shader: {w}")))?;
        let fs = self
            .program(fd, address(5))
            .map_err(|w| named(format!("pixel shader: {w}")))?;
        if vs.header.stage != Stage::Vertex || fs.header.stage != Stage::Fragment {
            return Err(named("shader stages that do not match their slots".into()));
        }
        // Vertex attributes (SET_VERTEX_ATTRIBUTE_A: STREAM 4:0, SOURCE 6
        // (1 inactive), OFFSET 20:7, COMPONENT_BIT_WIDTHS 26:21,
        // NUMERICAL_TYPE 29:27, SWAP_R_AND_B 31).
        let mut types = [AttrType::Float; 32];
        for (loc, t) in types.iter_mut().enumerate().take(16) {
            let a = reg(&e, "SET_VERTEX_ATTRIBUTE_A", loc as u32);
            *t = match a >> 27 & 7 {
                3 => AttrType::Sint,
                4 => AttrType::Uint,
                _ => AttrType::Float,
            };
        }
        let z_minus_one = reg(&e, "SET_VIEWPORT_Z_CLIP", 0) & 1 == 0;
        let vopts = spirv::Options {
            vertex_inputs: types,
            z_minus_one_to_one: z_minus_one,
            binding_base: 0,
        };
        let fopts = spirv::Options {
            vertex_inputs: types,
            z_minus_one_to_one: false,
            binding_base: 64,
        };
        let vm = spirv::emit(&vs, &vopts).map_err(|w| named(format!("vertex shader: {w}")))?;
        let fm = spirv::emit(&fs, &fopts).map_err(|w| named(format!("pixel shader: {w}")))?;

        // Vertex streams and attributes the shader reads.
        let mut buffers = Vec::new();
        let mut attributes = Vec::new();
        let mut stream_buffer: BTreeMap<u32, u32> = BTreeMap::new();
        for loc in 0..16u32 {
            if vm.inputs >> loc & 1 == 0 {
                continue;
            }
            let a = reg(&e, "SET_VERTEX_ATTRIBUTE_A", loc);
            if a >> 6 & 1 == 1 {
                return Err(named(format!(
                    "vertex attribute {loc} is inactive (a constant)"
                )));
            }
            if a >> 31 == 1 {
                return Err(named(format!("vertex attribute {loc} swaps R and B")));
            }
            let stream = a & 0x1f;
            let buffer = match stream_buffer.get(&stream) {
                Some(&b) => b,
                None => {
                    // SET_VERTEX_STREAM_A_FORMAT: STRIDE 11:0, ENABLE 12;
                    // LOCATION_A-B the address, LIMIT_A_A-B the last byte;
                    // SET_VERTEX_STREAM_INSTANCE_A IS_INSTANCED.
                    let format = reg(&e, "SET_VERTEX_STREAM_A_FORMAT", stream);
                    if format >> 12 & 1 == 0 {
                        return Err(named(format!("vertex stream {stream} is disabled")));
                    }
                    let start = ((reg(&e, "SET_VERTEX_STREAM_A_LOCATION_A", stream) & 0xff) as u64)
                        << 32
                        | reg(&e, "SET_VERTEX_STREAM_A_LOCATION_B", stream) as u64;
                    let limit = ((reg(&e, "SET_VERTEX_STREAM_LIMIT_A_A", stream) & 0xff) as u64)
                        << 32
                        | reg(&e, "SET_VERTEX_STREAM_LIMIT_A_B", stream) as u64;
                    if limit < start || limit - start >= 0x1000_0000 {
                        return Err(named(format!(
                            "vertex stream {stream} from {start:#x} to {limit:#x}"
                        )));
                    }
                    let data = self.gpu_read_bytes(fd, start, (limit - start + 1) as usize)?;
                    let per_instance =
                        stream < 16 && reg(&e, "SET_VERTEX_STREAM_INSTANCE_A", stream) & 1 == 1;
                    if per_instance && reg(&e, "SET_VERTEX_STREAM_A_FREQUENCY", stream) > 1 {
                        return Err(named(format!("vertex stream {stream}'s instance divisor")));
                    }
                    buffers.push(VertexBuffer {
                        stride: format & 0xfff,
                        per_instance,
                        data,
                    });
                    let b = buffers.len() as u32 - 1;
                    stream_buffer.insert(stream, b);
                    b
                }
            };
            attributes.push(VertexAttribute {
                location: loc,
                buffer,
                offset: a >> 7 & 0x3fff,
                widths: a >> 21 & 0x3f,
                numerical: a >> 27 & 7,
            });
        }
        let vertices = if indexed {
            // SET_INDEX_BUFFER_A-B the address, E's INDEX_SIZE (1, 2, 4
            // bytes); the base vertex (SET_VERTEX_ID_BASE) is not added
            // yet.
            if reg(&e, "SET_VERTEX_ID_BASE", 0) != 0 {
                return Err(named("a base vertex".into()));
            }
            let address = ((reg(&e, "SET_INDEX_BUFFER_A", 0) & 0xff) as u64) << 32
                | reg(&e, "SET_INDEX_BUFFER_B", 0) as u64;
            let size = match reg(&e, "SET_INDEX_BUFFER_E", 0) & 3 {
                0 => 1,
                1 => 2,
                2 => 4,
                s => return Err(named(format!("index size {s}"))),
            };
            let bytes = self.gpu_read_bytes(
                fd,
                address + first as u64 * size as u64,
                count as usize * size,
            )?;
            Vertices::Indexed(
                bytes
                    .chunks_exact(size)
                    .map(|c| {
                        let mut w = [0u8; 4];
                        w[..size].copy_from_slice(c);
                        u32::from_le_bytes(w)
                    })
                    .collect(),
            )
        } else {
            Vertices::Array { first, count }
        };
        // Quads, quad strips and polygons, which the host does not draw:
        // quads as two triangles each (0 1 2, 0 2 3), a strip's quads
        // likewise, a polygon as a fan (kestrel's choice of split; flat
        // shading then takes each triangle's last vertex).
        let vertices = if matches!(op, 7 | 8) {
            let ix: Vec<u32> = match vertices {
                Vertices::Indexed(ix) => ix,
                Vertices::Array { first, count } => (first..first + count).collect(),
            };
            let quads: Vec<[u32; 4]> = if op == 7 {
                ix.chunks_exact(4)
                    .map(|q| [q[0], q[1], q[2], q[3]])
                    .collect()
            } else {
                ix.windows(4)
                    .step_by(2)
                    .map(|q| [q[0], q[1], q[3], q[2]])
                    .collect()
            };
            Vertices::Indexed(
                quads
                    .iter()
                    .flat_map(|q| [q[0], q[1], q[2], q[0], q[2], q[3]])
                    .collect(),
            )
        } else {
            vertices
        };

        // Constant buffers and textures, by stage: SET_PIPELINE_BINDING's
        // GROUP picks the BIND_GROUP_CONSTANT_BUFFER set.
        let mut uniforms = Vec::new();
        let mut textures = Vec::new();
        for (slot, m) in [(1u32, &vm), (5, &fm)] {
            let group = reg(&e, "SET_PIPELINE_BINDING", slot) & 7;
            let cbuf = |p: &Process, index: u8| -> Result<Vec<u8>, IpcError> {
                let Some(&(address, size)) = e.constant_buffers.get(&(group, index as u32)) else {
                    return Err(named(format!(
                        "constant buffer {index} of group {group} is not bound"
                    )));
                };
                p.gpu_read_bytes(fd, address, size.min(spirv::UNIFORM_BYTES) as usize)
            };
            for &(index, binding) in &m.uniform_buffers {
                uniforms.push((binding, cbuf(self, index)?));
            }
            if m.textures.is_empty() {
                continue;
            }
            // SET_BINDLESS_TEXTURE's CONSTANT_BUFFER_SLOT_SELECT holds the
            // handles.
            let slot_cb = (reg(&e, "SET_BINDLESS_TEXTURE", 0) & 0x1f) as u8;
            let handles = cbuf(self, slot_cb)?;
            for &(word, binding) in &m.textures {
                let at = word as usize * 4;
                let Some(h) = handles.get(at..at + 4) else {
                    return Err(named(format!(
                        "texture handle word {word:#x} past its constant buffer"
                    )));
                };
                let h = u32::from_le_bytes(h.try_into().unwrap());
                let t = self.texture(fd, &e, h).map_err(&named)?;
                textures.push((binding, t));
            }
        }

        // Fixed function state.
        let scale = [0, 1, 2].map(|k| {
            float(reg(
                &e,
                [
                    "SET_VIEWPORT_SCALE_X",
                    "SET_VIEWPORT_SCALE_Y",
                    "SET_VIEWPORT_SCALE_Z",
                ][k],
                0,
            ))
        });
        let offset = [0, 1, 2].map(|k| {
            float(reg(
                &e,
                [
                    "SET_VIEWPORT_OFFSET_X",
                    "SET_VIEWPORT_OFFSET_Y",
                    "SET_VIEWPORT_OFFSET_Z",
                ][k],
                0,
            ))
        });
        if reg(&e, "SET_VIEWPORT_SCALE_OFFSET", 0) & 1 == 0 {
            return Err(named("the viewport transform is off".into()));
        }
        if reg(&e, "SET_WINDOW_ORIGIN", 0) & 0x11 != 0 {
            return Err(named("a lower-left window origin or a flipped Y".into()));
        }
        if scale[0] < 0.0 {
            return Err(named("a negative viewport X scale".into()));
        }
        let (min_depth, max_depth) = if z_minus_one {
            (offset[2] - scale[2], offset[2] + scale[2])
        } else {
            (offset[2], offset[2] + scale[2])
        };
        if !(0.0..=1.0).contains(&min_depth) || !(0.0..=1.0).contains(&max_depth) {
            return Err(named(format!(
                "a depth range of {min_depth} to {max_depth}"
            )));
        }
        let viewport = Viewport {
            x: offset[0] - scale[0],
            y: offset[1] - scale[1],
            width: 2.0 * scale[0],
            height: 2.0 * scale[1],
            min_depth,
            max_depth,
        };
        for name in ["SET_FRONT_POLYGON_MODE", "SET_BACK_POLYGON_MODE"] {
            let mode = reg(&e, name, 0);
            if mode != 0 && mode != 0x1b02 {
                return Err(named(format!("{name} = {mode:#x} (only FILL)")));
            }
        }
        // OGL_SET_CULL, OGL_SET_CULL_FACE (FRONT 0x404, BACK 0x405,
        // FRONT_AND_BACK 0x408), OGL_SET_FRONT_FACE (CW 0x900, CCW 0x901).
        let cull = if reg(&e, "OGL_SET_CULL", 0) & 1 == 1 {
            match reg(&e, "OGL_SET_CULL_FACE", 0) {
                0x404 => 1,
                0x405 => 2,
                0x408 => 3,
                v => return Err(named(format!("cull face {v:#x}"))),
            }
        } else {
            0
        };
        let front_ccw = match reg(&e, "OGL_SET_FRONT_FACE", 0) {
            0x900 => false,
            0x901 | 0 => true,
            v => return Err(named(format!("front face {v:#x}"))),
        };
        if reg(&e, "SET_STENCIL_TEST", 0) & 1 == 1 {
            return Err(named("a stencil test".into()));
        }
        if reg(&e, "SET_ALPHA_TEST", 0) & 1 == 1 {
            return Err(named("an alpha test".into()));
        }
        // SET_DEPTH_FUNC: OpenGL's (0x200 + n) or Direct3D's (1 + n), n in
        // Vulkan's order.
        let depth_test = if reg(&e, "SET_DEPTH_TEST", 0) & 1 == 1 {
            let f = reg(&e, "SET_DEPTH_FUNC", 0);
            Some(Compare(match f {
                0x200..=0x207 => f - 0x200,
                1..=8 => f - 1,
                _ => return Err(named(format!("depth function {f:#x}"))),
            }))
        } else {
            None
        };
        let depth_write = reg(&e, "SET_DEPTH_WRITE", 0) & 1 == 1;

        // Colour targets: the pixel shader's outputs t go to the target
        // SET_CT_SELECT's TARGETt names; SET_CT_WRITE(t) masks them.
        let ct = reg(&e, "SET_CT_SELECT", 0);
        let single = reg(&e, "SET_SINGLE_CT_WRITE_CONTROL", 0) & 1 == 1;
        let mut colors = Vec::new();
        let mut targets = Vec::new();
        for t in 0..8u32 {
            if fs.header.omap_targets >> (4 * t) & 0xf == 0 || t >= (ct & 0xf) {
                colors.push(None);
                continue;
            }
            if reg(&e, "SET_BLEND", t) & 1 == 1 {
                return Err(named(format!("blending on target {t}")));
            }
            let n = ct >> (4 + 3 * t) & 7;
            let Some(target) = clear::color_target(&e, n).map_err(&named)? else {
                colors.push(None);
                continue;
            };
            let w = reg(&e, "SET_CT_WRITE", if single { 0 } else { t });
            let mask = (w & 1 | (w >> 3 & 2) | (w >> 6 & 4) | (w >> 9 & 8)) as u8;
            let layer = target.first_layer;
            let image = target.image(layer);
            let (pixels, kept) = self.read_image_kept(fd, &image)?;
            colors.push(Some(ColorAttachment {
                format: reg(&e, "SET_COLOR_TARGET_FORMAT", n) & 0xff,
                width: target.width,
                height: target.height,
                write_mask: mask,
                pixels,
            }));
            targets.push((t, image, kept));
        }
        let zt = depth_target(&e).map_err(&named)?;
        let (depth, depth_raw) = match (&zt, depth_test.is_some() || depth_write) {
            (Some(z), true) => {
                let (raw, kept) = self.read_image_kept(fd, &z.image)?;
                let values: Vec<f32> = raw
                    .chunks_exact(z.image.bytes as usize)
                    .map(|px| depth_value(z.format, px))
                    .collect();
                (
                    Some(DepthAttachment {
                        width: z.image.width,
                        height: z.image.height,
                        values: values.clone(),
                    }),
                    Some((raw, kept, values)),
                )
            }
            _ => (None, None),
        };
        // The scissor: scissor 0 if enabled, within viewport 0's clip
        // (SET_VIEWPORT_CLIP_HORIZONTAL/VERTICAL: x 15:0, width 31:16)
        // and the surface clip.
        let (mut x0, mut y0, mut x1, mut y1) = (0u32, 0u32, u32::MAX, u32::MAX);
        let mut limit = |h: u32, v: u32, from_width: bool| {
            let (a, b) = (h & 0xffff, h >> 16);
            let (c, d) = (v & 0xffff, v >> 16);
            let (ax1, by1) = if from_width { (a + b, c + d) } else { (b, d) };
            x0 = x0.max(a);
            x1 = x1.min(ax1);
            y0 = y0.max(c);
            y1 = y1.min(by1);
        };
        if reg(&e, "SET_SCISSOR_ENABLE", 0) & 1 == 1 {
            limit(
                reg(&e, "SET_SCISSOR_HORIZONTAL", 0),
                reg(&e, "SET_SCISSOR_VERTICAL", 0),
                false,
            );
        }
        limit(
            reg(&e, "SET_VIEWPORT_CLIP_HORIZONTAL", 0),
            reg(&e, "SET_VIEWPORT_CLIP_VERTICAL", 0),
            true,
        );
        limit(
            reg(&e, "SET_SURFACE_CLIP_HORIZONTAL", 0),
            reg(&e, "SET_SURFACE_CLIP_VERTICAL", 0),
            true,
        );
        let scissor = [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)];

        let mut draw = Draw {
            vertex_spirv: vm.words,
            fragment_spirv: fm.words,
            topology,
            vertices,
            instance: e.pending.instance,
            buffers,
            attributes,
            uniforms,
            textures,
            viewport,
            scissor,
            cull,
            front_ccw,
            depth_test,
            depth_write,
            colors,
            depth,
        };
        let Some(mut renderer) = self.renderer.take() else {
            return Err(IpcError::Unimplemented(
                "a GPU draw, with no renderer here (kestrel draws with Vulkan, which this run \
                 does not have)"
                    .into(),
            ));
        };
        let result = renderer.draw(&mut draw);
        self.renderer = Some(renderer);
        result.map_err(|w| IpcError::Stop(format!("the host GPU failed a draw: {w}")))?;
        // Write the results back.
        for (t, image, kept) in targets {
            if let Some(c) = &draw.colors[t as usize] {
                self.write_image_over(fd, &image, kept, &c.pixels)?;
            }
        }
        if let (Some(z), Some((mut raw, kept, sent)), Some(d)) = (zt, depth_raw, &draw.depth) {
            if depth_write {
                let bytes = z.image.bytes as usize;
                for ((px, &v), &was) in raw.chunks_exact_mut(bytes).zip(&d.values).zip(&sent) {
                    // Pixels whose depth did not change keep their bits.
                    if v != was {
                        set_depth(z.format, px, v);
                    }
                }
                self.write_image_over(fd, &z.image, kept, &raw)?;
            }
        }
        Ok(())
    }

    /// The texture `handle` names: TIC in bits 19:0, TSC from bit 20.
    fn texture(&mut self, fd: u32, e: &Engine, handle: u32) -> Result<Texture, String> {
        let pool = |a: &str, b: &str| ((reg(e, a, 0) & 0xff) as u64) << 32 | reg(e, b, 0) as u64;
        let (tic_i, tsc_i) = (handle & 0xf_ffff, handle >> 20);
        if tic_i > reg(e, "SET_TEX_HEADER_POOL_C", 0) & 0x3f_ffff
            || tsc_i > reg(e, "SET_TEX_SAMPLER_POOL_C", 0) & 0xf_ffff
        {
            return Err(format!("texture handle {handle:#x} past its pools"));
        }
        let read8 = |p: &Process, at: u64| -> Result<[u32; 8], String> {
            let b = p.gpu_read_bytes(fd, at, 32).map_err(|e| format!("{e:?}"))?;
            Ok(std::array::from_fn(|i| {
                u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
            }))
        };
        let tic = read8(
            self,
            pool("SET_TEX_HEADER_POOL_A", "SET_TEX_HEADER_POOL_B") + tic_i as u64 * 32,
        )?;
        let tsc = read8(
            self,
            pool("SET_TEX_SAMPLER_POOL_A", "SET_TEX_SAMPLER_POOL_B") + tsc_i as u64 * 32,
        )?;
        let (image, width, height, srgb, swizzle) = texture::tic_image(&tic)?;
        let data = self.read_image(fd, &image).map_err(|e| format!("{e:?}"))?;
        Ok(Texture {
            width,
            height,
            components: tic[0] & 0x7f,
            data_type: tic[0] >> 7 & 7,
            srgb,
            swizzle,
            data,
            mag_linear: tsc[1] & 3 == 2,
            min_linear: tsc[1] >> 4 & 3 == 2,
            wrap: [tsc[0] & 7, tsc[0] >> 3 & 7],
        })
    }
}

/// The words a decoded program's instructions take: up to its EXIT's
/// group of four.
fn program_words(words: &[u32]) -> usize {
    let code = &words[super::shader::HEADER_BYTES / 4..];
    let mut at = 0;
    while at + 2 <= code.len() {
        let w = code[at] as u64 | (code[at + 1] as u64) << 32;
        let i = at / 2;
        at += 2;
        if i % 4 != 0 && w >> 52 == 0xe30 {
            return at.div_ceil(8) * 8;
        }
    }
    code.len()
}
