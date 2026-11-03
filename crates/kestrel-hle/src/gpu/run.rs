//! Running a channel's command lists (see `gpu/mod.rs`).

use super::{class, decode, engine_name, method, Call, Engine, Entry, Kind};
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
            for call in calls {
                self.gpu_method(fd, call)?;
            }
        }
        Ok(())
    }

    /// The guest address behind `len` bytes at GPU address `va` in the
    /// address space channel `fd` is bound to: inside one mapping of an
    /// nvmap object with memory.
    pub(crate) fn gpu_address(&self, fd: u32, va: u64, len: u64) -> Result<u64, IpcError> {
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
            .find(|m| va >= m.start && va + len <= m.start + m.size)
            .ok_or_else(|| {
                IpcError::Stop(format!(
                    "the GPU reading or writing {len:#x} bytes at {va:#x}, which no mapping \
                     holds (a GPU page fault)"
                ))
            })?;
        let addr = nv.map(m.handle).and_then(|o| o.addr).ok_or_else(|| {
            IpcError::Stop(format!(
                "GPU address {va:#x} in nvmap object {}, which has no memory",
                m.handle
            ))
        })?;
        Ok(addr + m.buffer_offset + (va - m.start))
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

    fn channel_state(&mut self, fd: u32) -> &mut super::ChannelState {
        &mut self
            .services
            .nv
            .gpu
            .channels
            .get_mut(&fd)
            .expect("an open channel")
            .engine
    }

    /// One method write.
    fn gpu_method(&mut self, fd: u32, call: Call) -> Result<(), IpcError> {
        self.channel_state(fd).methods_run += 1;
        if call.method < ENGINE_METHODS {
            return self.host_method(fd, call);
        }
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
        e.regs[(call.method / 4) as usize] = call.value;
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
            (_, "NO_OPERATION") | (class::THREED, "WAIT_FOR_IDLE") => Ok(()),
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
