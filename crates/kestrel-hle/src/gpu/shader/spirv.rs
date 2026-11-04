//! Translating kestrel's shader IR to SPIR-V 1.0 (Vulkan 1.0), the
//! program the host GPU runs (PLAN.md, phase 5: one IR, SPIR-V out).
//!
//! The translation keeps Maxwell's model: each register used is a
//! function variable of 32-bit unsigned integers, each operation loads the
//! registers it reads, converts them to the type it works in (a bitcast
//! for floats) and stores its result; the host's compiler removes the
//! traffic. Constant buffer `i` of the stage is a uniform block of 64 KiB
//! (4096 uvec4), the most a Maxwell constant buffer holds; a 2D texture is
//! a combined image and sampler. Numbers are from the SPIR-V specification
//! (Khronos, unified 1.6 revision) and its GLSL.std.450 extended set.
//!
//! Bindings, all in descriptor set 0: the stage's base (Options) plus the
//! constant buffer's index, or plus 32 plus the texture's place in
//! `Module::textures`.

use std::collections::HashMap;

use super::ir::{FSrc, Interp, Mufu, Op, Operand, Program, Round, RZ};
use super::{attr, Stage};

/// How a vertex attribute reaches the shader: as floats, or as signed or
/// unsigned integers (the vertex format's numerical type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum AttrType {
    #[default]
    Float,
    Sint,
    Uint,
}

/// What the translation depends on besides the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Options {
    /// Vertex shaders: each generic attribute's type.
    pub vertex_inputs: [AttrType; 32],
    /// Vertex shaders: clip z from -w..w (OpenGL's convention, Maxwell's
    /// SET_VIEWPORT_Z_CLIP NEGATIVE_W_TO_POSITIVE_W) rather than 0..w
    /// (Vulkan's): z becomes (z + w) / 2 on the way out.
    pub z_minus_one_to_one: bool,
    /// The first binding this stage's resources use.
    pub binding_base: u32,
}

/// A translated program and the resources it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Module {
    pub words: Vec<u32>,
    /// The constant buffers it reads, by index, and their bindings.
    pub uniform_buffers: Vec<(u8, u32)>,
    /// The textures it samples, by handle word, and their bindings.
    pub textures: Vec<(u16, u32)>,
    /// Vertex shaders: the generic attributes read (bit per location).
    pub inputs: u32,
    /// Pixel shaders: each generic input's interpolation.
    pub interpolation: Vec<(u32, Interp)>,
}

/// Each constant buffer is this many bytes.
pub const UNIFORM_BYTES: u32 = 0x1_0000;

mod op {
    pub const EXT_INST_IMPORT: u16 = 11;
    pub const EXT_INST: u16 = 12;
    pub const MEMORY_MODEL: u16 = 14;
    pub const ENTRY_POINT: u16 = 15;
    pub const EXECUTION_MODE: u16 = 16;
    pub const CAPABILITY: u16 = 17;
    pub const TYPE_VOID: u16 = 19;
    pub const TYPE_BOOL: u16 = 20;
    pub const TYPE_INT: u16 = 21;
    pub const TYPE_FLOAT: u16 = 22;
    pub const TYPE_VECTOR: u16 = 23;
    pub const TYPE_IMAGE: u16 = 25;
    pub const TYPE_SAMPLED_IMAGE: u16 = 27;
    pub const TYPE_ARRAY: u16 = 28;
    pub const TYPE_STRUCT: u16 = 30;
    pub const TYPE_POINTER: u16 = 32;
    pub const TYPE_FUNCTION: u16 = 33;
    pub const CONSTANT: u16 = 43;
    pub const FUNCTION: u16 = 54;
    pub const FUNCTION_END: u16 = 56;
    pub const VARIABLE: u16 = 59;
    pub const LOAD: u16 = 61;
    pub const STORE: u16 = 62;
    pub const ACCESS_CHAIN: u16 = 65;
    pub const DECORATE: u16 = 71;
    pub const MEMBER_DECORATE: u16 = 72;
    pub const COMPOSITE_CONSTRUCT: u16 = 80;
    pub const COMPOSITE_EXTRACT: u16 = 81;
    pub const IMAGE_SAMPLE_IMPLICIT_LOD: u16 = 87;
    pub const CONVERT_F_TO_U: u16 = 109;
    pub const CONVERT_F_TO_S: u16 = 110;
    pub const CONVERT_S_TO_F: u16 = 111;
    pub const CONVERT_U_TO_F: u16 = 112;
    pub const BITCAST: u16 = 124;
    pub const F_NEGATE: u16 = 127;
    pub const I_ADD: u16 = 128;
    pub const F_ADD: u16 = 129;
    pub const F_MUL: u16 = 133;
    pub const F_DIV: u16 = 136;
    pub const IS_NAN: u16 = 156;
    pub const SELECT: u16 = 169;
    pub const U_LESS_THAN: u16 = 176;
    pub const F_ORD_GREATER_THAN_EQUAL: u16 = 190;
    pub const SHIFT_RIGHT_LOGICAL: u16 = 194;
    pub const SHIFT_LEFT_LOGICAL: u16 = 196;
    pub const BITWISE_AND: u16 = 199;
    pub const LABEL: u16 = 248;
    pub const RETURN: u16 = 253;
}

/// GLSL.std.450 instructions.
mod glsl {
    pub const ROUND_EVEN: u32 = 2;
    pub const TRUNC: u32 = 3;
    pub const F_ABS: u32 = 4;
    pub const FLOOR: u32 = 8;
    pub const CEIL: u32 = 9;
    pub const EXP2: u32 = 29;
    pub const LOG2: u32 = 30;
    pub const SQRT: u32 = 31;
    pub const INVERSE_SQRT: u32 = 32;
    pub const U_MIN: u32 = 38;
    pub const F_CLAMP: u32 = 43;
    pub const FMA: u32 = 50;
}

mod deco {
    pub const BLOCK: u32 = 2;
    pub const ARRAY_STRIDE: u32 = 6;
    pub const BUILT_IN: u32 = 11;
    pub const NO_PERSPECTIVE: u32 = 13;
    pub const FLAT: u32 = 14;
    pub const LOCATION: u32 = 30;
    pub const BINDING: u32 = 33;
    pub const DESCRIPTOR_SET: u32 = 34;
    pub const OFFSET: u32 = 35;
}

mod builtin {
    pub const POSITION: u32 = 0;
    pub const FRAG_COORD: u32 = 15;
    pub const VERTEX_INDEX: u32 = 42;
    pub const INSTANCE_INDEX: u32 = 43;
}

mod storage {
    pub const UNIFORM_CONSTANT: u32 = 0;
    pub const INPUT: u32 = 1;
    pub const UNIFORM: u32 = 2;
    pub const OUTPUT: u32 = 3;
    pub const FUNCTION: u32 = 7;
}

fn inst(out: &mut Vec<u32>, op: u16, operands: &[u32]) {
    out.push((operands.len() as u32 + 1) << 16 | op as u32);
    out.extend_from_slice(operands);
}

/// A literal string's words (nul-terminated, padded).
fn string(s: &str) -> Vec<u32> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
        .chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().expect("four bytes")))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Ty {
    Void,
    Bool,
    U32,
    I32,
    F32,
    Vec(u8, u32),
    Ptr(u32, u32),
}

struct Builder {
    next: u32,
    annotations: Vec<u32>,
    globals: Vec<u32>,
    vars: Vec<u32>,
    body: Vec<u32>,
    types: HashMap<Ty, u32>,
    consts: HashMap<(u32, u32), u32>,
    interface: Vec<u32>,
    glsl: u32,
}

impl Builder {
    fn id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    fn ty(&mut self, t: Ty) -> u32 {
        if let Some(&id) = self.types.get(&t) {
            return id;
        }
        let operands: Vec<u32> = match t {
            Ty::Void | Ty::Bool => vec![],
            Ty::U32 => vec![32, 0],
            Ty::I32 => vec![32, 1],
            Ty::F32 => vec![32],
            Ty::Vec(n, elem) => vec![elem, n as u32],
            Ty::Ptr(class, pointee) => vec![class, pointee],
        };
        let id = self.id();
        let op = match t {
            Ty::Void => op::TYPE_VOID,
            Ty::Bool => op::TYPE_BOOL,
            Ty::U32 | Ty::I32 => op::TYPE_INT,
            Ty::F32 => op::TYPE_FLOAT,
            Ty::Vec(..) => op::TYPE_VECTOR,
            Ty::Ptr(..) => op::TYPE_POINTER,
        };
        let mut words = vec![id];
        words.extend(operands);
        inst(&mut self.globals, op, &words);
        self.types.insert(t, id);
        id
    }

    fn u32_ty(&mut self) -> u32 {
        self.ty(Ty::U32)
    }

    fn f32_ty(&mut self) -> u32 {
        self.ty(Ty::F32)
    }

    fn vec_ty(&mut self, n: u8, elem: Ty) -> u32 {
        let e = self.ty(elem);
        self.ty(Ty::Vec(n, e))
    }

    fn ptr(&mut self, class: u32, pointee: u32) -> u32 {
        self.ty(Ty::Ptr(class, pointee))
    }

    fn constant(&mut self, ty: u32, bits: u32) -> u32 {
        if let Some(&id) = self.consts.get(&(ty, bits)) {
            return id;
        }
        let id = self.id();
        inst(&mut self.globals, op::CONSTANT, &[ty, id, bits]);
        self.consts.insert((ty, bits), id);
        id
    }

    fn uint(&mut self, v: u32) -> u32 {
        let t = self.u32_ty();
        self.constant(t, v)
    }

    fn int(&mut self, v: i32) -> u32 {
        let t = self.ty(Ty::I32);
        self.constant(t, v as u32)
    }

    fn float(&mut self, v: f32) -> u32 {
        let t = self.f32_ty();
        self.constant(t, v.to_bits())
    }

    fn decorate(&mut self, target: u32, decoration: u32, extra: &[u32]) {
        let mut w = vec![target, decoration];
        w.extend_from_slice(extra);
        inst(&mut self.annotations, op::DECORATE, &w);
    }

    /// A global variable.
    fn global(&mut self, class: u32, pointee: u32) -> u32 {
        let p = self.ptr(class, pointee);
        let id = self.id();
        inst(&mut self.globals, op::VARIABLE, &[p, id, class]);
        if class == storage::INPUT || class == storage::OUTPUT {
            self.interface.push(id);
        }
        id
    }

    /// An instruction in the function body with a result.
    fn op(&mut self, op: u16, ty: u32, operands: &[u32]) -> u32 {
        let id = self.id();
        let mut w = vec![ty, id];
        w.extend_from_slice(operands);
        inst(&mut self.body, op, &w);
        id
    }

    fn ext(&mut self, ty: u32, instruction: u32, args: &[u32]) -> u32 {
        let mut w = vec![self.glsl, instruction];
        w.extend_from_slice(args);
        self.op(op::EXT_INST, ty, &w)
    }

    fn load(&mut self, ty: u32, ptr: u32) -> u32 {
        self.op(op::LOAD, ty, &[ptr])
    }

    fn store(&mut self, ptr: u32, value: u32) {
        inst(&mut self.body, op::STORE, &[ptr, value]);
    }

    fn chain(&mut self, class: u32, pointee: u32, base: u32, indices: &[u32]) -> u32 {
        let p = self.ptr(class, pointee);
        let mut w = vec![base];
        w.extend_from_slice(indices);
        self.op(op::ACCESS_CHAIN, p, &w)
    }

    fn bitcast(&mut self, ty: u32, v: u32) -> u32 {
        self.op(op::BITCAST, ty, &[v])
    }
}

/// The translation's state: the builder and the variables made so far.
struct Translation<'a> {
    b: Builder,
    program: &'a Program,
    opts: &'a Options,
    regs: HashMap<u8, u32>,
    ubos: HashMap<u8, u32>,
    /// Each texture: its handle word, binding, variable and sampled image
    /// type.
    textures: Vec<(u16, u32, u32, u32)>,
    inputs: HashMap<u32, u32>,
    input_types: HashMap<u32, AttrType>,
    interpolation: HashMap<u32, Interp>,
    outputs: HashMap<u32, u32>,
    per_vertex: Option<u32>,
    vertex_index: Option<u32>,
    instance_index: Option<u32>,
    frag_coord: Option<u32>,
}

/// Translate `program`.
pub fn emit(program: &Program, opts: &Options) -> Result<Module, String> {
    if program.header.writes_depth || program.header.writes_sample_mask {
        return Err("a pixel shader writing depth or the sample mask".into());
    }
    let mut b = Builder {
        next: 0,
        annotations: Vec::new(),
        globals: Vec::new(),
        vars: Vec::new(),
        body: Vec::new(),
        types: HashMap::new(),
        consts: HashMap::new(),
        interface: Vec::new(),
        glsl: 0,
    };
    b.glsl = b.id();
    let main = b.id();
    let mut t = Translation {
        b,
        program,
        opts,
        regs: HashMap::new(),
        ubos: HashMap::new(),
        textures: Vec::new(),
        inputs: HashMap::new(),
        input_types: HashMap::new(),
        interpolation: HashMap::new(),
        outputs: HashMap::new(),
        per_vertex: None,
        vertex_index: None,
        instance_index: None,
        frag_coord: None,
    };
    for o in &program.ops {
        t.translate(o)?;
    }
    let stage = program.header.stage;
    let Translation {
        mut b,
        ubos,
        textures,
        interpolation,
        input_types,
        ..
    } = t;
    // Assemble: the header, then the sections in the order SPIR-V asks.
    let void = b.ty(Ty::Void);
    let fn_ty = b.id();
    inst(&mut b.globals, op::TYPE_FUNCTION, &[fn_ty, void]);
    let mut words = vec![0x0723_0203, 0x0001_0000, 0, 0, 0];
    inst(&mut words, op::CAPABILITY, &[1]);
    let mut ext = vec![b.glsl];
    ext.extend(string("GLSL.std.450"));
    inst(&mut words, op::EXT_INST_IMPORT, &ext);
    inst(&mut words, op::MEMORY_MODEL, &[0, 1]);
    let model = if stage == Stage::Vertex { 0 } else { 4 };
    let mut entry = vec![model, main];
    entry.extend(string("main"));
    entry.extend(&b.interface);
    inst(&mut words, op::ENTRY_POINT, &entry);
    if stage == Stage::Fragment {
        inst(&mut words, op::EXECUTION_MODE, &[main, 7]);
    }
    words.extend(&b.annotations);
    words.extend(&b.globals);
    let label = b.id();
    inst(&mut words, op::FUNCTION, &[void, main, 0, fn_ty]);
    inst(&mut words, op::LABEL, &[label]);
    words.extend(&b.vars);
    words.extend(&b.body);
    inst(&mut words, op::RETURN, &[]);
    inst(&mut words, op::FUNCTION_END, &[]);
    words[3] = b.next + 1;
    let mut uniform_buffers: Vec<(u8, u32)> = ubos
        .keys()
        .map(|&i| (i, opts.binding_base + i as u32))
        .collect();
    uniform_buffers.sort();
    let mut interp: Vec<(u32, Interp)> = interpolation.into_iter().collect();
    interp.sort_by_key(|&(l, _)| l);
    Ok(Module {
        words,
        uniform_buffers,
        textures: textures
            .iter()
            .map(|&(h, binding, ..)| (h, binding))
            .collect(),
        inputs: input_types.keys().fold(0, |m, &l| m | 1 << l),
        interpolation: interp,
    })
}

impl Translation<'_> {
    fn stage(&self) -> Stage {
        self.program.header.stage
    }

    /// Register `r`'s variable.
    fn reg_var(&mut self, r: u8) -> u32 {
        if let Some(&v) = self.regs.get(&r) {
            return v;
        }
        let u = self.b.u32_ty();
        let p = self.b.ptr(storage::FUNCTION, u);
        let id = self.b.id();
        inst(&mut self.b.vars, op::VARIABLE, &[p, id, storage::FUNCTION]);
        self.regs.insert(r, id);
        id
    }

    fn read_reg(&mut self, r: u8) -> u32 {
        if r == RZ {
            return self.b.uint(0);
        }
        let v = self.reg_var(r);
        let u = self.b.u32_ty();
        self.b.load(u, v)
    }

    fn write_reg(&mut self, r: u8, value: u32) {
        if r != RZ {
            let v = self.reg_var(r);
            self.b.store(v, value);
        }
    }

    fn write_float(&mut self, r: u8, value: u32) {
        let u = self.b.u32_ty();
        let bits = self.b.bitcast(u, value);
        self.write_reg(r, bits);
    }

    /// Constant buffer `index`'s uniform block.
    fn ubo(&mut self, index: u8) -> u32 {
        if let Some(&v) = self.ubos.get(&index) {
            return v;
        }
        let uvec4 = self.b.vec_ty(4, Ty::U32);
        let len = self.b.uint(UNIFORM_BYTES / 16);
        let arr = self.b.id();
        inst(&mut self.b.globals, op::TYPE_ARRAY, &[arr, uvec4, len]);
        self.b.decorate(arr, deco::ARRAY_STRIDE, &[16]);
        let st = self.b.id();
        inst(&mut self.b.globals, op::TYPE_STRUCT, &[st, arr]);
        self.b.decorate(st, deco::BLOCK, &[]);
        inst(
            &mut self.b.annotations,
            op::MEMBER_DECORATE,
            &[st, 0, deco::OFFSET, 0],
        );
        let var = self.b.global(storage::UNIFORM, st);
        self.b.decorate(var, deco::DESCRIPTOR_SET, &[0]);
        let binding = self.opts.binding_base + index as u32;
        self.b.decorate(var, deco::BINDING, &[binding]);
        self.ubos.insert(index, var);
        var
    }

    /// An operand's 32 bits.
    fn read(&mut self, o: Operand) -> Result<u32, String> {
        Ok(match o {
            Operand::Reg(r) => self.read_reg(r),
            Operand::Imm(v) => self.b.uint(v),
            Operand::Cbuf { index, offset, reg } => {
                if index >= 18 {
                    return Err(format!("constant buffer {index}"));
                }
                let var = self.ubo(index);
                let u = self.b.u32_ty();
                let zero = self.b.int(0);
                if reg == RZ {
                    // A word past the buffer reads 0, as Maxwell's do.
                    if offset < 0 || offset as u32 >= UNIFORM_BYTES {
                        return Ok(self.b.uint(0));
                    }
                    let vec = self.b.int(offset / 16);
                    let comp = self.b.int(offset / 4 % 4);
                    let p = self.b.chain(storage::UNIFORM, u, var, &[zero, vec, comp]);
                    self.b.load(u, p)
                } else {
                    let base = self.read_reg(reg);
                    let off = self.b.uint(offset as u32);
                    let addr = self.b.op(op::I_ADD, u, &[base, off]);
                    let four = self.b.uint(4);
                    let two = self.b.uint(2);
                    let vec = self.b.op(op::SHIFT_RIGHT_LOGICAL, u, &[addr, four]);
                    let last = self.b.uint(UNIFORM_BYTES / 16 - 1);
                    let vec = self.b.ext(u, glsl::U_MIN, &[vec, last]);
                    let w = self.b.op(op::SHIFT_RIGHT_LOGICAL, u, &[addr, two]);
                    let three = self.b.uint(3);
                    let comp = self.b.op(op::BITWISE_AND, u, &[w, three]);
                    let p = self.b.chain(storage::UNIFORM, u, var, &[zero, vec, comp]);
                    let value = self.b.load(u, p);
                    // Past the buffer's end: 0.
                    let bool_ty = self.b.ty(Ty::Bool);
                    let limit = self.b.uint(UNIFORM_BYTES);
                    let inside = self.b.op(op::U_LESS_THAN, bool_ty, &[addr, limit]);
                    let z = self.b.uint(0);
                    self.b.op(op::SELECT, u, &[inside, value, z])
                }
            }
        })
    }

    /// A float operand, modifiers applied.
    fn read_f(&mut self, s: FSrc) -> Result<u32, String> {
        let bits = self.read(s.op)?;
        let f = self.b.f32_ty();
        let mut v = self.b.bitcast(f, bits);
        if s.abs {
            v = self.b.ext(f, glsl::F_ABS, &[v]);
        }
        if s.neg {
            v = self.b.op(op::F_NEGATE, f, &[v]);
        }
        Ok(v)
    }

    fn saturate(&mut self, v: u32, sat: bool) -> u32 {
        if !sat {
            return v;
        }
        let f = self.b.f32_ty();
        let (zero, one) = (self.b.float(0.0), self.b.float(1.0));
        self.b.ext(f, glsl::F_CLAMP, &[v, zero, one])
    }

    fn round(&mut self, v: u32, r: Round) -> u32 {
        let f = self.b.f32_ty();
        let i = match r {
            Round::Nearest => glsl::ROUND_EVEN,
            Round::Floor => glsl::FLOOR,
            Round::Ceil => glsl::CEIL,
            Round::Trunc => glsl::TRUNC,
        };
        self.b.ext(f, i, &[v])
    }

    /// The vertex shader's gl_PerVertex block (Position only).
    fn per_vertex(&mut self) -> u32 {
        if let Some(v) = self.per_vertex {
            return v;
        }
        let v4 = self.b.vec_ty(4, Ty::F32);
        let st = self.b.id();
        inst(&mut self.b.globals, op::TYPE_STRUCT, &[st, v4]);
        self.b.decorate(st, deco::BLOCK, &[]);
        inst(
            &mut self.b.annotations,
            op::MEMBER_DECORATE,
            &[st, 0, deco::BUILT_IN, builtin::POSITION],
        );
        let var = self.b.global(storage::OUTPUT, st);
        self.per_vertex = Some(var);
        var
    }

    fn builtin_input(&mut self, ty: u32, which: u32) -> u32 {
        let var = self.b.global(storage::INPUT, ty);
        self.b.decorate(var, deco::BUILT_IN, &[which]);
        var
    }

    /// Generic input `loc` (a vec4), with its type or interpolation.
    fn input(&mut self, loc: u32, ty: AttrType, interp: Option<Interp>) -> Result<u32, String> {
        if let Some(&v) = self.inputs.get(&loc) {
            if let Some(i) = interp {
                if self.interpolation.get(&loc) != Some(&i) {
                    return Err(format!("generic input {loc} interpolated in two ways"));
                }
            }
            return Ok(v);
        }
        let elem = match ty {
            AttrType::Float => Ty::F32,
            AttrType::Sint => Ty::I32,
            AttrType::Uint => Ty::U32,
        };
        let v4 = self.b.vec_ty(4, elem);
        let var = self.b.global(storage::INPUT, v4);
        self.b.decorate(var, deco::LOCATION, &[loc]);
        match interp {
            Some(Interp::Flat) => self.b.decorate(var, deco::FLAT, &[]),
            Some(Interp::Linear) => self.b.decorate(var, deco::NO_PERSPECTIVE, &[]),
            _ => {}
        }
        if let Some(i) = interp {
            self.interpolation.insert(loc, i);
        }
        self.input_types.insert(loc, ty);
        self.inputs.insert(loc, var);
        Ok(var)
    }

    /// Generic output `loc` (a vec4 of floats).
    fn output(&mut self, loc: u32) -> u32 {
        if let Some(&v) = self.outputs.get(&loc) {
            return v;
        }
        let v4 = self.b.vec_ty(4, Ty::F32);
        let var = self.b.global(storage::OUTPUT, v4);
        self.b.decorate(var, deco::LOCATION, &[loc]);
        self.outputs.insert(loc, var);
        var
    }

    fn frag_coord(&mut self) -> u32 {
        if let Some(v) = self.frag_coord {
            return v;
        }
        let v4 = self.b.vec_ty(4, Ty::F32);
        let var = self.builtin_input(v4, builtin::FRAG_COORD);
        self.frag_coord = Some(var);
        var
    }

    /// Component `c` of a vec4 input or output variable, as a float.
    fn component(&mut self, class: u32, var: u32, elem: Ty, c: u32) -> u32 {
        let t = self.b.ty(elem);
        let i = self.b.int(c as i32);
        self.b.chain(class, t, var, &[i])
    }

    /// Load the attribute word at `addr` (vertex shaders).
    fn load_attr(&mut self, addr: u16) -> Result<u32, String> {
        let u = self.b.u32_ty();
        match addr {
            attr::GENERIC..attr::GENERIC_END => {
                let loc = ((addr - attr::GENERIC) / 16) as u32;
                let ty = self.opts.vertex_inputs[loc as usize];
                let var = self.input(loc, ty, None)?;
                let elem = match ty {
                    AttrType::Float => Ty::F32,
                    AttrType::Sint => Ty::I32,
                    AttrType::Uint => Ty::U32,
                };
                let p = self.component(storage::INPUT, var, elem, (addr as u32 / 4) % 4);
                let t = self.b.ty(elem);
                let v = self.b.load(t, p);
                Ok(if elem == Ty::U32 {
                    v
                } else {
                    self.b.bitcast(u, v)
                })
            }
            attr::VERTEX_ID | attr::INSTANCE_ID => {
                let slot = if addr == attr::VERTEX_ID {
                    &mut self.vertex_index
                } else {
                    &mut self.instance_index
                };
                let var = match *slot {
                    Some(v) => v,
                    None => {
                        let i = self.b.ty(Ty::I32);
                        let which = if addr == attr::VERTEX_ID {
                            builtin::VERTEX_INDEX
                        } else {
                            builtin::INSTANCE_INDEX
                        };
                        let v = self.builtin_input(i, which);
                        if addr == attr::VERTEX_ID {
                            self.vertex_index = Some(v);
                        } else {
                            self.instance_index = Some(v);
                        }
                        v
                    }
                };
                let i = self.b.ty(Ty::I32);
                let v = self.b.load(i, var);
                Ok(self.b.bitcast(u, v))
            }
            _ => Err(format!("vertex attribute input {addr:#x}")),
        }
    }

    fn store_attr(&mut self, addr: u16, value: u32) -> Result<(), String> {
        let f = self.b.f32_ty();
        let v = self.b.bitcast(f, value);
        let p = match addr {
            0x70..=0x7c => {
                let block = self.per_vertex();
                let zero = self.b.int(0);
                let c = self.b.int((addr as i32 - 0x70) / 4);
                self.b.chain(storage::OUTPUT, f, block, &[zero, c])
            }
            attr::GENERIC..attr::GENERIC_END => {
                let loc = ((addr - attr::GENERIC) / 16) as u32;
                let var = self.output(loc);
                self.component(storage::OUTPUT, var, Ty::F32, (addr as u32 / 4) % 4)
            }
            _ => return Err(format!("vertex attribute output {addr:#x}")),
        };
        self.b.store(p, v);
        Ok(())
    }

    fn translate(&mut self, o: &Op) -> Result<(), String> {
        let f = self.b.f32_ty();
        let u = self.b.u32_ty();
        match *o {
            Op::Mov { dst, src } => {
                let v = self.read(src)?;
                self.write_reg(dst, v);
            }
            Op::FAdd { dst, a, b, sat } => {
                let (x, y) = (self.read_f(a)?, self.read_f(b)?);
                let r = self.b.op(op::F_ADD, f, &[x, y]);
                let r = self.saturate(r, sat);
                self.write_float(dst, r);
            }
            Op::FMul {
                dst,
                a,
                b,
                scale,
                sat,
            } => {
                let (x, y) = (self.read_f(a)?, self.read_f(b)?);
                let mut r = self.b.op(op::F_MUL, f, &[x, y]);
                if scale != 0 {
                    let k = self.b.float(2f32.powi(scale as i32));
                    r = self.b.op(op::F_MUL, f, &[r, k]);
                }
                let r = self.saturate(r, sat);
                self.write_float(dst, r);
            }
            Op::FFma { dst, a, b, c, sat } => {
                let (x, y, z) = (self.read_f(a)?, self.read_f(b)?, self.read_f(c)?);
                let r = self.b.ext(f, glsl::FMA, &[x, y, z]);
                let r = self.saturate(r, sat);
                self.write_float(dst, r);
            }
            Op::Mufu {
                dst,
                func,
                src,
                sat,
            } => {
                let x = self.read_f(src)?;
                let r = match func {
                    Mufu::Rcp => {
                        let one = self.b.float(1.0);
                        self.b.op(op::F_DIV, f, &[one, x])
                    }
                    Mufu::Rsq => self.b.ext(f, glsl::INVERSE_SQRT, &[x]),
                    Mufu::Ex2 => self.b.ext(f, glsl::EXP2, &[x]),
                    Mufu::Lg2 => self.b.ext(f, glsl::LOG2, &[x]),
                    Mufu::Sqrt => self.b.ext(f, glsl::SQRT, &[x]),
                };
                let r = self.saturate(r, sat);
                self.write_float(dst, r);
            }
            Op::Shl { dst, a, b, wrap } => {
                let (x, s) = (self.read(a)?, self.read(b)?);
                let mask = self.b.uint(31);
                let s5 = self.b.op(op::BITWISE_AND, u, &[s, mask]);
                let shifted = self.b.op(op::SHIFT_LEFT_LOGICAL, u, &[x, s5]);
                let r = if wrap {
                    shifted
                } else {
                    // A shift of 32 or more gives 0.
                    let bool_ty = self.b.ty(Ty::Bool);
                    let n = self.b.uint(32);
                    let small = self.b.op(op::U_LESS_THAN, bool_ty, &[s, n]);
                    let zero = self.b.uint(0);
                    self.b.op(op::SELECT, u, &[small, shifted, zero])
                };
                self.write_reg(dst, r);
            }
            Op::I2F {
                dst,
                src,
                signed,
                neg,
                abs,
            } => {
                let x = self.read(src)?;
                let mut r = if signed {
                    let i = self.b.ty(Ty::I32);
                    let xi = self.b.bitcast(i, x);
                    self.b.op(op::CONVERT_S_TO_F, f, &[xi])
                } else {
                    self.b.op(op::CONVERT_U_TO_F, f, &[x])
                };
                if abs {
                    r = self.b.ext(f, glsl::F_ABS, &[r]);
                }
                if neg {
                    r = self.b.op(op::F_NEGATE, f, &[r]);
                }
                self.write_float(dst, r);
            }
            Op::F2I {
                dst,
                src,
                signed,
                bits,
                round,
            } => {
                // Rounded, then saturated to the integer's range; NaN is 0
                // (Maxwell's conversions saturate; the host's would be
                // undefined out of range).
                let x = self.read_f(src)?;
                let r = self.round(x, round);
                let bool_ty = self.b.ty(Ty::Bool);
                let nan = self.b.op(op::IS_NAN, bool_ty, &[r]);
                // Below 32 bits every limit is a float exactly; at 32 the
                // largest float under 2^31 (2^32) is the limit and values
                // from 2^31 (2^32) up give the largest integer.
                let (lo, hi, top, max) = match (signed, bits) {
                    (true, 32) => (
                        -2_147_483_648.0f32,
                        2_147_483_520.0f32,
                        2_147_483_648.0f32,
                        i32::MAX as u32,
                    ),
                    (false, 32) => (0.0, 4_294_967_040.0, 4_294_967_296.0, u32::MAX),
                    (true, n) => {
                        let m = (1u32 << (n - 1)) as f32;
                        (-m, m - 1.0, m, (1u32 << (n - 1)) - 1)
                    }
                    (false, n) => {
                        let m = (1u32 << n) as f32;
                        (0.0, m - 1.0, m, (1u32 << n) - 1)
                    }
                };
                let (lo, hi, top) = (self.b.float(lo), self.b.float(hi), self.b.float(top));
                let c = self.b.ext(f, glsl::F_CLAMP, &[r, lo, hi]);
                let v = if signed {
                    let i = self.b.ty(Ty::I32);
                    let vi = self.b.op(op::CONVERT_F_TO_S, i, &[c]);
                    self.b.bitcast(u, vi)
                } else {
                    self.b.op(op::CONVERT_F_TO_U, u, &[c])
                };
                let big = self.b.op(op::F_ORD_GREATER_THAN_EQUAL, bool_ty, &[r, top]);
                let max = self.b.uint(max);
                let v = self.b.op(op::SELECT, u, &[big, max, v]);
                let zero = self.b.uint(0);
                let v = self.b.op(op::SELECT, u, &[nan, zero, v]);
                self.write_reg(dst, v);
            }
            Op::F2F {
                dst,
                src,
                round,
                sat,
            } => {
                let x = self.read_f(src)?;
                let r = match round {
                    Some(r) => self.round(x, r),
                    None => x,
                };
                let r = self.saturate(r, sat);
                self.write_float(dst, r);
            }
            Op::LdAttr { dst, count, addr } => {
                if self.stage() != Stage::Vertex {
                    return Err("an attribute load outside a vertex shader".into());
                }
                for k in 0..count {
                    let v = self.load_attr(addr + 4 * k as u16)?;
                    self.write_reg(dst.wrapping_add(k), v);
                }
            }
            Op::StAttr { src, count, addr } => {
                for k in 0..count {
                    let v = self.read_reg(src.wrapping_add(k));
                    self.store_attr(addr + 4 * k as u16, v)?;
                }
            }
            Op::Ipa {
                dst,
                addr,
                mode,
                mult,
                sat,
            } => {
                let v = match addr {
                    0x70..=0x7c if mode == Interp::Linear => {
                        let var = self.frag_coord();
                        let p =
                            self.component(storage::INPUT, var, Ty::F32, (addr as u32 - 0x70) / 4);
                        self.b.load(f, p)
                    }
                    attr::GENERIC..attr::GENERIC_END => {
                        let loc = ((addr - attr::GENERIC) / 16) as u32;
                        let var = self.input(loc, AttrType::Float, Some(mode))?;
                        let p = self.component(storage::INPUT, var, Ty::F32, (addr as u32 / 4) % 4);
                        let v = self.b.load(f, p);
                        if mode == Interp::Perspective {
                            // Maxwell interpolates value / w and multiplies
                            // by `mult` (programs pass w: 1 / FragCoord.w,
                            // NAK's load_frag_w); the host interpolates the
                            // value itself, so: value * mult * FragCoord.w.
                            let m = self.read_reg(mult);
                            let mf = self.b.bitcast(f, m);
                            let fc = self.frag_coord();
                            let pw = self.component(storage::INPUT, fc, Ty::F32, 3);
                            let w = self.b.load(f, pw);
                            let vm = self.b.op(op::F_MUL, f, &[v, mf]);
                            self.b.op(op::F_MUL, f, &[vm, w])
                        } else {
                            v
                        }
                    }
                    _ => return Err(format!("pixel shader input {addr:#x} ({mode:?})")),
                };
                let v = self.saturate(v, sat);
                self.write_float(dst, v);
            }
            Op::Tex2d {
                dsts,
                mask,
                x,
                y,
                handle,
            } => {
                if self.stage() != Stage::Fragment {
                    return Err("a texture sample outside a pixel shader".into());
                }
                let (var, sampled) = match self.textures.iter().find(|t| t.0 == handle) {
                    Some(t) => (t.2, t.3),
                    None => {
                        let image = self.b.id();
                        // OpTypeImage: float, 2D, not depth, not arrayed,
                        // single-sampled, sampled, unknown format.
                        inst(
                            &mut self.b.globals,
                            op::TYPE_IMAGE,
                            &[image, f, 1, 0, 0, 0, 1, 0],
                        );
                        let sampled = self.b.id();
                        inst(
                            &mut self.b.globals,
                            op::TYPE_SAMPLED_IMAGE,
                            &[sampled, image],
                        );
                        let var = self.b.global(storage::UNIFORM_CONSTANT, sampled);
                        let binding = self.opts.binding_base + 32 + self.textures.len() as u32;
                        self.b.decorate(var, deco::DESCRIPTOR_SET, &[0]);
                        self.b.decorate(var, deco::BINDING, &[binding]);
                        self.textures.push((handle, binding, var, sampled));
                        (var, sampled)
                    }
                };
                let s = self.b.load(sampled, var);
                let (xs, ys) = (self.read_reg(x), self.read_reg(y));
                let (xf, yf) = (self.b.bitcast(f, xs), self.b.bitcast(f, ys));
                let v2 = self.b.vec_ty(2, Ty::F32);
                let coord = self.b.op(op::COMPOSITE_CONSTRUCT, v2, &[xf, yf]);
                let v4 = self.b.vec_ty(4, Ty::F32);
                let texel = self.b.op(op::IMAGE_SAMPLE_IMPLICIT_LOD, v4, &[s, coord]);
                let targets = [
                    dsts[0],
                    dsts[0].wrapping_add(1),
                    dsts[1],
                    dsts[1].wrapping_add(1),
                ];
                let mut k = 0;
                for c in 0..4 {
                    if mask >> c & 1 == 1 {
                        let comp = self.b.op(op::COMPOSITE_EXTRACT, f, &[texel, c]);
                        let base = targets[k];
                        if (k < 2 && dsts[0] != RZ) || (k >= 2 && dsts[1] != RZ) {
                            self.write_float(base, comp);
                        }
                        k += 1;
                    }
                }
            }
            Op::Exit => self.exit()?,
        }
        Ok(())
    }

    fn exit(&mut self) -> Result<(), String> {
        let f = self.b.f32_ty();
        match self.stage() {
            Stage::Vertex => {
                if self.opts.z_minus_one_to_one && self.per_vertex.is_some() {
                    let block = self.per_vertex();
                    let zero = self.b.int(0);
                    let (two, three) = (self.b.int(2), self.b.int(3));
                    let pz = self.b.chain(storage::OUTPUT, f, block, &[zero, two]);
                    let pw = self.b.chain(storage::OUTPUT, f, block, &[zero, three]);
                    let (z, w) = (self.b.load(f, pz), self.b.load(f, pw));
                    let sum = self.b.op(op::F_ADD, f, &[z, w]);
                    let half = self.b.float(0.5);
                    let nz = self.b.op(op::F_MUL, f, &[sum, half]);
                    self.b.store(pz, nz);
                }
            }
            Stage::Fragment => {
                // Each colour target the header lists takes the next four
                // registers from R0 (NAK: per-output vec4s, in order).
                let targets = self.program.header.omap_targets;
                let mut r = 0u8;
                for t in 0..8 {
                    if targets >> (4 * t) & 0xf == 0 {
                        continue;
                    }
                    let v4 = self.b.vec_ty(4, Ty::F32);
                    let var = self.b.global(storage::OUTPUT, v4);
                    self.b.decorate(var, deco::LOCATION, &[t]);
                    let mut comps = Vec::new();
                    for k in 0..4 {
                        let bits = self.read_reg(r + k);
                        comps.push(self.b.bitcast(f, bits));
                    }
                    let v = self.b.op(op::COMPOSITE_CONSTRUCT, v4, &comps);
                    self.b.store(var, v);
                    r += 4;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::shader::ir::{FSrc, Interp, Mufu, Op, Operand, Program, Round, RZ};
    use crate::gpu::shader::{Header, Stage};

    fn header(stage: Stage) -> Header {
        Header {
            stage,
            omap_targets: if stage == Stage::Fragment { 0xf } else { 0 },
            writes_depth: false,
            writes_sample_mask: false,
            kills_pixels: false,
        }
    }

    /// Validate with Khronos' spirv-val when it is installed (it is not a
    /// dependency: without it, only the module's shape is checked).
    fn validate(m: &Module) {
        assert_eq!(m.words[0], 0x0723_0203, "the SPIR-V magic");
        let Ok(out) = std::process::Command::new("spirv-val")
            .arg("--version")
            .output()
        else {
            eprintln!("spirv-val not installed: SPIR-V not validated");
            return;
        };
        assert!(out.status.success());
        let dir = std::env::temp_dir().join(format!("kestrel-spirv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{:08x}.spv", m.words.len() as u32 ^ m.words[3]));
        let bytes: Vec<u8> = m.words.iter().flat_map(|w| w.to_le_bytes()).collect();
        std::fs::write(&path, bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .args(["--target-env", "vulkan1.1"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn opts(base: u32) -> Options {
        Options {
            vertex_inputs: [AttrType::Float; 32],
            z_minus_one_to_one: true,
            binding_base: base,
        }
    }

    #[test]
    fn a_vertex_shader_reads_attributes_and_constants_and_writes_its_outputs() {
        let cb = |index, offset, reg| Operand::Cbuf { index, offset, reg };
        let p = Program {
            header: header(Stage::Vertex),
            ops: vec![
                Op::LdAttr {
                    dst: 0,
                    count: 4,
                    addr: 0x80,
                },
                Op::LdAttr {
                    dst: 4,
                    count: 1,
                    addr: 0x2fc,
                },
                Op::Shl {
                    dst: 5,
                    a: Operand::Reg(4),
                    b: Operand::Imm(4),
                    wrap: false,
                },
                Op::Mov {
                    dst: 6,
                    src: cb(1, 0x30, 5),
                },
                Op::FMul {
                    dst: 7,
                    a: FSrc::plain(Operand::Reg(0)),
                    b: FSrc::plain(cb(2, 0x10, RZ)),
                    scale: 1,
                    sat: false,
                },
                Op::FFma {
                    dst: 0,
                    a: FSrc::plain(Operand::Reg(7)),
                    b: FSrc {
                        op: Operand::Imm(0x3f00_0000),
                        neg: true,
                        abs: false,
                    },
                    c: FSrc::plain(Operand::Reg(6)),
                    sat: true,
                },
                Op::I2F {
                    dst: 8,
                    src: Operand::Reg(4),
                    signed: true,
                    neg: false,
                    abs: true,
                },
                Op::F2I {
                    dst: 9,
                    src: FSrc::plain(Operand::Reg(8)),
                    signed: false,
                    bits: 16,
                    round: Round::Floor,
                },
                Op::StAttr {
                    src: 0,
                    count: 4,
                    addr: 0x70,
                },
                Op::StAttr {
                    src: 8,
                    count: 2,
                    addr: 0x84,
                },
                Op::Exit,
            ],
        };
        let m = emit(&p, &opts(0)).unwrap();
        assert_eq!(m.uniform_buffers, [(1, 1), (2, 2)]);
        assert_eq!(m.inputs, 1);
        validate(&m);
    }

    #[test]
    fn a_pixel_shader_interpolates_samples_and_writes_its_colour() {
        let p = Program {
            header: header(Stage::Fragment),
            ops: vec![
                Op::Ipa {
                    dst: 0,
                    addr: 0x7c,
                    mode: Interp::Linear,
                    mult: RZ,
                    sat: false,
                },
                Op::Mufu {
                    dst: 2,
                    func: Mufu::Rcp,
                    src: FSrc::plain(Operand::Reg(0)),
                    sat: false,
                },
                Op::Ipa {
                    dst: 1,
                    addr: 0x80,
                    mode: Interp::Perspective,
                    mult: 2,
                    sat: false,
                },
                Op::Ipa {
                    dst: 0,
                    addr: 0x84,
                    mode: Interp::Perspective,
                    mult: 2,
                    sat: false,
                },
                Op::Ipa {
                    dst: 4,
                    addr: 0x90,
                    mode: Interp::Flat,
                    mult: RZ,
                    sat: true,
                },
                Op::Tex2d {
                    dsts: [0, 2],
                    mask: 0xf,
                    x: 1,
                    y: 0,
                    handle: 0x1a4,
                },
                Op::F2F {
                    dst: 3,
                    src: FSrc {
                        op: Operand::Reg(4),
                        neg: false,
                        abs: true,
                    },
                    round: Some(Round::Trunc),
                    sat: false,
                },
                Op::Exit,
            ],
        };
        let m = emit(&p, &opts(64)).unwrap();
        assert_eq!(m.textures, [(0x1a4, 64 + 32)]);
        assert_eq!(
            m.interpolation,
            [(0, Interp::Perspective), (1, Interp::Flat)]
        );
        validate(&m);
        // A generic input interpolated two ways stops, named.
        let mut q = p.clone();
        q.ops.insert(
            0,
            Op::Ipa {
                dst: 5,
                addr: 0x80,
                mode: Interp::Flat,
                mult: RZ,
                sat: false,
            },
        );
        assert!(emit(&q, &opts(64))
            .unwrap_err()
            .contains("interpolated in two ways"));
    }
}
