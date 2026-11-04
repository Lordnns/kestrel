//! Running a channel's command lists (see `gpu/mod.rs`).

use super::{class, decode, engine_name, method, mme, Call, Engine, Entry, Kind};
use super::{MME_CODE_WORDS, MME_START_WORDS};
use crate::service::{nv, IpcError};
use crate::Process;

/// The host (channel) methods, byte offsets below 0x100 (`clb06f.h`; the
/// syncpoint methods from the Tegra K1 channel class, `cla26f.h`).
mod host {
    pub const SET_OBJECT: u32 = 0x00;
    pub const NOP: u32 = 0x08;
    pub const SEMAPHORE_A: u32 = 0x10;
    pub const SEMAPHORE_B: u32 = 0x14;
    pub const SEMAPHORE_C: u32 = 0x18;
    pub const SEMAPHORE_D: u32 = 0x1c;
    pub const FB_FLUSH: u32 = 0x24;
    pub const MEM_OP_A: u32 = 0x28;
    pub const MEM_OP_B: u32 = 0x2c;
    pub const MEM_OP_C: u32 = 0x30;
    pub const MEM_OP_D: u32 = 0x34;
    pub const SET_REFERENCE: u32 = 0x50;
    pub const SYNCPOINT_A: u32 = 0x70;
    pub const SYNCPOINT_B: u32 = 0x74;
    pub const WFI: u32 = 0x78;
    pub const YIELD: u32 = 0x80;
}

/// The first engine method; below it, host methods.
const ENGINE_METHODS: u32 = 0x100;

/// LOAD_CONSTANT_BUFFER_OFFSET (`clb197.h`).
const CB_OFFSET: u32 = 0x238c;

/// SET_MME_SHADOW_SCRATCH(0) (`clb197.h`): the macros' scratch registers.
const SCRATCH: u32 = 0x3400;

/// SET_MME_SHADOW_RAM_CONTROL (`clb197.h`).
const SHADOW_RAM_CONTROL: u32 = 0x124;

/// CALL_MME_MACRO(0) (`clb197.h`): macro n is called at 0x3800 + n * 8,
/// its parameters follow at 0x3804 + n * 8.
const MACRO_CALLS: u32 = 0x3800;

/// What a running macro reads and sends: the 3D engine's registers and
/// methods. A stop in a method the macro sends is kept to return as it is.
struct MacroHost<'a> {
    p: &'a mut Process,
    fd: u32,
    subchannel: u32,
    error: Option<IpcError>,
}

impl mme::Host for MacroHost<'_> {
    fn read(&mut self, method: u32) -> u32 {
        let state = self.p.channel_state(self.fd);
        state.engines[&class::THREED].shadow[(method / 4) as usize]
    }

    fn send(&mut self, method: u32, value: u32) -> Result<(), String> {
        let call = Call {
            subchannel: self.subchannel,
            method,
            value,
        };
        self.p.channel_state(self.fd).methods_run += 1;
        self.p.engine_method(self.fd, call).map_err(|e| {
            self.error = Some(e);
            "stopped".to_string()
        })
    }
}

fn stop(what: String) -> IpcError {
    IpcError::Unimplemented(what)
}

impl Process {
    /// Run the GPFIFO entries submitted on channel `fd` (its descriptor),
    /// in order.
    pub(crate) fn gpu_submit(&mut self, fd: u32, entries: &[u64]) -> Result<(), IpcError> {
        for &raw in entries {
            let entry = Entry::decode(raw);
            if entry.words == 0 {
                // A control entry: NOP (0) does nothing (`clb06f.h`,
                // NVB06F_GP_ENTRY1_OPCODE).
                if entry.opcode == 0 {
                    continue;
                }
                return Err(stop(format!(
                    "GPFIFO control entry with opcode {:#x}",
                    entry.opcode
                )));
            }
            let words = self.gpu_read_words(fd, entry.address, entry.words)?;
            let calls = decode(&words).map_err(stop)?;
            let mut i = 0;
            while i < calls.len() {
                let call = calls[i];
                i += 1;
                let Some(n) = self.macro_call(fd, call) else {
                    self.gpu_method(fd, call)?;
                    continue;
                };
                // CALL_MME_MACRO(n)'s data is the first parameter; the
                // CALL_MME_DATA(n) writes that follow on the subchannel are
                // the rest (envytools: the unit takes parameters from them).
                let mut params = vec![call.value];
                let data = MACRO_CALLS + n * 8 + 4;
                while i < calls.len()
                    && calls[i].subchannel == call.subchannel
                    && calls[i].method == data
                {
                    params.push(calls[i].value);
                    i += 1;
                }
                self.run_macro(fd, call.subchannel, n, &params)?;
            }
        }
        Ok(())
    }

    /// The guest address behind `len` bytes at GPU address `va` in the
    /// address space channel `fd` is bound to: inside one mapping of an
    /// nvmap object with memory.
    fn gpu_address(&self, fd: u32, va: u64, len: u64) -> Result<u64, IpcError> {
        let (addr, left) = self.gpu_span(fd, va)?;
        if len > left {
            return Err(IpcError::Stop(format!(
                "the GPU reading or writing {len:#x} bytes at {va:#x}, which no mapping holds \
                 (a GPU page fault)"
            )));
        }
        Ok(addr)
    }

    fn gpu_read_words(&self, fd: u32, va: u64, words: u32) -> Result<Vec<u32>, IpcError> {
        let len = words as u64 * 4;
        let addr = self.gpu_address(fd, va, len)?;
        let bytes = self
            .mem
            .read_vec(addr, len as usize)
            .map_err(|e| IpcError::Stop(format!("the GPU reading guest memory: {e}")))?;
        Ok(bytes
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes(w.try_into().expect("four bytes")))
            .collect())
    }

    fn gpu_read_u32(&self, fd: u32, va: u64) -> Result<u32, IpcError> {
        Ok(self.gpu_read_words(fd, va, 1)?[0])
    }

    fn gpu_write_u32(&mut self, fd: u32, va: u64, value: u32) -> Result<(), IpcError> {
        let addr = self.gpu_address(fd, va, 4)?;
        self.mem
            .write_bytes(addr, &value.to_le_bytes())
            .map_err(|e| IpcError::Stop(format!("the GPU writing guest memory: {e}")))
    }

    pub(super) fn channel_state(&mut self, fd: u32) -> &mut super::ChannelState {
        &mut self
            .services
            .nv
            .gpu
            .channels
            .get_mut(&fd)
            .expect("an open channel")
            .engine
    }

    /// The macro `call` calls: CALL_MME_MACRO(n) on a subchannel bound to
    /// the 3D engine.
    fn macro_call(&mut self, fd: u32, call: Call) -> Option<u32> {
        let class = self.channel_state(fd).subchannels[call.subchannel as usize]?;
        let at = call.method.checked_sub(MACRO_CALLS)?;
        (class == class::THREED && at % 8 == 0).then_some(at / 8)
    }

    /// Run 3D macro `n` with `params`: its method writes go to the 3D
    /// engine on `subchannel`, as the pushbuffer's would.
    fn run_macro(
        &mut self,
        fd: u32,
        subchannel: u32,
        n: u32,
        params: &[u32],
    ) -> Result<(), IpcError> {
        let state = self.channel_state(fd);
        state.methods_run += params.len() as u64;
        let e = state
            .engines
            .get(&class::THREED)
            .expect("bound engines exist");
        // kestrel's choice: a macro whose start address was never loaded
        // stops (no source says what the start RAM holds before).
        let Some(&start) = e.mme_start.get(n as usize) else {
            return Err(stop(format!(
                "GPU 3D macro {n} called, whose start address was never loaded"
            )));
        };
        let code = e.mme_code.clone();
        let mut host = MacroHost {
            p: self,
            fd,
            subchannel,
            error: None,
        };
        let ran = mme::run(&code, start, params, &mut host);
        if let Some(e) = host.error {
            return Err(e);
        }
        ran.map_err(|e| stop(format!("GPU 3D macro {n}: {e}")))
    }

    /// One method write.
    fn gpu_method(&mut self, fd: u32, call: Call) -> Result<(), IpcError> {
        self.channel_state(fd).methods_run += 1;
        if call.method < ENGINE_METHODS {
            return self.host_method(fd, call);
        }
        self.engine_method(fd, call)
    }

    /// A method write to the engine bound to its subchannel.
    fn engine_method(&mut self, fd: u32, call: Call) -> Result<(), IpcError> {
        let Some(class) = self.channel_state(fd).subchannels[call.subchannel as usize] else {
            return Err(stop(format!(
                "GPU method {:#x} = {:#x} on subchannel {}, which has no object bound",
                call.method, call.value, call.subchannel
            )));
        };
        let engine = engine_name(class);
        let Some((m, index)) = method(class, call.method) else {
            return Err(stop(format!(
                "GPU {engine} method {:#x} = {:#x}, which NVIDIA's header does not name",
                call.method, call.value
            )));
        };
        let state = self.channel_state(fd);
        let e = state.engines.get_mut(&class).expect("bound engines exist");
        // The 3D engine's MME shadow RAM, as SET_MME_SHADOW_RAM_CONTROL's
        // MODE (1:0) names its uses (NVIDIA gives only the names; Mesa's
        // NVK, MIT, writes state in PASSTHROUGH to "replay the previous
        // state from shadow RAM" after; deko3d does the same around its
        // clears). kestrel's reading: METHOD_TRACK (0) and _WITH_FILTER (1,
        // whose filter no source gives) record each write in the shadow;
        // METHOD_PASSTHROUGH (2) does not; METHOD_REPLAY (3) runs the method
        // with its shadow value instead of the data. The control method
        // itself is always recorded. Macros read the shadow.
        let i = (call.method / 4) as usize;
        let call = if class != class::THREED || call.method == SHADOW_RAM_CONTROL {
            e.shadow[i] = call.value;
            call
        } else {
            match e.reg(SHADOW_RAM_CONTROL) & 3 {
                0 | 1 => {
                    e.shadow[i] = call.value;
                    call
                }
                2 => call,
                _ => Call {
                    value: e.shadow[i],
                    ..call
                },
            }
        };
        e.regs[i] = call.value;
        if m.kind == Kind::State {
            return Ok(());
        }
        let name = if m.stride == 0 {
            m.name.to_string()
        } else {
            format!("{}({index})", m.name)
        };
        let unimplemented = || {
            stop(format!(
                "GPU {engine} method {name} ({:#x}) = {:#x}",
                call.method, call.value
            ))
        };
        match (class, m.name) {
            // Nothing to do: kestrel runs every method in order, at once,
            // so there is never work to wait for.
            (_, "NO_OPERATION")
            | (class::THREED, "WAIT_FOR_IDLE" | "PIPE_NOP" | "PIXEL_SHADER_BARRIER") => Ok(()),
            // Cache invalidations and flushes (shader, texture, sampler,
            // constant buffer, ROP caches, pending writes, ZCULL): kestrel
            // has no GPU caches, its memory is always current (kestrel's
            // choice: nothing to do).
            (_, name) if is_cache_op(name) => Ok(()),
            // DECOMPRESS_SURFACE and DECOMPRESS_ZETA_SURFACE: write a
            // target's compressed data out plainly (deko3d's Queue::
            // decompressSurface, before an image is presented). kestrel
            // writes every surface uncompressed (kestrel's choice: nothing
            // to do).
            (class::THREED, "DECOMPRESS_SURFACE" | "DECOMPRESS_ZETA_SURFACE") => Ok(()),
            // deko3d's tiled cache barrier and flush (methods NVIDIA's
            // header does not name, `gen-gpu-methods.py`): the GPU's tiled
            // cache holds rendering on chip before it reaches memory;
            // kestrel renders to memory directly (kestrel's choice: nothing
            // to do).
            (class::THREED, "DEKO3D_TILED_CACHE_BARRIER" | "DEKO3D_TILED_CACHE_FLUSH") => Ok(()),
            // deko3d's DiscardRenderTarget: the contents may be dropped;
            // keeping them is always right (kestrel's choice: nothing to
            // do).
            (class::THREED, "DEKO3D_DISCARD_RENDER_TARGET") => Ok(()),
            // The macro memories: a pointer, then words stored from it.
            (class::THREED, "LOAD_MME_INSTRUCTION_RAM_POINTER") => {
                e.mme_code_pointer = call.value;
                Ok(())
            }
            (class::THREED, "LOAD_MME_INSTRUCTION_RAM") => store(
                &mut e.mme_code,
                &mut e.mme_code_pointer,
                call.value,
                MME_CODE_WORDS,
            )
            .map_err(|_| unimplemented()),
            (class::THREED, "LOAD_MME_START_ADDRESS_RAM_POINTER") => {
                e.mme_start_pointer = call.value;
                Ok(())
            }
            (class::THREED, "LOAD_MME_START_ADDRESS_RAM") => store(
                &mut e.mme_start,
                &mut e.mme_start_pointer,
                call.value,
                MME_START_WORDS,
            )
            .map_err(|_| unimplemented()),
            // CHECK_SPH_VERSION and CHECK_AAM_VERSION: the shader program
            // header and attribute address map versions the program uses,
            // CURRENT (15:0) and OLDEST_SUPPORTED (31:16). No source says
            // which GM20B accepts; deko3d, which runs on consoles, declares
            // SPH 3 and AAM 2. kestrel's choice: a range that includes
            // those passes, others stop, named.
            (class::THREED, "CHECK_SPH_VERSION" | "CHECK_AAM_VERSION") => {
                let supported = if m.name == "CHECK_SPH_VERSION" { 3 } else { 2 };
                let (current, oldest) = (call.value & 0xffff, call.value >> 16);
                if (oldest..=current).contains(&supported) {
                    Ok(())
                } else {
                    Err(unimplemented())
                }
            }
            // SET_FALCON04: a call to the GPU's firmware that writes a
            // privileged (PGRAPH) register, as deko3d's WriteHardwareReg
            // macro (`source/maxwell/pgraph.mme`, zlib) uses it: the
            // register's address is the method's data, the value and a mask
            // of the bits to change are SET_MME_SHADOW_SCRATCH(1) and (2),
            // and the firmware sets SET_MME_SHADOW_SCRATCH(0) to 1 when it
            // is done (the macro waits for that). kestrel keeps the
            // registers' values in `ChannelState::hw_regs`; nothing reads
            // them yet. Other firmware calls stop, named.
            (class::THREED, "SET_FALCON04") => {
                let (value, mask) = (e.reg(SCRATCH + 4), e.reg(SCRATCH + 8));
                e.set(SCRATCH, 1);
                let regs = &mut self.channel_state(fd).hw_regs;
                let old = regs.get(&call.value).copied().unwrap_or(0);
                regs.insert(call.value, (old & !mask) | (value & mask));
                Ok(())
            }
            // Constant buffer uploads: LOAD_CONSTANT_BUFFER_OFFSET (15:0)
            // points into the buffer SET_CONSTANT_BUFFER_SELECTOR_A-C
            // select (SIZE 16:0, ADDRESS_UPPER 7:0, ADDRESS_LOWER), and
            // each LOAD_CONSTANT_BUFFER(i) word is written there, the
            // offset advancing by 4 (Mesa's NVK, MIT, sends a whole buffer
            // as words to LOAD_CONSTANT_BUFFER(0) after one offset). kestrel's
            // choice: a word past the buffer's size stops, named.
            (class::THREED, "LOAD_CONSTANT_BUFFER_OFFSET") => Ok(()),
            (class::THREED, "LOAD_CONSTANT_BUFFER") => {
                let size = e.reg(0x2380) & 0x1_ffff;
                let base = ((e.reg(0x2384) & 0xff) as u64) << 32 | e.reg(0x2388) as u64;
                let offset = e.reg(CB_OFFSET) & 0xffff;
                if offset + 4 > size {
                    return Err(unimplemented());
                }
                e.set(CB_OFFSET, offset + 4);
                self.gpu_write_u32(fd, base + offset as u64, call.value)
            }
            // BIND_GROUP_CONSTANT_BUFFER(group): SHADER_SLOT (8:4) of the
            // shader stage `group` gets the selected constant buffer
            // (SET_CONSTANT_BUFFER_SELECTOR_A-C), or none if not VALID (0).
            (class::THREED, "BIND_GROUP_CONSTANT_BUFFER") => {
                let slot = call.value >> 4 & 0x1f;
                if call.value & 1 == 1 {
                    let size = e.reg(0x2380) & 0x1_ffff;
                    let base = ((e.reg(0x2384) & 0xff) as u64) << 32 | e.reg(0x2388) as u64;
                    e.constant_buffers.insert((index, slot), (base, size));
                } else {
                    e.constant_buffers.remove(&(index, slot));
                }
                Ok(())
            }
            // INCREMENT_SYNC_POINT: the syncpoint in INDEX (bits 11:0).
            // Its CLEAN_L2 and CONDITION fields wait for work that is
            // already done.
            (class::THREED, "INCREMENT_SYNC_POINT") => {
                let id = call.value & 0xfff;
                self.syncpoint_incr(id).ok_or_else(|| {
                    IpcError::Stop(format!(
                        "the GPU incrementing syncpoint {id}, which does not exist"
                    ))
                })
            }
            // The inline-to-memory methods, in the 3D, compute and
            // inline-to-memory classes, and DMA copy (`gpu/transfer.rs`).
            (class::THREED | class::COMPUTE | class::INLINE_TO_MEMORY, "LAUNCH_DMA") => self
                .inline_launch(fd, class, call.value)
                .map_err(|r| r.unwrap_or_else(unimplemented)),
            (class::THREED | class::COMPUTE | class::INLINE_TO_MEMORY, "LOAD_INLINE_DATA") => self
                .inline_data(fd, class, call.value)
                .map_err(|r| r.unwrap_or_else(unimplemented)),
            (class::DMA_COPY, "LAUNCH_DMA") => self.copy_launch(fd, call.value),
            // DMA copy's NOP: nothing to do.
            (class::DMA_COPY, "NOP") => Ok(()),
            // A clear of a colour target (`gpu/clear.rs`).
            (class::THREED, "CLEAR_SURFACE") => self.clear_surface(fd, call.value),
            // Draws (`gpu/draw.rs`).
            (class::THREED, "BEGIN" | "END" | "DRAW_VERTEX_ARRAY" | "DRAW_INDEX_BUFFER") => {
                self.draw_method(fd, m.name, call.value)
            }
            // The Z cull unit's region clears, saves, loads and syncs:
            // kestrel tests depth per pixel and keeps no Z cull data
            // (kestrel's choice: nothing to do).
            (class::THREED, "CLEAR_ZCULL_REGION" | "STORE_ZCULL" | "LOAD_ZCULL" | "ZCULL_SYNC") => {
                Ok(())
            }
            (class::THREED, "SET_REPORT_SEMAPHORE_D") => {
                let abc = [e.reg(0x1b00), e.reg(0x1b04), e.reg(0x1b08)];
                self.report_semaphore(fd, abc, call.value)
                    .map_err(|r| r.unwrap_or_else(unimplemented))
            }
            _ => Err(unimplemented()),
        }
    }

    /// SET_REPORT_SEMAPHORE_D (`clb197.h`): OPERATION (bits 1:0) RELEASE
    /// with STRUCTURE_SIZE (bit 28) ONE_WORD, REPORT (27:23) NONE and no
    /// reduction (bit 3) writes PAYLOAD (C) at the address in A (bits 39:32)
    /// and B. Its RELEASE, PIPELINE_LOCATION and FLUSH fields order it after
    /// work that is already done. Other forms: Err(None), stopped by name.
    fn report_semaphore(&mut self, fd: u32, abc: [u32; 3], d: u32) -> Result<(), Option<IpcError>> {
        let release = d & 3 == 0;
        let one_word = d >> 28 & 1 == 1;
        let report = d >> 23 & 0x1f;
        let reduction = d >> 3 & 1 == 1;
        if !(release && one_word && report == 0 && !reduction) {
            return Err(None);
        }
        let va = ((abc[0] & 0xff) as u64) << 32 | abc[1] as u64;
        let payload = abc[2];
        self.gpu_write_u32(fd, va, payload).map_err(Some)
    }

    fn host_method(&mut self, fd: u32, call: Call) -> Result<(), IpcError> {
        let state = self.channel_state(fd);
        if state.host.is_empty() {
            state.host = vec![0; (ENGINE_METHODS / 4) as usize];
        }
        state.host[(call.method / 4) as usize] = call.value;
        let reg = |p: &mut Self, m: u32| p.channel_state(fd).host[(m / 4) as usize];
        let unimplemented = || {
            stop(format!(
                "GPU host method {:#x} = {:#x}",
                call.method, call.value
            ))
        };
        match call.method {
            // SET_OBJECT: NVCLASS (15:0) on the subchannel; ENGINE (20:16)
            // 0x1f is a software object.
            host::SET_OBJECT => {
                let class = call.value & 0xffff;
                if call.value >> 16 & 0x1f == 0x1f {
                    return Err(stop(format!("GPU software object {class:#x}")));
                }
                if super::table(class).is_none() {
                    return Err(stop(format!("GPU object class {class:#x}")));
                }
                let state = self.channel_state(fd);
                state.subchannels[call.subchannel as usize] = Some(class);
                state
                    .engines
                    .entry(class)
                    .or_insert_with(|| Engine::new(class));
                Ok(())
            }
            host::NOP
            | host::SEMAPHORE_A
            | host::SEMAPHORE_B
            | host::SEMAPHORE_C
            | host::MEM_OP_A
            | host::MEM_OP_B
            | host::MEM_OP_C
            | host::SET_REFERENCE
            | host::SYNCPOINT_A => Ok(()),
            // FB_FLUSH, MEM_OP_D (membars, TLB invalidations, L2 flushes),
            // WFI, YIELD: kestrel has no caches or TLB to flush, and runs
            // in order (kestrel's choice: nothing to do).
            host::FB_FLUSH | host::MEM_OP_D | host::WFI | host::YIELD => Ok(()),
            // SEMAPHORED: OPERATION (4:0) ACQUIRE (1), RELEASE (2), ACQ_GEQ
            // (4); RELEASE_SIZE (24) 4BYTE; the address in A (OFFSET_UPPER,
            // 7:0) and B (OFFSET_LOWER, 31:2), the payload in C.
            host::SEMAPHORE_D => {
                let va = ((reg(self, host::SEMAPHORE_A) & 0xff) as u64) << 32
                    | (reg(self, host::SEMAPHORE_B) & !3) as u64;
                let payload = reg(self, host::SEMAPHORE_C);
                match call.value & 0x1f {
                    2 if call.value >> 24 & 1 == 1 => self.gpu_write_u32(fd, va, payload),
                    op @ (1 | 4) => {
                        let have = self.gpu_read_u32(fd, va)?;
                        // ACQ_GEQ: kestrel's choice, a wrapping compare, as
                        // for syncpoints.
                        let met = if op == 1 {
                            have == payload
                        } else {
                            nv::reached(have, payload)
                        };
                        if met {
                            Ok(())
                        } else {
                            Err(stop(format!(
                                "a GPU semaphore acquire at {va:#x} for {payload:#x} (it holds \
                                 {have:#x}): nothing else would release it"
                            )))
                        }
                    }
                    _ => Err(unimplemented()),
                }
            }
            // SYNCPOINTB: OPERATION (1:0) WAIT (0) or INCR (1), the
            // syncpoint in SYNCPT_INDEX (15:8); WAIT's value in SYNCPOINTA.
            host::SYNCPOINT_B => {
                let id = call.value >> 8 & 0xff;
                match call.value & 3 {
                    0 => {
                        let want = reg(self, host::SYNCPOINT_A);
                        match self.services.nv.syncpoint(id) {
                            Some(sp) if nv::reached(sp.value, want) => Ok(()),
                            Some(sp) => Err(stop(format!(
                                "the GPU waiting for syncpoint {id} to reach {want} (it is at \
                                 {}): nothing else would increment it",
                                sp.value
                            ))),
                            None => Err(IpcError::Stop(format!(
                                "the GPU waiting on syncpoint {id}, which does not exist"
                            ))),
                        }
                    }
                    1 => self.syncpoint_incr(id).ok_or_else(|| {
                        IpcError::Stop(format!(
                            "the GPU incrementing syncpoint {id}, which does not exist"
                        ))
                    }),
                    _ => Err(unimplemented()),
                }
            }
            _ => Err(unimplemented()),
        }
    }
}

/// Whether a method only invalidates or flushes a cache.
fn is_cache_op(name: &str) -> bool {
    (name.starts_with("INVALIDATE_")
        || name.starts_with("FLUSH_")
        || name.starts_with("TWOD_INVALIDATE_"))
        && (name.contains("CACHE") || name == "FLUSH_PENDING_WRITES")
        || name == "INVALIDATE_ZCULL"
}

/// Store `value` at `*pointer` in `memory` and advance the pointer; Err
/// past `limit` words.
fn store(memory: &mut Vec<u32>, pointer: &mut u32, value: u32, limit: u32) -> Result<(), ()> {
    if *pointer >= limit {
        return Err(());
    }
    let at = *pointer as usize;
    if memory.len() <= at {
        memory.resize(at + 1, 0);
    }
    memory[at] = value;
    *pointer += 1;
    Ok(())
}
