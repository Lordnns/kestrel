//! The GPU's nvdrv devices (Switchbrew, "NV services"): what libnx's
//! nvGpuInit, nvAddressSpace* and nvGpuChannel* send (`nx/source/nvidia/
//! gpu.c`, `address_space.c`, `channel.c`, `gpu_channel.c` and their
//! `ioctl/*.c`), and so what deko3d does before it draws:
//!
//! - "/dev/nvhost-ctrl-gpu" describes the GPU: the Tegra X1's GM20B, with
//!   the values Switchbrew and libnx's `nvidia/ioctl.h` record from a
//!   console.
//! - "/dev/nvhost-as-gpu" is a GPU address space per descriptor: regions
//!   reserved, and nvmap objects mapped at GPU addresses.
//! - "/dev/nvhost-gpu" is a channel per descriptor: bound to an address
//!   space, given a command queue (GPFIFO) and a syncpoint, an engine
//!   class. Command lists submitted to it are recorded, and then the
//!   program stops, named: running them is phase 5B.

use std::collections::BTreeMap;

use super::nv::{error, put_u32, u32_at, u64_at, Device};
use super::IpcError;
use crate::kernel::{Kind, ObjectId};
use crate::Process;

/// The /dev/nvhost-ctrl-gpu ioctls ("NV services").
mod ctrl_gpu {
    pub const ZCULL_GET_CTX_SIZE: u32 = 0x8004_4701;
    pub const ZCULL_GET_INFO: u32 = 0x8028_4702;
    pub const GET_CHARACTERISTICS: u32 = 0xc0b0_4705;
    pub const GET_TPC_MASKS: u32 = 0xc018_4706;
    pub const GET_GPU_TIME: u32 = 0xc010_471c;
}

/// The /dev/nvhost-as-gpu ioctls ("NV services").
mod as_gpu {
    pub const BIND_CHANNEL: u32 = 0x4004_4101;
    pub const ALLOC_SPACE: u32 = 0xc018_4102;
    pub const FREE_SPACE: u32 = 0xc010_4103;
    pub const UNMAP_BUFFER: u32 = 0xc008_4105;
    pub const MAP_BUFFER_EX: u32 = 0xc028_4106;
    pub const GET_VA_REGIONS: u32 = 0xc040_4108;
    /// ALLOC_AS_EX, libnx's nvioctlNvhostAsGpu_InitializeEx.
    pub const ALLOC_AS_EX: u32 = 0x4028_4109;
}

/// The /dev/nvhost-gpu ioctls ("NV services", "Channels").
mod channel {
    pub const SET_NVMAP_FD: u32 = 0x4004_4801;
    pub const SET_TIMEOUT: u32 = 0x4004_4803;
    pub const ALLOC_OBJ_CTX: u32 = 0xc010_4809;
    pub const ZCULL_BIND: u32 = 0xc010_480b;
    pub const SET_ERROR_NOTIFIER: u32 = 0xc018_480c;
    pub const SET_PRIORITY: u32 = 0x4004_480d;
    pub const ALLOC_GPFIFO_EX2: u32 = 0xc020_481a;
    /// SUBMIT_GPFIFO2 through Ioctl2 (libnx's KickoffPb): the entries in
    /// the second input buffer.
    pub const SUBMIT_GPFIFO2: u32 = 0xc018_481b;
    /// SUBMIT_GPFIFO through Ioctl: the entries inline after a 0x18-byte
    /// header, so its size field varies; this is its number without the
    /// size.
    pub const SUBMIT_GPFIFO_NR: u32 = 0xc000_4808;
}

/// The GPU's page sizes: small pages, and the big pages
/// GET_CHARACTERISTICS lists (0x30000: 64 KiB and 128 KiB, 128 KiB the
/// default).
const SMALL_PAGE: u32 = 0x1000;
const BIG_PAGE: u32 = 0x2_0000;
const BIG_PAGES: u32 = 0x3_0000;

/// kestrel's choice of GPU address space layout, which GET_VA_REGIONS
/// reports: small pages from 16 MiB (null and small GPU addresses stay
/// unmapped) to 16 GiB, big pages from there to the end of the 40-bit
/// space GET_CHARACTERISTICS gives (gpu_va_bit_count 0x28).
const SMALL_START: u64 = 0x100_0000;
const BIG_START: u64 = 0x4_0000_0000;
const VA_END: u64 = 1 << 40;

/// The engine classes GET_CHARACTERISTICS lists, which ALLOC_OBJ_CTX
/// takes ("NV services": 2D, 3D, compute, inline-to-memory, DMA copy, the
/// GPFIFO channel).
const CLASSES: [u32; 6] = [0x902d, 0xb197, 0xb1c0, 0xa140, 0xb0b5, 0xb06f];

/// SET_PRIORITY's values ("NV services": low, medium, high).
const PRIORITIES: [u32; 3] = [0x32, 0x64, 0x96];

/// GET_CHARACTERISTICS's answer, 0xa0 bytes, as Switchbrew ("NV
/// services") and libnx's `nvidia/ioctl.h` record a Switch's GM20B. Where
/// Switchbrew gives two values, impl is 0xb (GM20B; 0xe is GM20B_B,
/// kestrel's choice of the first).
fn characteristics() -> Vec<u8> {
    let mut b = Vec::with_capacity(0xa0);
    let w = |b: &mut Vec<u8>, v: u32| b.extend(v.to_le_bytes());
    let d = |b: &mut Vec<u8>, v: u64| b.extend(v.to_le_bytes());
    w(&mut b, 0x120); // arch: GM200
    w(&mut b, 0xb); // impl: GM20B
    w(&mut b, 0xa1); // rev: A1
    w(&mut b, 1); // num_gpc
    d(&mut b, 0x4_0000); // l2_cache_size
    d(&mut b, 0); // on_board_video_memory_size
    for v in [
        2,         // num_tpc_per_gpc
        0x20,      // bus_type: AXI
        BIG_PAGE,  // big_page_size
        0x2_0000,  // compression_page_size
        0x1b,      // pde_coverage_bit_count
        BIG_PAGES, // available_big_page_sizes
        1,         // gpc_mask
        0x503,     // sm_arch_sm_version
        0x503,     // sm_arch_spa_version
        0x80,      // sm_arch_warp_count
        0x28,      // gpu_va_bit_count
        0,         // reserved
    ] {
        w(&mut b, v);
    }
    d(&mut b, 0x55); // flags
                     // twod, threed, compute, gpfifo, inline_to_memory and dma_copy classes
    for class in [0x902d, 0xb197, 0xb1c0, 0xb06f, 0xa140, 0xb0b5] {
        w(&mut b, class);
    }
    for v in [1, 0, 2, 1, 0, 1, 0x2_1d70, 0] {
        // max_fbps_count, fbp_en_mask, max_ltc_per_fbp, max_lts_per_ltc,
        // max_tex_per_tpc, max_gpc_count, rop_l2_en_mask_0 and _1
        w(&mut b, v);
    }
    d(&mut b, 0x62_3032_6d67); // chipname "gm20b"
    d(&mut b, 0); // gr_compbit_store_base_hw
    debug_assert_eq!(b.len(), 0xa0);
    b
}

/// ZCULL_GET_INFO's answer: the values libnx's `nvidia/ioctl.h` records
/// from a console, in its field order (width and height alignment, pixel
/// squares by aliquots, aliquot total, region byte multiplier, region and
/// subregion header sizes, subregion width and height alignment,
/// subregion count).
const ZCULL_INFO: [u32; 10] = [0x20, 0x20, 0x400, 0x800, 0x20, 0x20, 0xc0, 0x20, 0x40, 0x10];

/// A range of GPU addresses ALLOC_SPACE reserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub start: u64,
    pub size: u64,
    pub page_size: u32,
    pub sparse: bool,
}

/// An nvmap object mapped at a GPU address (MAP_BUFFER_EX).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub size: u64,
    pub handle: u32,
    /// Where in the nvmap object the mapping starts.
    pub buffer_offset: u64,
    pub kind: u32,
    pub page_size: u32,
    pub cacheable: bool,
    /// Ranges whose kind MAP_BUFFER_EX's modify flag changed: (offset in
    /// the mapping, size, kind).
    pub modified: Vec<(u64, u64, u32)>,
}

/// A GPU address space: one per /dev/nvhost-as-gpu descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressSpace {
    /// Set by ALLOC_AS_EX (libnx's InitializeEx).
    pub big_page_size: Option<u32>,
    pub reservations: Vec<Reservation>,
    pub mappings: Vec<Mapping>,
    /// The channel bound to it.
    pub channel: Option<u32>,
}

/// A GPU channel: one per /dev/nvhost-gpu descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    pub nvmap_fd: Option<u32>,
    /// The address space BIND_CHANNEL bound it to.
    pub address_space: Option<u32>,
    /// ALLOC_GPFIFO_EX2: the queue's entry count and the channel's
    /// syncpoint.
    pub gpfifo_entries: Option<u32>,
    pub syncpoint: Option<u32>,
    /// ALLOC_OBJ_CTX: the engine class.
    pub class: Option<u32>,
    pub zcull: Option<(u64, u32)>,
    pub error_notifier: bool,
    pub priority: Option<u32>,
    pub timeout: Option<u32>,
    /// QueryEvent's events 1 to 3 (BptIntEvent, BptPauseEvent,
    /// ErrorNotifierEvent), made on first use; nothing signals them yet.
    pub events: [Option<ObjectId>; 3],
    /// Every GPFIFO entry submitted, in order, for phase 5B to run.
    pub submitted: Vec<u64>,
}

/// GPU state of nvdrv: address spaces and channels by descriptor.
#[derive(Debug, Default)]
pub struct Gpu {
    pub address_spaces: BTreeMap<u32, AddressSpace>,
    pub channels: BTreeMap<u32, Channel>,
    /// The syncpoints channels took: kestrel's choice, the first channel
    /// gets 1, the next 2, and so on (0 stays unused).
    pub channel_syncpoints: u32,
}

impl AddressSpace {
    /// The ranges GPU addresses are taken by: reservations, and mappings
    /// outside them.
    fn used(&self) -> Vec<(u64, u64)> {
        let mut used: Vec<(u64, u64)> = self
            .reservations
            .iter()
            .map(|r| (r.start, r.size))
            .collect();
        used.extend(
            self.mappings
                .iter()
                .filter(|m| self.reservation_holding(m.start, m.size).is_none())
                .map(|m| (m.start, m.size)),
        );
        used.sort_unstable();
        used
    }

    fn reservation_holding(&self, start: u64, size: u64) -> Option<&Reservation> {
        self.reservations
            .iter()
            .find(|r| start >= r.start && start + size <= r.start + r.size)
    }

    fn overlaps(used: &[(u64, u64)], start: u64, size: u64) -> bool {
        used.iter().any(|&(s, n)| start < s + n && s < start + size)
    }

    /// The first free `size` bytes aligned to `align` in the region for
    /// pages of `page_size` (kestrel's choice: first fit, lowest
    /// address).
    fn find_free(&self, page_size: u32, size: u64, align: u64) -> Option<u64> {
        let (lo, hi) = if page_size == SMALL_PAGE {
            (SMALL_START, BIG_START)
        } else {
            (BIG_START, VA_END)
        };
        let used = self.used();
        let mut at = lo.next_multiple_of(align);
        for &(s, n) in &used {
            if s + n <= at {
                continue;
            }
            if at + size <= s {
                break;
            }
            at = (s + n).next_multiple_of(align);
        }
        (at + size <= hi).then_some(at)
    }

    fn page_size_ok(&self, page_size: u32) -> bool {
        page_size == SMALL_PAGE || Some(page_size) == self.big_page_size
    }
}

impl Process {
    /// One /dev/nvhost-ctrl-gpu ioctl on `arg`.
    pub(super) fn nvhost_ctrl_gpu_ioctl(
        &mut self,
        code: u32,
        arg: &mut [u8],
    ) -> Result<u32, IpcError> {
        Ok(match code {
            // ZCULL_GET_CTX_SIZE: no source gives a console's answer.
            // kestrel's choice: 0, as kestrel's GPU keeps no ZCULL context
            // to save (deko3d then binds none).
            ctrl_gpu::ZCULL_GET_CTX_SIZE => {
                put_u32(arg, 0, 0);
                error::SUCCESS
            }
            ctrl_gpu::ZCULL_GET_INFO => {
                for (i, v) in ZCULL_INFO.into_iter().enumerate() {
                    put_u32(arg, 4 * i, v);
                }
                error::SUCCESS
            }
            // GET_CHARACTERISTICS: the buffer's size and address in (both
            // "must not be NULL"; kestrel's choice: BadParameter if 0), the
            // size out as 0xa0, then the characteristics.
            ctrl_gpu::GET_CHARACTERISTICS => {
                if u64_at(arg, 0) == 0 || u64_at(arg, 8) == 0 {
                    return Ok(error::BAD_PARAMETER);
                }
                arg[0..8].copy_from_slice(&0xa0u64.to_le_bytes());
                arg[16..16 + 0xa0].copy_from_slice(&characteristics());
                error::SUCCESS
            }
            // GET_TPC_MASKS: the mask buffer's size in ("must not be NULL",
            // BadParameter if 0, kestrel's choice), one 32-bit mask per GPC
            // out at 0x10. One GPC with two TPCs (num_tpc_per_gpc): 0b11.
            ctrl_gpu::GET_TPC_MASKS => {
                if u32_at(arg, 0) == 0 {
                    return Ok(error::BAD_PARAMETER);
                }
                put_u32(arg, 0x10, 0b11);
                put_u32(arg, 0x14, 0);
                error::SUCCESS
            }
            // GET_GPU_TIME: PTIMER, one tick every 1.625 ns ("NV services").
            // kestrel's choice: it starts with the system tick, so it is the
            // 19.2 MHz clock times 1e9 / 19.2e6 / 1.625 = 1250 / 39.
            ctrl_gpu::GET_GPU_TIME => {
                let ptimer = (self.now() as u128 * 1250 / 39) as u64;
                arg[0..8].copy_from_slice(&ptimer.to_le_bytes());
                arg[8..16].fill(0);
                error::SUCCESS
            }
            _ => {
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvhost-ctrl-gpu ioctl {code:#010x}"
                )))
            }
        })
    }

    /// One /dev/nvhost-as-gpu ioctl on `arg`, for the address space of
    /// descriptor `fd`.
    pub(super) fn nvhost_as_gpu_ioctl(
        &mut self,
        fd: u32,
        code: u32,
        arg: &mut [u8],
    ) -> Result<u32, IpcError> {
        let nv = &mut self.services.nv;
        let Some(space) = nv.gpu.address_spaces.get(&fd) else {
            return Ok(error::BAD_PARAMETER);
        };
        let initialized = space.big_page_size.is_some();
        if code != as_gpu::ALLOC_AS_EX && !initialized {
            // kestrel's choice: an address space takes nothing before it is
            // initialized.
            return Ok(error::INVALID_STATE);
        }
        Ok(match code {
            // ALLOC_AS_EX (libnx's InitializeEx). Switchbrew and libnx
            // disagree on its fields: Switchbrew puts the big page size
            // first and flags third, libnx sends flags (1) first and the big
            // page size third. Homebrew runs on consoles with libnx's
            // layout, so kestrel reads that (kestrel's choice). The big page
            // size must be one GET_CHARACTERISTICS lists, 0 meaning the
            // default (128 KiB); a second initialization is InvalidState.
            as_gpu::ALLOC_AS_EX => {
                let big = match u32_at(arg, 8) {
                    0 => BIG_PAGE,
                    v if v.is_power_of_two() && v & BIG_PAGES == v => v,
                    _ => return Ok(error::BAD_PARAMETER),
                };
                if initialized {
                    return Ok(error::INVALID_STATE);
                }
                nv.gpu
                    .address_spaces
                    .get_mut(&fd)
                    .expect("found")
                    .big_page_size = Some(big);
                error::SUCCESS
            }
            // BIND_CHANNEL: a /dev/nvhost-gpu descriptor. A channel is bound
            // once ("NV services": it cannot be unbound or moved), and
            // kestrel's choice is one channel to an address space.
            as_gpu::BIND_CHANNEL => {
                let ch = u32_at(arg, 0);
                if nv.file(ch) != Some(Device::NvhostGpu) {
                    return Ok(error::BAD_PARAMETER);
                }
                let channel = nv.gpu.channels.get_mut(&ch).expect("open channel");
                if channel.address_space.is_some() || space.channel.is_some() {
                    return Ok(error::INVALID_STATE);
                }
                channel.address_space = Some(fd);
                nv.gpu.address_spaces.get_mut(&fd).expect("found").channel = Some(ch);
                error::SUCCESS
            }
            // ALLOC_SPACE: pages, page size, flags (1 fixed offset, 2
            // sparse), then the alignment in or, when fixed, the address;
            // the address out.
            as_gpu::ALLOC_SPACE => {
                let (pages, page_size, flags) = (u32_at(arg, 0), u32_at(arg, 4), u32_at(arg, 8));
                let size = pages as u64 * page_size as u64;
                if !space.page_size_ok(page_size) || pages == 0 || flags & !3 != 0 {
                    return Ok(error::BAD_PARAMETER);
                }
                let start = if flags & 1 != 0 {
                    let at = u64_at(arg, 16);
                    if at % page_size as u64 != 0
                        || at < SMALL_START
                        || at + size > VA_END
                        || AddressSpace::overlaps(&space.used(), at, size)
                    {
                        return Ok(error::BAD_PARAMETER);
                    }
                    at
                } else {
                    let align = u64_at(arg, 16).max(page_size as u64);
                    if !align.is_power_of_two() {
                        return Ok(error::BAD_PARAMETER);
                    }
                    match space.find_free(page_size, size, align) {
                        Some(at) => at,
                        None => return Ok(error::INSUFFICIENT_MEMORY),
                    }
                };
                nv.gpu
                    .address_spaces
                    .get_mut(&fd)
                    .expect("found")
                    .reservations
                    .push(Reservation {
                        start,
                        size,
                        page_size,
                        sparse: flags & 2 != 0,
                    });
                arg[16..24].copy_from_slice(&start.to_le_bytes());
                error::SUCCESS
            }
            // FREE_SPACE: the address, pages and page size of a reservation
            // (kestrel's choice: exactly one ALLOC_SPACE made, and what was
            // mapped inside it is unmapped with it).
            as_gpu::FREE_SPACE => {
                let (start, pages, page_size) = (u64_at(arg, 0), u32_at(arg, 8), u32_at(arg, 12));
                let size = pages as u64 * page_size as u64;
                let space = nv.gpu.address_spaces.get_mut(&fd).expect("found");
                let Some(i) = space
                    .reservations
                    .iter()
                    .position(|r| r.start == start && r.size == size && r.page_size == page_size)
                else {
                    return Ok(error::BAD_PARAMETER);
                };
                space.reservations.remove(i);
                space
                    .mappings
                    .retain(|m| m.start + m.size <= start || m.start >= start + size);
                error::SUCCESS
            }
            as_gpu::MAP_BUFFER_EX => self.map_buffer_ex(fd, arg)?,
            // UNMAP_BUFFER: the address a mapping starts at.
            as_gpu::UNMAP_BUFFER => {
                let start = u64_at(arg, 0);
                let space = nv.gpu.address_spaces.get_mut(&fd).expect("found");
                match space.mappings.iter().position(|m| m.start == start) {
                    Some(i) => {
                        space.mappings.remove(i);
                        error::SUCCESS
                    }
                    None => error::BAD_PARAMETER,
                }
            }
            // GET_VA_REGIONS: the buffer size out as two regions' worth
            // (48), then the small and big page regions: address, page
            // size, pages.
            as_gpu::GET_VA_REGIONS => {
                let big = space.big_page_size.expect("initialized");
                put_u32(arg, 8, 48);
                put_u32(arg, 12, 0);
                let regions = [
                    (
                        SMALL_START,
                        SMALL_PAGE,
                        (BIG_START - SMALL_START) / SMALL_PAGE as u64,
                    ),
                    (BIG_START, big, (VA_END - BIG_START) / big as u64),
                ];
                for (i, (start, page, pages)) in regions.into_iter().enumerate() {
                    let at = 16 + 24 * i;
                    arg[at..at + 8].copy_from_slice(&start.to_le_bytes());
                    put_u32(arg, at + 8, page);
                    put_u32(arg, at + 12, 0);
                    arg[at + 16..at + 24].copy_from_slice(&pages.to_le_bytes());
                }
                error::SUCCESS
            }
            _ => {
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvhost-as-gpu ioctl {code:#010x}"
                )))
            }
        })
    }

    /// MAP_BUFFER_EX: flags (1 fixed offset, 4 cacheable, 0x100 modify),
    /// kind, nvmap handle, page size (in and out), the offset in the
    /// buffer, the mapping's size (0: the rest of the buffer), then the
    /// alignment in or, when fixed or modifying, the address; the address
    /// out.
    fn map_buffer_ex(&mut self, fd: u32, arg: &mut [u8]) -> Result<u32, IpcError> {
        let (flags, kind, handle) = (u32_at(arg, 0), u32_at(arg, 4), u32_at(arg, 8));
        let (page_size, buffer_offset, size) = (u32_at(arg, 12), u64_at(arg, 16), u64_at(arg, 24));
        let at = u64_at(arg, 32);
        let nv = &mut self.services.nv;
        if flags & !0x105 != 0 {
            return Ok(error::BAD_PARAMETER);
        }
        // Modify: change the kind of part of an existing mapping (libnx's
        // nvAddressSpaceModify), which starts at `at`.
        if flags & 0x100 != 0 {
            let space = nv.gpu.address_spaces.get_mut(&fd).expect("found");
            let Some(m) = space.mappings.iter_mut().find(|m| m.start == at) else {
                return Ok(error::BAD_PARAMETER);
            };
            if size == 0 || buffer_offset + size > m.size {
                return Ok(error::BAD_PARAMETER);
            }
            m.modified.push((buffer_offset, size, kind));
            return Ok(error::SUCCESS);
        }
        let Some(object) = nv.map(handle).filter(|o| o.addr.is_some()) else {
            return Ok(error::BAD_PARAMETER);
        };
        let size = if size == 0 {
            (object.size as u64).saturating_sub(buffer_offset)
        } else {
            size
        };
        if size == 0 || buffer_offset + size > object.size as u64 {
            return Ok(error::BAD_PARAMETER);
        }
        // "Unaligned size will cause a Panic" ("NV services"): on a console
        // the driver stops the program.
        if size % SMALL_PAGE as u64 != 0 || buffer_offset % SMALL_PAGE as u64 != 0 {
            return Err(IpcError::Stop(format!(
                "nvdrv MAP_BUFFER_EX of {size:#x} bytes at offset {buffer_offset:#x}, not \
                 page-aligned: the driver panics on a console"
            )));
        }
        let space = nv.gpu.address_spaces.get(&fd).expect("found");
        // kestrel's choice: the page size asked for (0: small pages).
        let page_size = if page_size == 0 {
            SMALL_PAGE
        } else {
            page_size
        };
        if !space.page_size_ok(page_size) {
            return Ok(error::BAD_PARAMETER);
        }
        let start = if flags & 1 != 0 {
            // kestrel's choice: a fixed mapping goes inside a reservation
            // (as deko3d's code segment does), on its page size, over no
            // other mapping.
            let fits = space
                .reservation_holding(at, size)
                .is_some_and(|r| at % r.page_size as u64 == 0);
            let taken = space
                .mappings
                .iter()
                .any(|m| at < m.start + m.size && m.start < at + size);
            if !fits || taken {
                return Ok(error::BAD_PARAMETER);
            }
            at
        } else {
            let align = at.max(page_size as u64);
            if !align.is_power_of_two() {
                return Ok(error::BAD_PARAMETER);
            }
            match space.find_free(page_size, size, align) {
                Some(start) => start,
                None => return Ok(error::INSUFFICIENT_MEMORY),
            }
        };
        nv.gpu
            .address_spaces
            .get_mut(&fd)
            .expect("found")
            .mappings
            .push(Mapping {
                start,
                size,
                handle,
                buffer_offset,
                kind,
                page_size,
                cacheable: flags & 4 != 0,
                modified: Vec::new(),
            });
        put_u32(arg, 12, page_size);
        arg[32..40].copy_from_slice(&start.to_le_bytes());
        Ok(error::SUCCESS)
    }

    /// One /dev/nvhost-gpu ioctl on `arg`, for the channel of descriptor
    /// `fd`; `extra` is Ioctl2's second input buffer.
    pub(super) fn nvhost_gpu_ioctl(
        &mut self,
        fd: u32,
        code: u32,
        arg: &mut [u8],
        extra: Option<&[u8]>,
    ) -> Result<u32, IpcError> {
        let nv = &mut self.services.nv;
        let Some(ch) = nv.gpu.channels.get(&fd) else {
            return Ok(error::BAD_PARAMETER);
        };
        let ch = ch.clone();
        let set = |p: &mut Self, f: &dyn Fn(&mut Channel)| {
            f(p.services
                .nv
                .gpu
                .channels
                .get_mut(&fd)
                .expect("open channel"));
        };
        Ok(match code {
            // SET_NVMAP_FD: an open /dev/nvmap descriptor.
            channel::SET_NVMAP_FD => {
                let map_fd = u32_at(arg, 0);
                if nv.file(map_fd) != Some(Device::Nvmap) {
                    return Ok(error::BAD_PARAMETER);
                }
                set(self, &|c| c.nvmap_fd = Some(map_fd));
                error::SUCCESS
            }
            channel::SET_TIMEOUT => {
                let timeout = u32_at(arg, 0);
                set(self, &|c| c.timeout = Some(timeout));
                error::SUCCESS
            }
            // SET_PRIORITY: low, medium or high (kestrel's choice:
            // BadParameter otherwise).
            channel::SET_PRIORITY => {
                let priority = u32_at(arg, 0);
                if !PRIORITIES.contains(&priority) {
                    return Ok(error::BAD_PARAMETER);
                }
                set(self, &|c| c.priority = Some(priority));
                error::SUCCESS
            }
            // SET_ERROR_NOTIFIER: offset and size (ignored), then non-zero
            // to set it up, zero to take it down.
            channel::SET_ERROR_NOTIFIER => {
                let on = u32_at(arg, 16) != 0;
                set(self, &|c| c.error_notifier = on);
                error::SUCCESS
            }
            // ALLOC_GPFIFO_EX2: entries, flags, then the channel's fence
            // out (its syncpoint and current value). kestrel's choices: the
            // channel must be bound to an address space first (as Linux's
            // driver asks), a channel takes one queue (InvalidState after),
            // and the entry count must be non-zero.
            channel::ALLOC_GPFIFO_EX2 => {
                let entries = u32_at(arg, 0);
                if ch.address_space.is_none() || ch.gpfifo_entries.is_some() {
                    return Ok(error::INVALID_STATE);
                }
                if entries == 0 {
                    return Ok(error::BAD_PARAMETER);
                }
                nv.gpu.channel_syncpoints += 1;
                let id = nv.gpu.channel_syncpoints;
                let Some(sp) = nv.syncpoint(id) else {
                    return Ok(error::INSUFFICIENT_MEMORY);
                };
                put_u32(arg, 12, id);
                put_u32(arg, 16, sp.max);
                set(self, &|c| {
                    c.gpfifo_entries = Some(entries);
                    c.syncpoint = Some(id);
                });
                error::SUCCESS
            }
            // ALLOC_OBJ_CTX: an engine class GET_CHARACTERISTICS lists and
            // flags; the object ID out ("ignored": kestrel's choice, 1).
            // "You can only have one object context allocated at a time.
            // You must have bound an address space before using this."
            // (kestrel's choice of errors: AlreadyAllocated, InvalidState).
            channel::ALLOC_OBJ_CTX => {
                let class = u32_at(arg, 0);
                if !CLASSES.contains(&class) {
                    return Ok(error::BAD_PARAMETER);
                }
                if ch.address_space.is_none() {
                    return Ok(error::INVALID_STATE);
                }
                if ch.class.is_some() {
                    return Ok(error::ALREADY_ALLOCATED);
                }
                set(self, &|c| c.class = Some(class));
                arg[8..16].copy_from_slice(&1u64.to_le_bytes());
                error::SUCCESS
            }
            // ZCULL_BIND: a GPU address mapped in the channel's address
            // space, and a mode (0 to 3).
            channel::ZCULL_BIND => {
                let (va, mode) = (u64_at(arg, 0), u32_at(arg, 8));
                let mapped = ch
                    .address_space
                    .and_then(|a| nv.gpu.address_spaces.get(&a))
                    .is_some_and(|s| {
                        s.mappings
                            .iter()
                            .any(|m| va >= m.start && va < m.start + m.size)
                    });
                if !mapped || mode > 3 {
                    return Ok(error::BAD_PARAMETER);
                }
                set(self, &|c| c.zcull = Some((va, mode)));
                error::SUCCESS
            }
            // SUBMIT_GPFIFO2 (Ioctl2) and SUBMIT_GPFIFO (Ioctl, entries
            // inline): the entries are recorded, then the program stops,
            // named: kestrel does not run GPU command lists yet (phase 5B).
            _ if code == channel::SUBMIT_GPFIFO2
                || (code & 0xc000_ffff == channel::SUBMIT_GPFIFO_NR) =>
            {
                let inline = code != channel::SUBMIT_GPFIFO2;
                let count = u32_at(arg, 8) as usize;
                let entries = if inline {
                    &arg[0x18..]
                } else {
                    extra.unwrap_or(&[])
                };
                if ch.gpfifo_entries.is_none() || ch.class.is_none() {
                    return Ok(error::INVALID_STATE);
                }
                if entries.len() < count * 8 {
                    return Err(IpcError::Stop(format!(
                        "nvdrv ioctl {code:#010x} with {count} GPFIFO entries in {:#x} bytes",
                        entries.len()
                    )));
                }
                let words: Vec<u64> = (0..count).map(|i| u64_at(entries, 8 * i)).collect();
                set(self, &|c| c.submitted.extend(&words));
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvhost-gpu ioctl {code:#010x}: {count} GPFIFO entries submitted, \
                     GPU command lists do not run yet (phase 5B)"
                )));
            }
            _ => {
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvhost-gpu ioctl {code:#010x}"
                )))
            }
        })
    }

    /// QueryEvent on a channel: BptIntEvent (1), BptPauseEvent (2) and
    /// ErrorNotifierEvent (3) ("NV services"); a handle to it, made on
    /// first use. None for other IDs.
    pub(super) fn channel_event(&mut self, fd: u32, id: u32) -> Option<Result<u32, u64>> {
        let i = (id as usize).checked_sub(1).filter(|&i| i < 3)?;
        let existing = self.services.nv.gpu.channels.get(&fd)?.events[i];
        let event = match existing {
            Some(e) => e,
            None => {
                let e = self.service_event();
                self.services.nv.gpu.channels.get_mut(&fd)?.events[i] = Some(e);
                e
            }
        };
        Some(self.handles.add_handle(event, Kind::ReadableEvent))
    }
}
