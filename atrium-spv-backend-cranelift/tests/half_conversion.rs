//! Exhaustive check of the integer-IR f32<->f16 conversions
//! behind PackHalf2x16 / UnpackHalf2x16, against Rust's own
//! `f16` (IEEE round-to-nearest-even). Runs on whatever ISA
//! the host is, which is the point: these replace hardware
//! FCVT on targets without a bespoke port.
#![feature(f16)]

use atrium_spv_backend_cranelift::{f16_bits_to_f32, f32_to_f16_bits};
use cranelift_codegen::ir::{types, AbiParam, Function, InstBuilder, Signature, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::process::Command;

type Pack = unsafe extern "C" fn(f32) -> u32;
type Unpack = unsafe extern "C" fn(u32) -> f32;

fn build_library(dir: &std::path::Path) -> libloading::Library {
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    let mut isa = cranelift_native::builder().unwrap();
    if cfg!(target_arch = "x86_64") {
        for f in ["has_sse41", "has_fma", "has_avx", "has_avx2"] {
            isa.enable(f).unwrap();
        }
    }
    let isa = isa.finish(settings::Flags::new(flags)).unwrap();
    let cc = CallConv::triple_default(isa.triple());
    let mut m = ObjectModule::new(ObjectBuilder::new(
        isa, b"half".to_vec(), cranelift_module::default_libcall_names()).unwrap());
    let mut fctx = FunctionBuilderContext::new();

    for (name, pack) in [("pack", true), ("unpack", false)] {
        let mut sig = Signature::new(cc);
        let (pt, rt) = if pack { (types::F32, types::I32) } else { (types::I32, types::F32) };
        sig.params.push(AbiParam::new(pt));
        sig.returns.push(AbiParam::new(rt));
        let id = m.declare_function(name, Linkage::Export, &sig).unwrap();
        let mut func = Function::with_name_signature(UserFuncName::user(0, id.as_u32()), sig);
        {
            let mut b = FunctionBuilder::new(&mut func, &mut fctx);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let x = b.block_params(blk)[0];
            let r = if pack { f32_to_f16_bits(&mut b, x) } else { f16_bits_to_f32(&mut b, x) };
            b.ins().return_(&[r]);
            b.finalize();
        }
        let mut ctx = Context::for_function(func);
        m.define_function(id, &mut ctx).unwrap();
    }
    let obj = dir.join("half.o");
    std::fs::write(&obj, m.finish().emit().unwrap()).unwrap();
    let so = dir.join(if cfg!(target_os = "macos") { "half.dylib" } else { "half.so" });
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let st = Command::new("cc").arg(flag).arg("-o").arg(&so).arg(&obj).status().unwrap();
    assert!(st.success());
    unsafe { libloading::Library::new(&so) }.unwrap()
}

#[test]
fn half_conversions_match_ieee() {
    let dir = tempfile::TempDir::new().unwrap();
    let lib = build_library(dir.path());
    let pack: libloading::Symbol<Pack> = unsafe { lib.get(b"pack") }.unwrap();
    let unpack: libloading::Symbol<Unpack> = unsafe { lib.get(b"unpack") }.unwrap();

    // Every half -> f32 is exact.
    for h in 0..=0xffffu32 {
        let want = f16::from_bits(h as u16) as f32;
        let got = unsafe { unpack(h) };
        if want.is_nan() {
            assert!(got.is_nan(), "unpack({h:#06x}) = {got}, want NaN");
        } else {
            assert_eq!(got.to_bits(), want.to_bits(), "unpack({h:#06x})");
        }
    }

    // f32 -> half: every 2^7th f32 bit pattern (33M inputs:
    // all exponents, signs, and rounding positions), plus
    // the exact half/halfway boundaries.
    let check = |bits: u32| {
        let x = f32::from_bits(bits);
        let got = unsafe { pack(x) };
        assert_eq!(got >> 16, 0, "pack({x:e}) leaked high bits: {got:#x}");
        if x.is_nan() {
            assert_eq!(got & 0x7fff, 0x7e00, "pack(NaN {bits:#x}) = {got:#x}");
        } else {
            let want = (x as f16).to_bits() as u32;
            assert_eq!(got, want, "pack({x:e} = {bits:#010x})");
        }
    };
    for bits in (0..=u32::MAX).step_by(127) {
        check(bits);
    }
    for h in 0..=0xffffu32 {
        let x = f16::from_bits(h as u16) as f32;
        if x.is_finite() {
            let b = x.to_bits();
            // The value itself, and its f32 neighbours either side.
            for d in [0u32, 1, u32::MAX] { check(b.wrapping_add(d)); }
            // Halfway to the next half: 1 << 12 ulps of f32 for normals.
            for d in [0x1000u32, 0x0fff, 0x1001] { check(b.wrapping_add(d)); }
        }
    }
}
