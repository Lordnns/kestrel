//! The GPU's copy engines (phase 5B): inline-to-memory, which writes data
//! carried in the command list itself to memory, and DMA copy, which copies
//! memory to memory. Both read and write pitch surfaces (rows `pitch` bytes
//! apart) and block-linear ones (`display::Blocks`).
//!
//! Sources: NVIDIA's class headers (MIT) for the methods and their fields,
//! inline-to-memory's in `cla140.h` (and the same methods at the same
//! offsets in the 3D and compute classes, `clb197.h`, `clb1c0.h`), DMA
//! copy's in `clb0b5.h`; envytools' rnndb (`graph/gk104_p2mf.xml`, MIT) for
//! the units the headers leave out (the destination's width and X origin in
//! bytes); deko3d (`source/maxwell/gpu_transfer.cpp`, zlib) for how a
//! program drives them (the DMA copy's width and X origin in bytes without
//! component remapping, its LAYER as the slice of a 3D surface, its origin
//! for block-linear surfaces only).

use super::{class, engine_name, Engine};
use crate::display::Blocks;
use crate::service::IpcError;
use crate::Process;

/// Where a transfer reads or writes, line by line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// Line `i` at `address + i * pitch`.
    Pitch { address: u64, pitch: u32 },
    /// Line `i` at row `origin[1] + i` of slice `origin[2]`, from byte
    /// `origin[0]`.
    Blocks {
        address: u64,
        blocks: Blocks,
        origin: [u32; 3],
    },
}

impl Surface {
    /// The runs of contiguous GPU memory, (address, bytes), that hold `len`
    /// bytes of line `line`, in order.
    pub fn runs(&self, line: u32, len: u32) -> Vec<(u64, usize)> {
        match *self {
            Surface::Pitch { address, pitch } => {
                vec![(address + line as u64 * pitch as u64, len as usize)]
            }
            Surface::Blocks {
                address,
                blocks,
                origin,
            } => {
                let (y, z) = ((origin[1] + line) as usize, origin[2] as usize);
                let mut runs: Vec<(u64, usize)> = Vec::new();
                let (mut x, end) = (origin[0] as usize, (origin[0] + len) as usize);
                while x < end {
                    // A GOB's bytes are contiguous 16 at a time (`display`).
                    let n = (16 - x % 16).min(end - x);
                    let at = address + blocks.offset(x, y, z) as u64;
                    match runs.last_mut() {
                        Some((a, l)) if *a + *l as u64 == at => *l += n,
                        _ => runs.push((at, n)),
                    }
                    x += n;
                }
                runs
            }
        }
    }
}

/// An inline-to-memory transfer in progress: LAUNCH_DMA set it up, the
/// LOAD_INLINE_DATA words that follow fill `data` up to `line_length *
/// lines` bytes; then it is written, and its semaphore released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    pub dst: Surface,
    pub line_length: u32,
    pub lines: u32,
    pub data: Vec<u8>,
    /// A one-word semaphore to release when done: (GPU address, payload).
    pub semaphore: Option<(u64, u32)>,
}

/// The inline-to-memory methods' offsets, the same in the 3D, compute and
/// inline-to-memory classes.
mod i2m {
    pub const LINE_LENGTH_IN: u32 = 0x180;
    pub const LINE_COUNT: u32 = 0x184;
    pub const OFFSET_OUT_UPPER: u32 = 0x188;
    pub const OFFSET_OUT: u32 = 0x18c;
    pub const PITCH_OUT: u32 = 0x190;
    pub const SET_DST_BLOCK_SIZE: u32 = 0x194;
    pub const SET_DST_WIDTH: u32 = 0x198;
    pub const SET_DST_HEIGHT: u32 = 0x19c;
    pub const SET_DST_LAYER: u32 = 0x1a4;
    pub const SET_DST_ORIGIN_BYTES_X: u32 = 0x1a8;
    pub const SET_DST_ORIGIN_SAMPLES_Y: u32 = 0x1ac;
    pub const SET_I2M_SEMAPHORE_A: u32 = 0x1dc;
    pub const SET_I2M_SEMAPHORE_B: u32 = 0x1e0;
    pub const SET_I2M_SEMAPHORE_C: u32 = 0x1e4;
}

/// DMA copy's methods (`clb0b5.h`).
mod dma {
    pub const SET_SEMAPHORE_A: u32 = 0x240;
    pub const SET_SEMAPHORE_B: u32 = 0x244;
    pub const SET_SEMAPHORE_PAYLOAD: u32 = 0x248;
    pub const OFFSET_IN_UPPER: u32 = 0x400;
    pub const OFFSET_IN_LOWER: u32 = 0x404;
    pub const OFFSET_OUT_UPPER: u32 = 0x408;
    pub const OFFSET_OUT_LOWER: u32 = 0x40c;
    pub const PITCH_IN: u32 = 0x410;
    pub const PITCH_OUT: u32 = 0x414;
    pub const LINE_LENGTH_IN: u32 = 0x418;
    pub const LINE_COUNT: u32 = 0x41c;
    pub const SET_DST_BLOCK_SIZE: u32 = 0x70c;
    pub const SET_SRC_BLOCK_SIZE: u32 = 0x728;
}

/// The largest block, 2^5 GOBs in each direction (envytools: 0-5).
const MAX_BLOCK_LOG2: u32 = 5;

/// A block size field (WIDTH 3:0, HEIGHT 7:4, DEPTH 11:8) as log2 GOBs, or
/// what it holds that kestrel does not read: QUARTER_GOB widths (0xe) and
/// sizes past 32 GOBs.
fn block_size(v: u32) -> Result<[u32; 3], String> {
    let log2 = [v & 0xf, v >> 4 & 0xf, v >> 8 & 0xf];
    if log2.iter().any(|&l| l > MAX_BLOCK_LOG2) {
        return Err(format!("a block size of {v:#x}"));
    }
    Ok(log2)
}

fn address(upper: u32, lower: u32) -> u64 {
    (upper as u64) << 32 | lower as u64
}

impl Process {
    /// The guest address behind GPU address `va` in channel `fd`'s address
    /// space, and how many bytes from there the mapping holds.
    pub(super) fn gpu_span(&self, fd: u32, va: u64) -> Result<(u64, u64), IpcError> {
        let nv = &self.services.nv;
        let space = nv
            .gpu
            .channels
            .get(&fd)
            .and_then(|c| c.address_space)
            .and_then(|a| nv.gpu.address_spaces.get(&a))
            .ok_or_else(|| IpcError::Stop("a GPU channel with no address space".into()))?;
        let m = space
            .mappings
            .iter()
            .find(|m| va >= m.start && va < m.start + m.size)
            .ok_or_else(|| {
                IpcError::Stop(format!(
                    "the GPU reading or writing at {va:#x}, which no mapping holds (a GPU page \
                     fault)"
                ))
            })?;
        let addr = nv.map(m.handle).and_then(|o| o.addr).ok_or_else(|| {
            IpcError::Stop(format!(
                "GPU address {va:#x} in nvmap object {}, which has no memory",
                m.handle
            ))
        })?;
        Ok((
            addr + m.buffer_offset + (va - m.start),
            m.start + m.size - va,
        ))
    }

    /// Read `len` bytes at GPU address `va`, across mappings.
    pub(super) fn gpu_read_bytes(&self, fd: u32, va: u64, len: usize) -> Result<Vec<u8>, IpcError> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let at = va + out.len() as u64;
            let (addr, left) = self.gpu_span(fd, at)?;
            let n = (len - out.len()).min(left as usize);
            let bytes = self
                .mem
                .read_vec(addr, n)
                .map_err(|e| IpcError::Stop(format!("the GPU reading guest memory: {e}")))?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    /// Write `bytes` at GPU address `va`, across mappings.
    pub(super) fn gpu_write_bytes(
        &mut self,
        fd: u32,
        va: u64,
        bytes: &[u8],
    ) -> Result<(), IpcError> {
        let mut done = 0;
        while done < bytes.len() {
            let (addr, left) = self.gpu_span(fd, va + done as u64)?;
            let n = (bytes.len() - done).min(left as usize);
            self.mem
                .write_bytes(addr, &bytes[done..done + n])
                .map_err(|e| IpcError::Stop(format!("the GPU writing guest memory: {e}")))?;
            done += n;
        }
        Ok(())
    }

    /// Read `len` bytes of each of `lines` lines of `src`.
    pub(super) fn read_lines(
        &self,
        fd: u32,
        src: &Surface,
        len: u32,
        lines: u32,
    ) -> Result<Vec<u8>, IpcError> {
        let mut out = Vec::with_capacity(len as usize * lines as usize);
        for line in 0..lines {
            for (va, n) in src.runs(line, len) {
                out.extend(self.gpu_read_bytes(fd, va, n)?);
            }
        }
        Ok(out)
    }

    /// Write `data` as lines of `len` bytes to `dst`.
    pub(super) fn write_lines(
        &mut self,
        fd: u32,
        dst: &Surface,
        len: u32,
        data: &[u8],
    ) -> Result<(), IpcError> {
        if len == 0 {
            return Ok(());
        }
        let mut at = 0;
        for (line, chunk) in data.chunks(len as usize).enumerate() {
            for (va, n) in dst.runs(line as u32, chunk.len() as u32) {
                self.gpu_write_bytes(fd, va, &chunk[at..at + n])?;
                at += n;
            }
            at = 0;
        }
        Ok(())
    }

    /// LAUNCH_DMA of the inline-to-memory methods on `class` (3D, compute or
    /// inline-to-memory), `value` its fields (`cla140.h`): the transfer
    /// starts, and the LOAD_INLINE_DATA words that follow carry its data.
    /// Err(Some) stops as it is, Err(None) by the method's name.
    pub(super) fn inline_launch(
        &mut self,
        fd: u32,
        class: u32,
        value: u32,
    ) -> Result<(), Option<IpcError>> {
        let named = |what: &str| {
            Some(IpcError::Unimplemented(format!(
                "GPU {} inline-to-memory LAUNCH_DMA = {value:#x}: {what}",
                engine_name(class)
            )))
        };
        let state = self.channel_state(fd);
        let e: &mut Engine = state.engines.get_mut(&class).expect("bound engines exist");
        if e.inline.is_some() {
            return Err(named("the previous transfer has not had all its data"));
        }
        // DST_MEMORY_LAYOUT (0): 0 block linear, 1 pitch.
        let base = address(e.reg(i2m::OFFSET_OUT_UPPER), e.reg(i2m::OFFSET_OUT));
        let dst = if value & 1 == 1 {
            Surface::Pitch {
                address: base,
                pitch: e.reg(i2m::PITCH_OUT),
            }
        } else {
            // The width in bytes and the X origin in bytes (envytools:
            // DST_WIDTH "is the line length", DST_X "in bytes"); the layer
            // is the slice. GM20B's GOBs are 8 rows high.
            let log2 = block_size(e.reg(i2m::SET_DST_BLOCK_SIZE)).map_err(|w| named(&w))?;
            Surface::Blocks {
                address: base,
                blocks: Blocks {
                    width: e.reg(i2m::SET_DST_WIDTH) as usize,
                    height: e.reg(i2m::SET_DST_HEIGHT) as usize,
                    log2,
                },
                origin: [
                    e.reg(i2m::SET_DST_ORIGIN_BYTES_X) & 0x1f_ffff,
                    e.reg(i2m::SET_DST_ORIGIN_SAMPLES_Y) & 0x1_ffff,
                    e.reg(i2m::SET_DST_LAYER),
                ],
            }
        };
        // REDUCTION_ENABLE (1) applies to the semaphore: not read yet.
        if value >> 1 & 1 == 1 {
            return Err(named("a semaphore reduction"));
        }
        // COMPLETION_TYPE (5:4): FLUSH_DISABLE (0) and FLUSH_ONLY (1) have
        // nothing to do (no caches); RELEASE_SEMAPHORE (2) writes the
        // payload (SET_I2M_SEMAPHORE_C) at A and B when the transfer is
        // done, if SEMAPHORE_STRUCT_SIZE (12) is ONE_WORD (1).
        let semaphore = match value >> 4 & 3 {
            0 | 1 => None,
            2 if value >> 12 & 1 == 1 => Some((
                address(
                    e.reg(i2m::SET_I2M_SEMAPHORE_A),
                    e.reg(i2m::SET_I2M_SEMAPHORE_B),
                ),
                e.reg(i2m::SET_I2M_SEMAPHORE_C),
            )),
            2 => return Err(named("a four-word semaphore")),
            _ => return Err(named("a reserved completion type")),
        };
        // INTERRUPT_TYPE (9:8) and SYSMEMBAR_DISABLE (6): an interrupt for
        // the driver and a memory barrier. kestrel's choice: nothing to do
        // (the transfer is done before the submission returns).
        let inline = Inline {
            dst,
            line_length: e.reg(i2m::LINE_LENGTH_IN),
            lines: e.reg(i2m::LINE_COUNT),
            data: Vec::new(),
            semaphore,
        };
        if inline.line_length as u64 * inline.lines as u64 == 0 {
            // No data to wait for: done at once.
            return self.inline_finish(fd, inline).map_err(Some);
        }
        self.channel_state(fd)
            .engines
            .get_mut(&class)
            .expect("bound engines exist")
            .inline = Some(inline);
        Ok(())
    }

    /// LOAD_INLINE_DATA: one word of the transfer in progress, its bytes in
    /// memory order (little-endian). The last word may carry fewer bytes
    /// than four; the rest are dropped. Err(None): no transfer in progress,
    /// stopped by the method's name.
    pub(super) fn inline_data(
        &mut self,
        fd: u32,
        class: u32,
        value: u32,
    ) -> Result<(), Option<IpcError>> {
        let state = self.channel_state(fd);
        let e = state.engines.get_mut(&class).expect("bound engines exist");
        let Some(inline) = e.inline.as_mut() else {
            return Err(None);
        };
        let total = inline.line_length as usize * inline.lines as usize;
        let n = (total - inline.data.len()).min(4);
        inline.data.extend_from_slice(&value.to_le_bytes()[..n]);
        if inline.data.len() < total {
            return Ok(());
        }
        let inline = e.inline.take().expect("a transfer in progress");
        self.inline_finish(fd, inline).map_err(Some)
    }

    fn inline_finish(&mut self, fd: u32, inline: Inline) -> Result<(), IpcError> {
        self.write_lines(fd, &inline.dst, inline.line_length, &inline.data)?;
        if let Some((va, payload)) = inline.semaphore {
            self.gpu_write_bytes(fd, va, &payload.to_le_bytes())?;
        }
        Ok(())
    }

    /// DMA copy's LAUNCH_DMA (`clb0b5.h`): copy LINE_COUNT lines of
    /// LINE_LENGTH_IN bytes from OFFSET_IN to OFFSET_OUT, each side pitch
    /// or block linear, then release the semaphore.
    pub(super) fn copy_launch(&mut self, fd: u32, value: u32) -> Result<(), IpcError> {
        let named = |what: &str| {
            IpcError::Unimplemented(format!("GPU DMA copy LAUNCH_DMA = {value:#x}: {what}"))
        };
        let state = self.channel_state(fd);
        let e = &state.engines[&class::DMA_COPY];
        let bit = |n: u32| value >> n & 1 == 1;
        // REMAP_ENABLE (10): components rearranged and sized; SRC_TYPE and
        // DST_TYPE (12, 13) PHYSICAL: addresses outside the GPU's virtual
        // memory; SEMAPHORE_REDUCTION_ENABLE (19). Not read yet.
        if bit(10) {
            return Err(named("component remapping"));
        }
        if bit(12) || bit(13) {
            return Err(named("a physical address"));
        }
        if bit(19) {
            return Err(named("a semaphore reduction"));
        }
        // SEMAPHORE_TYPE (4:3): NONE (0), RELEASE_ONE_WORD_SEMAPHORE (1)
        // writes SET_SEMAPHORE_PAYLOAD at A (UPPER 7:0) and B.
        let semaphore = match value >> 3 & 3 {
            0 => None,
            1 => Some((
                address(
                    e.reg(dma::SET_SEMAPHORE_A) & 0xff,
                    e.reg(dma::SET_SEMAPHORE_B),
                ),
                e.reg(dma::SET_SEMAPHORE_PAYLOAD),
            )),
            2 => return Err(named("a four-word semaphore")),
            _ => return Err(named("a reserved semaphore type")),
        };
        // Each side: SRC_MEMORY_LAYOUT (7), DST_MEMORY_LAYOUT (8), 0 block
        // linear, 1 pitch. A block-linear side is described by its
        // SET_*_BLOCK_SIZE, WIDTH (bytes), HEIGHT, DEPTH, LAYER (the slice)
        // and ORIGIN (X bytes 15:0, Y 31:16), five words from the block
        // size; GOB_HEIGHT (15:12) must be FERMI_8 (1), GM20B's.
        let side = |pitch_layout: bool, upper: u32, lower: u32, pitch: u32, block: u32| {
            let address = address(e.reg(upper) & 0xff, e.reg(lower));
            if pitch_layout {
                return Ok(Surface::Pitch {
                    address,
                    pitch: e.reg(pitch),
                });
            }
            let size = e.reg(block);
            if size >> 12 & 0xf != 1 {
                return Err(named(&format!(
                    "a GOB height field of {}",
                    size >> 12 & 0xf
                )));
            }
            let log2 = block_size(size & 0xfff).map_err(|w| named(&w))?;
            let origin = e.reg(block + 0x14);
            Ok(Surface::Blocks {
                address,
                blocks: Blocks {
                    width: e.reg(block + 4) as usize,
                    height: e.reg(block + 8) as usize,
                    log2,
                },
                origin: [origin & 0xffff, origin >> 16, e.reg(block + 0x10)],
            })
        };
        let src = side(
            bit(7),
            dma::OFFSET_IN_UPPER,
            dma::OFFSET_IN_LOWER,
            dma::PITCH_IN,
            dma::SET_SRC_BLOCK_SIZE,
        )?;
        let dst = side(
            bit(8),
            dma::OFFSET_OUT_UPPER,
            dma::OFFSET_OUT_LOWER,
            dma::PITCH_OUT,
            dma::SET_DST_BLOCK_SIZE,
        )?;
        // MULTI_LINE_ENABLE (9): without it, one line (kestrel's reading:
        // LINE_COUNT is not used). DATA_TRANSFER_TYPE (1:0) NONE (0) copies
        // nothing; PIPELINED and NON_PIPELINED (1, 2) differ in overlap
        // with the previous transfer, which is done. FLUSH_ENABLE (2),
        // INTERRUPT_TYPE (6:5), FORCE_RMWDISABLE (11), BYPASS_L2 (20):
        // kestrel's choice, nothing to do.
        let len = e.reg(dma::LINE_LENGTH_IN);
        let lines = if bit(9) { e.reg(dma::LINE_COUNT) } else { 1 };
        match value & 3 {
            0 => {}
            1 | 2 => {
                // All of the source is read before any of it is written
                // (kestrel's choice where they overlap).
                let data = self.read_lines(fd, &src, len, lines)?;
                self.write_lines(fd, &dst, len, &data)?;
            }
            _ => return Err(named("a reserved transfer type")),
        }
        if let Some((va, payload)) = semaphore {
            self.gpu_write_bytes(fd, va, &payload.to_le_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pitch_line_is_one_run() {
        let s = Surface::Pitch {
            address: 0x1000,
            pitch: 0x40,
        };
        assert_eq!(s.runs(2, 10), [(0x1080, 10)]);
    }

    #[test]
    fn a_block_linear_line_is_split_where_the_gob_layout_breaks_it() {
        // Blocks one GOB high, 128 bytes across: 40 bytes of row 1 from
        // byte 8 are bytes 8-15 of sector 0's second row (at 16 + 8), all
        // of sector 1's (at 32 + 16), then bytes 32-47, sector 8's second
        // row (at 256 + 16).
        let s = Surface::Blocks {
            address: 0x10000,
            blocks: Blocks {
                width: 128,
                height: 8,
                log2: [0, 0, 0],
            },
            origin: [8, 0, 0],
        };
        assert_eq!(
            s.runs(1, 40),
            [(0x10000 + 24, 8), (0x10000 + 48, 16), (0x10000 + 272, 16)]
        );
    }
}
