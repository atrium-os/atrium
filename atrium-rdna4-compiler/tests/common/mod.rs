//! Compute kernels built in code with rspirv (no shader compiler needed).
#![allow(dead_code)]
use rspirv::binary::Assemble;
use rspirv::dr::{Builder, Operand};
use rspirv::spirv::{self, AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode, ExecutionModel,
    FunctionControl, MemoryModel, SelectionControl, StorageClass};

pub struct K {
    pub b: Builder,
    pub u32_t: u32,
    pub bool_t: u32,
    pub ptr_sb_u32: u32,
    pub gid_x: u32,
    pub bufs: Vec<u32>,
}

/// Start a compute kernel with `nbufs` runtime-array<uint> SSBOs at set 0,
/// bindings 0.., local size (64,1,1). Returns the builder positioned inside main
/// with `gid_x` = gl_GlobalInvocationID.x loaded.
pub fn begin(nbufs: u32) -> K {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let u32_t = b.type_int(32, 0);
    let bool_t = b.type_bool();
    let v3 = b.type_vector(u32_t, 3);
    let rta = b.type_runtime_array(u32_t);
    b.decorate(rta, Decoration::ArrayStride, [Operand::LiteralBit32(4)]);
    let st = b.type_struct([rta]);
    b.decorate(st, Decoration::Block, []);
    b.member_decorate(st, 0, Decoration::Offset, [Operand::LiteralBit32(0)]);
    let ptr_st = b.type_pointer(None, StorageClass::StorageBuffer, st);
    let ptr_sb_u32 = b.type_pointer(None, StorageClass::StorageBuffer, u32_t);
    let ptr_in_v3 = b.type_pointer(None, StorageClass::Input, v3);
    let gid = b.variable(ptr_in_v3, None, StorageClass::Input, None);
    b.decorate(gid, Decoration::BuiltIn, [Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let mut bufs = vec![];
    for i in 0..nbufs {
        let v = b.variable(ptr_st, None, StorageClass::StorageBuffer, None);
        b.decorate(v, Decoration::DescriptorSet, [Operand::LiteralBit32(0)]);
        b.decorate(v, Decoration::Binding, [Operand::LiteralBit32(i)]);
        bufs.push(v);
    }
    let fnty = b.type_function(void, []);
    let f = b.begin_function(void, None, FunctionControl::NONE, fnty).unwrap();
    let mut iface = vec![gid];
    iface.extend(&bufs);
    b.entry_point(ExecutionModel::GLCompute, f, "main", iface);
    b.execution_mode(f, ExecutionMode::LocalSize, [64, 1, 1]);
    b.begin_block(None).unwrap();
    let g = b.load(v3, None, gid, None, []).unwrap();
    let gid_x = b.composite_extract(u32_t, None, g, [0]).unwrap();
    K { b, u32_t, bool_t, ptr_sb_u32, gid_x, bufs }
}

impl K {
    pub fn c(&mut self, v: u32) -> u32 { self.b.constant_bit32(self.u32_t, v) }
    pub fn elem(&mut self, buf: usize, idx: u32) -> u32 {
        let zero = self.c(0);
        self.b.access_chain(self.ptr_sb_u32, None, self.bufs[buf], [zero, idx]).unwrap()
    }
    pub fn load(&mut self, buf: usize, idx: u32) -> u32 {
        let p = self.elem(buf, idx);
        self.b.load(self.u32_t, None, p, None, []).unwrap()
    }
    pub fn store(&mut self, buf: usize, idx: u32, v: u32) {
        let p = self.elem(buf, idx);
        self.b.store(p, v, None, []).unwrap();
    }
    pub fn atomic_add(&mut self, buf: usize, idx: u32, v: u32) -> u32 {
        let p = self.elem(buf, idx);
        let scope = self.c(1); // Device
        let sem = self.c(0);
        self.b.atomic_i_add(self.u32_t, None, p, scope, sem, v).unwrap()
    }
    /// if (cond) { body } — structured, with a merge block.
    pub fn if_then(&mut self, cond: u32, body: impl FnOnce(&mut K)) {
        let then_l = self.b.id();
        let merge_l = self.b.id();
        self.b.selection_merge(merge_l, SelectionControl::NONE).unwrap();
        self.b.branch_conditional(cond, then_l, merge_l, []).unwrap();
        self.b.begin_block(Some(then_l)).unwrap();
        body(self);
        self.b.branch(merge_l).unwrap();
        self.b.begin_block(Some(merge_l)).unwrap();
    }
    pub fn finish(mut self) -> Vec<u8> {
        self.b.ret().unwrap();
        self.b.end_function().unwrap();
        self.b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

/// out[gid] = in[gid] * 3 + 7 for gid < in_count; counter += 1 per active thread.
/// Buffers: 0 = params {count}, 1 = in, 2 = out, 3 = counter.
pub fn saxpy_like() -> Vec<u8> {
    let mut k = begin(4);
    let zero = k.c(0);
    let n = k.load(0, zero);
    let gid = k.gid_x;
    let lt = k.b.u_less_than(k.bool_t, None, gid, n).unwrap();
    k.if_then(lt, |k| {
        let x = k.load(1, k.gid_x);
        let three = k.c(3);
        let seven = k.c(7);
        let m = k.b.i_mul(k.u32_t, None, x, three).unwrap();
        let y = k.b.i_add(k.u32_t, None, m, seven).unwrap();
        let g = k.gid_x;
        k.store(2, g, y);
        let one = k.c(1);
        let z = k.c(0);
        k.atomic_add(3, z, one);
    });
    let _ = spirv::Op::Nop;
    k.finish()
}
