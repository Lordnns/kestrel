//! Reading and writing a GPU surface's 2D image (one layer or slice) as
//! linear rows, for draws and textures: the host renders on linear images,
//! the guest's memory holds them pitch linear or block linear
//! (`display::Blocks`).

use super::clear::Layout;
use crate::display::Blocks;
use crate::service::IpcError;
use crate::Process;

/// One 2D image of a surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Where the surface (pitch: this image) starts.
    pub address: u64,
    /// Elements wide and high (pixels, or compressed blocks).
    pub width: u32,
    pub height: u32,
    /// Bytes an element takes.
    pub bytes: u32,
    pub layout: Layout,
    /// Block linear: the surface's height in rows, for its slices' size,
    /// and the slice; pitch: unused.
    pub surface_height: u32,
    pub z: u32,
}

impl Image {
    /// The surface's block-linear description.
    fn blocks(&self, log2: [u32; 3]) -> Blocks {
        Blocks {
            width: (self.width * self.bytes) as usize,
            height: self.surface_height as usize,
            log2,
        }
    }

    /// The bytes of guest memory the image's rows lie in, from `address`.
    fn span(&self) -> std::ops::Range<u64> {
        match self.layout {
            Layout::Pitch(pitch) => {
                0..(self.height.max(1) as u64 - 1) * pitch as u64 + (self.width * self.bytes) as u64
            }
            Layout::Blocks(log2) => {
                let r = self
                    .blocks(log2)
                    .rows(0, self.height as usize, self.z as usize);
                r.start as u64..r.end as u64
            }
        }
    }

    /// Call `f(linear offset, surface offset, length)` for each run of
    /// bytes that is contiguous in both (rows, or 16-byte GOB runs).
    fn runs(&self, mut f: impl FnMut(usize, usize, usize)) {
        let row = (self.width * self.bytes) as usize;
        match self.layout {
            Layout::Pitch(pitch) => {
                for y in 0..self.height as usize {
                    f(y * row, y * pitch as usize, row);
                }
            }
            Layout::Blocks(log2) if row % 64 == 0 => {
                // Whole GOB rows: a GOB row's four 16-byte runs are its
                // sectors s, s + 1, s + 8, s + 9 (`display::Blocks`), at
                // 0, 32, 256 and 288 bytes from the first.
                let b = self.blocks(log2);
                for y in 0..self.height as usize {
                    for x in (0..row).step_by(64) {
                        let at = b.offset(x, y, self.z as usize);
                        let lin = y * row + x;
                        f(lin, at, 16);
                        f(lin + 16, at + 32, 16);
                        f(lin + 32, at + 256, 16);
                        f(lin + 48, at + 288, 16);
                    }
                }
            }
            Layout::Blocks(log2) => {
                let b = self.blocks(log2);
                for y in 0..self.height as usize {
                    let mut x = 0;
                    while x < row {
                        let n = (16 - x % 16).min(row - x);
                        f(y * row + x, b.offset(x, y, self.z as usize), n);
                        x += n;
                    }
                }
            }
        }
    }
}

impl Process {
    /// Read `image` as rows of `width * bytes` bytes.
    pub(super) fn read_image(&self, fd: u32, image: &Image) -> Result<Vec<u8>, IpcError> {
        Ok(self.read_image_kept(fd, image)?.0)
    }

    /// Read `image`, and keep the memory it lies in for `write_image_over`.
    pub(super) fn read_image_kept(
        &self,
        fd: u32,
        image: &Image,
    ) -> Result<(Vec<u8>, Vec<u8>), IpcError> {
        let span = image.span();
        let src = self.gpu_read_bytes(
            fd,
            image.address + span.start,
            (span.end - span.start) as usize,
        )?;
        let mut out = vec![0u8; (image.width * image.bytes * image.height) as usize];
        let base = span.start as usize;
        image.runs(|lin, at, n| {
            out[lin..lin + n].copy_from_slice(&src[at - base..at - base + n]);
        });
        Ok((out, src))
    }

    /// Write `data` (rows as `read_image` gives them) back to `image`.
    pub(super) fn write_image(
        &mut self,
        fd: u32,
        image: &Image,
        data: &[u8],
    ) -> Result<(), IpcError> {
        let span = image.span();
        // The bytes between the image's runs (padding, other slices) keep
        // what they hold.
        let kept = self.gpu_read_bytes(
            fd,
            image.address + span.start,
            (span.end - span.start) as usize,
        )?;
        self.write_image_over(fd, image, kept, data)
    }

    /// Write `data` back to `image` over `kept`, the memory
    /// `read_image_kept` read (nothing else writes it in between: the GPU
    /// runs while the program waits).
    pub(super) fn write_image_over(
        &mut self,
        fd: u32,
        image: &Image,
        mut kept: Vec<u8>,
        data: &[u8],
    ) -> Result<(), IpcError> {
        let span = image.span();
        let base = span.start as usize;
        image.runs(|lin, at, n| {
            kept[at - base..at - base + n].copy_from_slice(&data[lin..lin + n]);
        });
        self.gpu_write_bytes(fd, image.address + span.start, &kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_linear_image_is_read_in_gob_runs() {
        // 32x2 pixels of 4 bytes (128 bytes a row), blocks one GOB high:
        // row 1's bytes 16-31 are sector 1's second row (32 + 16) of the
        // first GOB.
        let img = Image {
            address: 0,
            width: 32,
            height: 2,
            bytes: 4,
            layout: Layout::Blocks([0, 0, 0]),
            surface_height: 2,
            z: 0,
        };
        let mut seen = Vec::new();
        img.runs(|lin, at, n| seen.push((lin, at, n)));
        assert_eq!(seen.len(), 16);
        assert!(seen.contains(&(128 + 16, 48, 16)));
        assert_eq!(img.span(), 0..1024);
    }
}
