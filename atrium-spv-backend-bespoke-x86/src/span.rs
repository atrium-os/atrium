//! The batched fragment entry `atrium_fs_main_span`
//! (`atrium_spv_loader::FsSpanMain`), as a thunk placed right after the
//! scalar `atrium_fs_main` that calls it once per covered lane. The same
//! design as the arm64 backend's thunk, with the SysV argument layout.
//!
//! FsSpanMain: rdi varyings_soa, esi varying_stride, rdx uniforms,
//! rcx push_constants, r8 frag_x, r9 frag_y, then on the stack (8-byte
//! slots from the entry rsp): [8] frag_z, [16] frag_w, [24] coverage_mask
//! (u64), [32] samples_mask, [40] out_color_soa, [48] out_depth,
//! [56] front_facing, [64] primitive_id, [72] lane_count.

use atrium_spv_ir::{Function, Op, ShaderStage};
use iced_x86::code_asm::*;

use crate::emit::enc;
use crate::BackendError;

/// Six pushes and this frame put rsp at 0 mod 16 for the calls.
const FRAME: i32 = 56;
/// Entry rsp, measured from the post-prologue rsp.
const E: i32 = FRAME + 48;
// Frame: [0] front_facing and [8] primitive_id, fs_main's own stack
// arguments; [16] uniforms; [24] push constants; [32] varying stride;
// [40] samples mask.

/// The thunk, to be placed at byte `at` from the start of `fs_main`
/// (which it calls with a rel32). `None` for shaders outside the span
/// subset, which keep the per-pixel entry only. The subset matches the
/// Cranelift and arm64 spans: no MRT, no images, no derivatives.
pub(crate) fn thunk(func: &Function, at: u64) -> Result<Option<Vec<u8>>, BackendError> {
    if func.stage != ShaderStage::Fragment || func.output_varying_byte_offset.values().any(|&o| o != 0) {
        return Ok(None);
    }
    let images = func
        .blocks
        .values()
        .any(|b| b.insts.iter().any(|i| matches!(i.op, Op::ImageHandle { .. } | Op::Derivative { .. })));
    if images {
        return Ok(None);
    }
    let mut a = CodeAssembler::new(64).map_err(enc)?;
    let arg = |off: i32| rsp + (E + off);
    for r in [rbx, rbp, r12, r13, r14, r15] {
        a.push(r).map_err(enc)?;
    }
    a.sub(rsp, FRAME).map_err(enc)?;
    a.mov(qword_ptr(rsp + 16), rdx).map_err(enc)?;
    a.mov(qword_ptr(rsp + 24), rcx).map_err(enc)?;
    a.mov(esi, esi).map_err(enc)?;
    a.mov(qword_ptr(rsp + 32), rsi).map_err(enc)?;
    a.mov(eax, dword_ptr(arg(32))).map_err(enc)?;
    a.mov(dword_ptr(rsp + 40), eax).map_err(enc)?;
    // rbx lane, ebp lane_count, r12 remaining mask, r13 this lane's
    // varyings, r14/r15 frag_x/frag_y.
    a.xor(ebx, ebx).map_err(enc)?;
    a.mov(ebp, dword_ptr(arg(72))).map_err(enc)?;
    a.mov(r12, qword_ptr(arg(24))).map_err(enc)?;
    a.mov(r13, rdi).map_err(enc)?;
    a.mov(r14, r8).map_err(enc)?;
    a.mov(r15, r9).map_err(enc)?;

    let mut top = a.create_label();
    let mut advance = a.create_label();
    let mut end = a.create_label();
    a.set_label(&mut top).map_err(enc)?;
    a.cmp(ebx, ebp).map_err(enc)?;
    a.jae(end).map_err(enc)?;
    a.test(r12b, 1).map_err(enc)?;
    a.jz(advance).map_err(enc)?;
    // A covered lane: fs_main(varyings, uniforms, push, x, y, z, w,
    // samples, out_color + 16 lane, out_depth + 4 lane, ff, pid).
    a.mov(rdi, r13).map_err(enc)?;
    a.mov(rsi, qword_ptr(rsp + 16)).map_err(enc)?;
    a.mov(rdx, qword_ptr(rsp + 24)).map_err(enc)?;
    a.vmovss(xmm0, dword_ptr(r14 + rbx * 4)).map_err(enc)?;
    a.vmovss(xmm1, dword_ptr(r15 + rbx * 4)).map_err(enc)?;
    a.mov(rax, qword_ptr(arg(8))).map_err(enc)?;
    a.vmovss(xmm2, dword_ptr(rax + rbx * 4)).map_err(enc)?;
    a.mov(rax, qword_ptr(arg(16))).map_err(enc)?;
    a.vmovss(xmm3, dword_ptr(rax + rbx * 4)).map_err(enc)?;
    a.mov(ecx, dword_ptr(rsp + 40)).map_err(enc)?;
    a.mov(rax, rbx).map_err(enc)?;
    a.shl(rax, 4).map_err(enc)?;
    a.mov(r8, qword_ptr(arg(40))).map_err(enc)?;
    a.add(r8, rax).map_err(enc)?;
    a.mov(r9, qword_ptr(arg(48))).map_err(enc)?;
    a.lea(r9, qword_ptr(r9 + rbx * 4)).map_err(enc)?;
    // The callee owns its argument area, so store them for each call.
    a.mov(eax, dword_ptr(arg(56))).map_err(enc)?;
    a.mov(qword_ptr(rsp), rax).map_err(enc)?;
    a.mov(eax, dword_ptr(arg(64))).map_err(enc)?;
    a.mov(qword_ptr(rsp + 8), rax).map_err(enc)?;
    a.call(0u64).map_err(enc)?;
    a.set_label(&mut advance).map_err(enc)?;
    a.add(r13, qword_ptr(rsp + 32)).map_err(enc)?;
    a.shr(r12, 1).map_err(enc)?;
    a.inc(ebx).map_err(enc)?;
    a.jmp(top).map_err(enc)?;
    a.set_label(&mut end).map_err(enc)?;
    a.add(rsp, FRAME).map_err(enc)?;
    for r in [r15, r14, r13, r12, rbp, rbx] {
        a.pop(r).map_err(enc)?;
    }
    a.ret().map_err(enc)?;
    Ok(Some(a.assemble(at).map_err(enc)?))
}
