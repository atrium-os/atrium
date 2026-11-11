//! The case corpus. Hand-assembled cases come first (M0); compiled cases
//! replace them as the compiler lands.

use crate::{Buffer, Case};
use atrium_rdna4_cmd::{emit_dispatch, Blob, Dispatch};
use atrium_rdna4_isa::encode;

/// GPU address of the code buffer (shader at 0, IB at [`IB_OFFSET`]).
pub const CODE_VA: u64 = 0x1_0000_0000;
/// Byte offset of the indirect buffer inside the code buffer.
pub const IB_OFFSET: u64 = 0x1000;
/// GPU address of the first data buffer.
pub const DATA_VA: u64 = 0x1_0010_0000;

/// Inline-constant source operand for a small non-negative integer (0..=64).
fn inline_int(v: u32) -> u64 {
    assert!(v <= 64);
    128 + v as u64
}

fn words_to_bytes(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|d| d.to_le_bytes()).collect()
}

/// `write_42`: one workgroup of 64 threads (two wave32 waves); thread `i`
/// stores 42 at `out[i]`. The output address arrives in user SGPRs s[0:1].
pub fn write_42() -> Case {
    let mut sh: Vec<u32> = Vec::new();
    // v1 = v0 << 2            (byte offset of this thread's element)
    sh.extend(encode("V_LSHLREV_B32", "ENC_VOP2", "default", &[("VDST", 1), ("SRC0", inline_int(2)), ("VSRC1", 0)]).unwrap());
    // v2 = 42
    sh.extend(encode("V_MOV_B32", "ENC_VOP1", "default", &[("VDST", 2), ("SRC0", inline_int(42))]).unwrap());
    // global_store_b32 v1, v2, s[0:1]   (address = s[0:1] + v1)
    sh.extend(encode("GLOBAL_STORE_B32", "ENC_VGLOBAL", "default", &[("SADDR", 0), ("VADDR", 1), ("VSRC", 2)]).unwrap());
    sh.extend(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap());

    let mut ib = Blob::compute();
    ib.acquire_all();
    emit_dispatch(&mut ib, &Dispatch {
        shader_va: CODE_VA,
        vgprs: 3,
        user_sgprs: vec![DATA_VA as u32, (DATA_VA >> 32) as u32],
        threads: [64, 1, 1],
        groups: [1, 1, 1],
        wave32: true,
        lds_bytes: 0,
        scratch: None,
    });
    ib.pad(8);

    let mut code = words_to_bytes(&sh);
    code.resize(IB_OFFSET as usize, 0);
    code.extend(ib.bytes());
    let ib_dw = ib.dw.len() as u64;

    Case {
        name: "write_42".into(),
        ip: "compute",
        buffers: vec![
            Buffer { name: "code".into(), va: CODE_VA, size: 0x2000, init: code, expect: None, unordered: None },
            Buffer { name: "out".into(), va: DATA_VA, size: 64 * 4, init: vec![], expect: Some(words_to_bytes(&[42; 64])), unordered: None },
        ],
        ib: ("code".into(), IB_OFFSET, ib_dw),
    }
}

/// The `code` buffer for a compiled shader: the shader at 0, then the IB at
/// [`IB_OFFSET`] or, for a shader longer than that, at the next page after
/// it (so a large shader is never cut off and run into the IB).
pub(crate) fn code_buffer(shader: &[u32], ib: &Blob) -> (Buffer, u64, u64) {
    let mut code = words_to_bytes(shader);
    let ib_off = (IB_OFFSET as usize).max(code.len().next_multiple_of(0x1000));
    assert!(((ib_off + 4 * ib.dw.len()) as u64) < DATA_VA - CODE_VA, "shader larger than the code region");
    code.resize(ib_off, 0);
    code.extend(ib.bytes());
    let size = code.len().next_multiple_of(0x1000).max(0x2000) as u64;
    (Buffer { name: "code".into(), va: CODE_VA, size, init: code, expect: None, unordered: None }, ib_off as u64, ib.dw.len() as u64)
}

// ------------------------------------------------------------ spilling ---
//
// Every compiled case can be rebuilt with a low VGPR cap, which forces the
// compiler to spill to scratch; the expected outputs stay the same.

thread_local! {
    static CAP: std::cell::Cell<u32> = const { std::cell::Cell::new(256) };
    static SCAP: std::cell::Cell<u32> = const { std::cell::Cell::new(106) };
}

/// SGPR cap of the spill variants.
pub const SPILL_SCAP: u32 = 24;

/// Run `f` with the SGPR compile cap at `cap`.
pub fn with_scap<T>(cap: u32, f: impl FnOnce() -> T) -> T {
    let old = SCAP.with(|c| c.replace(cap));
    let r = f();
    SCAP.with(|c| c.set(old));
    r
}

/// The current SGPR compile cap.
pub(crate) fn scap() -> u32 {
    SCAP.with(|c| c.get())
}

/// VGPR cap of the spill variants.
pub const SPILL_CAP: u32 = 16;

/// Run `f` with the compile cap at `cap` VGPRs.
pub fn with_cap<T>(cap: u32, f: impl FnOnce() -> T) -> T {
    let old = CAP.with(|c| c.replace(cap));
    let r = f();
    CAP.with(|c| c.set(old));
    r
}

/// The current compile cap.
pub(crate) fn cap() -> u32 {
    CAP.with(|c| c.get())
}

/// Compile under the current cap.
pub(crate) fn compile(g: &atrium_gir::Function) -> Result<atrium_rdna4_compiler::Binary, atrium_rdna4_compiler::Error> {
    atrium_rdna4_compiler::compile_with(g, &atrium_rdna4_compiler::Options { max_vgprs: cap(), max_sgprs: scap() })
}

/// GPU address of the scratch ring (above every data slot).
pub const SCRATCH_VA: u64 = 0x1_1000_0000;
/// Wave slots in the scratch ring; the SPI holds back waves beyond them.
pub const SCRATCH_WAVES: u32 = 64;

/// The scratch ring for `dwords` per lane, if any: the dispatch field and
/// its buffer. The buffer is 4x the ring in case WAVES counts per shader
/// engine (not yet pinned on the R9700).
pub(crate) fn scratch_ring(dwords: u32) -> (Option<atrium_rdna4_cmd::Scratch>, Option<Buffer>) {
    if dwords == 0 {
        return (None, None);
    }
    let s = atrium_rdna4_cmd::Scratch { va: SCRATCH_VA, wave_bytes: atrium_rdna4_cmd::Scratch::wave_bytes(dwords, 32), waves: SCRATCH_WAVES };
    let b = Buffer { name: "scratch".into(), va: SCRATCH_VA, size: (4 * s.bytes()).next_multiple_of(0x1000), init: vec![], expect: None, unordered: None };
    (Some(s), Some(b))
}

/// GPU address of data buffer `i` (1 MiB apart).
pub(crate) fn data_va(i: usize) -> u64 {
    DATA_VA + (i as u64) * 0x10_0000
}

pub(crate) struct Rng(pub(crate) u64);
impl Rng {
    pub(crate) fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    /// An exact float in [1, 251), never zero.
    pub(crate) fn f(&mut self) -> f32 {
        1.0 + (self.next() % 2000) as f32 / 8.0
    }
}

/// A desktop compute shader compiled by our backend: SPIR-V → GIR →
/// optimise → RDNA4. Expected buffers come from the GIR interpreter run on
/// the same inputs. `None` when the shader's SPIR-V is not built
/// (`bundles/*/build.sh`).
pub fn desktop(name: &str, spv: &str, nodes: u32, record: u32) -> Option<Case> {
    use atrium_gir::interp;
    let path = format!("{}/../bundles/{spv}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(path).ok()?;
    let m = atrium_spv_frontend::translate_with_options(&bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).expect("frontend");
    let mut g = atrium_gir::build::build(&m, 0).expect("gir");
    atrium_gir::passes::optimize(&mut g);
    atrium_gir::verify::verify(&g).expect("verify");
    let bin = compile(&g).expect("rdna4 compile");
    let (ring, ring_buf) = scratch_ring(bin.scratch_dwords);

    // Inputs: scene = node count + 64 bytes per node of floats; glyph runs
    // get small glyph counts and offsets into a glyph buffer.
    let mut r = Rng(0x5eed ^ nodes as u64);
    let fl = |n: usize, r: &mut Rng| -> Vec<u8> { (0..n).flat_map(|_| r.f().to_le_bytes()).collect() };
    let mut scene = fl(4 + 16 * nodes as usize, &mut r);
    scene[0..4].copy_from_slice(&nodes.to_le_bytes());
    let mut bufs: Vec<Vec<u8>> = vec![];
    let glyph = spv.contains("glyph");
    if glyph {
        let mut total = 0u32;
        for i in 0..nodes as usize {
            let count = r.next() % 4;
            let meta = 16 + 64 * i + 48;
            scene[meta..meta + 4].copy_from_slice(&count.to_le_bytes());
            scene[meta + 4..meta + 8].copy_from_slice(&total.to_le_bytes());
            total += count;
        }
        let glyphs = fl(12 * total as usize, &mut r);
        bufs.extend([scene, glyphs, vec![0; (record * total) as usize], vec![0; 4]]);
    } else {
        bufs.extend([scene, vec![0; (record * nodes) as usize], vec![0; 4]]);
    }
    let groups = (nodes + 63) / 64 + 1;
    let mut ib_bufs: interp::Buffers = bufs.iter().enumerate().map(|(i, b)| ((0, i as u32), b.clone())).collect();
    interp::run(&g, [groups, 1, 1], &mut ib_bufs).expect("interpreter");

    let mut user = vec![];
    for (set, binding) in &bin.abi.buffers {
        assert_eq!(*set, 0);
        let va = data_va(*binding as usize);
        user.extend([va as u32, (va >> 32) as u32]);
    }
    let mut ib = Blob::compute();
    ib.acquire_all();
    emit_dispatch(&mut ib, &Dispatch { shader_va: CODE_VA, vgprs: bin.vgprs, user_sgprs: user, threads: bin.local_size, groups: [groups, 1, 1], wave32: true, lds_bytes: bin.lds_bytes, scratch: ring });
    ib.pad(8);
    let (code, ib_off, ib_dw) = code_buffer(&bin.code, &ib);

    let names = if glyph { vec!["scene", "glyphs", "instances", "counter"] } else { vec!["scene", "instances", "counter"] };
    let mut buffers = vec![code];
    for (i, b) in bufs.iter().enumerate() {
        let out = ib_bufs.remove(&(0, i as u32)).unwrap();
        let size = ((b.len().max(4) as u64) + 4095) & !4095;
        buffers.push(Buffer {
            name: names[i].into(),
            va: data_va(i),
            size,
            init: b.clone(),
            unordered: (names[i] == "instances").then_some(record),
            expect: Some(out),
        });
    }
    buffers.extend(ring_buf);
    Some(Case { name: name.into(), buffers, ib: ("code".into(), ib_off, ib_dw), ip: "compute" })
}

/// A Tier-B example (atrium-vk-icd/examples/shaders) compiled by our
/// backend: every binding gets 1 KiB of small integers, one workgroup.
/// `None` when the SPIR-V is missing or the backend does not support the
/// shader yet.
pub fn tierb(name: &str) -> Option<Case> {
    use atrium_gir::interp;
    // The oracle's own shaders first, then the Vulkan ICD's examples.
    let own = format!("{}/shaders/{name}.comp.spv", env!("CARGO_MANIFEST_DIR"));
    let icd = format!("{}/../atrium-vk-icd/examples/shaders/{name}.comp.spv", env!("CARGO_MANIFEST_DIR"));
    let (bytes, words) = match std::fs::read(own) {
        // The oracle's shaders write up to 16 words per lane.
        Ok(b) => (b, 1024u32),
        Err(_) => (std::fs::read(icd).ok()?, 256),
    };
    let m = atrium_spv_frontend::translate_with_options(&bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).ok()?;
    let mut g = atrium_gir::build::build(&m, 0).ok()?;
    atrium_gir::passes::optimize(&mut g);
    // A GIR type error is a compiler bug, not an unsupported example.
    atrium_gir::verify::verify(&g).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let bin = compile(&g).ok()?;
    let (ring, ring_buf) = scratch_ring(bin.scratch_dwords);
    let fill = |k: u32| -> Vec<u8> { (0..words).flat_map(|i| (i % 7 + k).to_le_bytes()).collect() };
    let mut ib_bufs: interp::Buffers = g.buffers.iter().map(|&(s, b)| ((s, b), fill(b + 1))).collect();
    // Push constants: small distinct integers (3, 4, 5, ...).
    let push: Vec<u8> = (0..g.push_bytes / 4).flat_map(|i| (i + 3).to_le_bytes()).collect();
    if g.push_bytes > 0 {
        ib_bufs.insert(interp::PUSH, push.clone());
    }
    let inputs = ib_bufs.clone();
    interp::run(&g, [1, 1, 1], &mut ib_bufs).ok()?;
    let mut user = vec![];
    let mut buffers = vec![];
    for (i, (set, binding)) in bin.abi.buffers.iter().enumerate() {
        let va = data_va(i);
        user.extend([va as u32, (va >> 32) as u32]);
        buffers.push(Buffer {
            name: format!("b{set}_{binding}"),
            va,
            size: 4096.max(4 * words as u64),
            init: inputs[&(*set, *binding)].clone(),
            expect: Some(ib_bufs.remove(&(*set, *binding)).unwrap()),
            unordered: None,
        });
    }
    if bin.abi.push {
        let va = data_va(buffers.len());
        user.extend([va as u32, (va >> 32) as u32]);
        buffers.push(Buffer { name: "push".into(), va, size: 4096, init: push, expect: None, unordered: None });
    }
    let mut ib = Blob::compute();
    ib.acquire_all();
    emit_dispatch(&mut ib, &Dispatch { shader_va: CODE_VA, vgprs: bin.vgprs, user_sgprs: user, threads: bin.local_size, groups: [1, 1, 1], wave32: true, lds_bytes: bin.lds_bytes, scratch: ring });
    ib.pad(8);
    let (code, ib_off, ib_dw) = code_buffer(&bin.code, &ib);
    buffers.insert(0, code);
    buffers.extend(ring_buf);
    Some(Case { name: format!("tierb_{name}"), buffers, ib: ("code".into(), ib_off, ib_dw), ip: "compute" })
}

/// Tier-B examples tried as compiled cases, simplest first.
pub const TIERB: [&str; 25] = [
    "write_42", "per_thread", "multi_binding", "max_const", "ternary", "branch", "rmw", "rotate",
    "bit_chain", "bit_count", "loop_mul", "sum_loop", "atomic_add", "per_thread_triangle", "spec_scale", "float_sqrt",
    "push_scale", "groupshared_xor", "subgroup_sum", "subgroup_ops", "subgroup_partial", "math_exact", "raster_math",
    "control_flow", "scalar_loads",
];

/// The desktop compute shaders: name, SPIR-V path under `bundles/`, bytes
/// per instance record.
pub const DESKTOP: [(&str, &str, u32); 4] = [
    ("op_texture", "atrium-core/compute/op_texture.comp.spv", 16),
    ("op_rectangle", "atrium-core/compute/op_rectangle.comp.spv", 48),
    ("op_path", "atrium-core/compute/op_path.comp.spv", 48),
    ("op_glyph_run", "atrium-text/compute/op_glyph_run.comp.spv", 48),
];

/// Every case, by name.
pub fn all() -> Vec<Case> {
    let mut v = vec![write_42()];
    for (n, p, rec) in DESKTOP {
        v.extend(desktop(n, p, 100, rec));
    }
    for t in TIERB {
        v.extend(tierb(t));
    }
    v.extend(crate::fresco::bin());
    v.extend(crate::fresco::raster());
    v.extend(crate::fresco::frame());
    v.extend(crate::fresco::damage());
    v.extend(crate::fresco::shift());
    v.extend(crate::fresco::compose());
    v.extend(spilled());
    v
}

/// Every compiled case again at [`SPILL_CAP`] VGPRs, where that makes the
/// compiler spill: named `spill_<case>`, same expected outputs.
pub fn spilled() -> Vec<Case> {
    with_cap(SPILL_CAP, || {
        let mut v = vec![];
        for (n, p, rec) in DESKTOP {
            v.extend(desktop(n, p, 100, rec));
        }
        for t in TIERB {
            v.extend(tierb(t));
        }
        v.extend(crate::fresco::bin());
        v.extend(crate::fresco::raster());
        v.into_iter()
            .filter(|c| c.buffers.iter().any(|b| b.name == "scratch"))
            .map(|mut c| {
                c.name = format!("spill_{}", c.name);
                c
            })
            .chain(sgpr_spilled())
            .collect()
    })
}

/// SGPR spilling (into VGPR lanes): glyph_run and Fresco bin/raster with
/// a few SGPRs below their need, and raster with both caps at once (this
/// runs inside [`spilled`], under the VGPR cap, for that last one).
fn sgpr_spilled() -> Vec<Case> {
    let rename = |c: Option<Case>, p: &str| c.map(|mut c| { c.name = format!("{p}_{}", c.name); c });
    let mut v = vec![];
    with_cap(256, || {
        let (n, p, rec) = DESKTOP[3];
        v.extend(rename(with_scap(10, || desktop(n, p, 100, rec)), "sspill"));
        v.extend(rename(with_scap(15, crate::fresco::bin), "sspill"));
        v.extend(rename(with_scap(SPILL_SCAP, crate::fresco::raster), "sspill"));
    });
    v.extend(rename(with_scap(SPILL_SCAP, crate::fresco::raster), "vsspill"));
    v
}

// ------------------------------------------------------------ calibration ---
//
// Cases that measure an allocation unit the RDNA4 ISA guide does not pin down
// for COMPUTE_PGM_RSRC1/2. They run after the corpus (one may trap on the
// card if an assumption is wrong) and their outputs are read, not just
// matched. Expectations are what the R9700 measured (report v3, 2026-11-04).

fn lit(mut w: Vec<u32>, v: u32) -> Vec<u32> {
    w.push(v);
    w
}

/// `v[dst] = value` (32-bit literal).
fn mov_lit(dst: u64, value: u32) -> Vec<u32> {
    lit(encode("V_MOV_B32", "ENC_VOP1", "default", &[("VDST", dst), ("SRC0", 255)]).unwrap(), value)
}

/// `global_store_b32 v[vaddr], v[data], s[0:1] offset:ioffset`.
fn store(vaddr: u64, data: u64, ioffset: u64) -> Vec<u32> {
    encode("GLOBAL_STORE_B32", "ENC_VGLOBAL", "default", &[("SADDR", 0), ("VADDR", vaddr), ("VSRC", data), ("IOFFSET", ioffset)]).unwrap()
}

/// One wave of one work-item, output buffer at s[0:1]. `rsrc2_extra` is ORed
/// into COMPUTE_PGM_RSRC2.
fn one_lane_case(name: String, sh: Vec<u32>, vgprs: u32, wave32: bool, rsrc2_extra: u32, out_dw: usize, expect: Vec<u32>) -> Case {
    use atrium_rdna4_cmd::regs::{self, compute_pgm_rsrc2 as r2};
    let user = vec![DATA_VA as u32, (DATA_VA >> 32) as u32];
    let mut ib = Blob::compute();
    ib.acquire_all();
    emit_dispatch(&mut ib, &Dispatch { shader_va: CODE_VA, vgprs, user_sgprs: user.clone(), threads: [1, 1, 1], groups: [1, 1, 1], wave32, lds_bytes: 0, scratch: None });
    if rsrc2_extra != 0 {
        // Re-issue RSRC2 just before the DISPATCH_DIRECT (header + 4 dwords).
        let n = ib.dw.len();
        let dispatch: Vec<u32> = ib.dw.drain(n - 5..).collect();
        assert_eq!(dispatch[0], ib.header(atrium_rdna4_cmd::op::DISPATCH_DIRECT, 4));
        let rsrc2 = ((user.len() as u32) << r2::USER_SGPR_SHIFT) | r2::TGID_X_EN_MASK | r2::TGID_Y_EN_MASK | r2::TGID_Z_EN_MASK | rsrc2_extra;
        ib.set_sh(regs::COMPUTE_PGM_RSRC2, &[rsrc2]);
        ib.dw.extend(dispatch);
    }
    ib.pad(8);
    let mut code = words_to_bytes(&sh);
    assert!(code.len() <= IB_OFFSET as usize, "shader larger than the code slot");
    code.resize(IB_OFFSET as usize, 0);
    code.extend(ib.bytes());
    let ib_dw = ib.dw.len() as u64;
    Case {
        name,
        ip: "compute",
        buffers: vec![
            Buffer { name: "code".into(), va: CODE_VA, size: 0x2000, init: code, expect: None, unordered: None },
            Buffer { name: "out".into(), va: DATA_VA, size: (out_dw * 4) as u64, init: vec![], expect: Some(words_to_bytes(&expect)), unordered: None },
        ],
        ib: ("code".into(), IB_OFFSET, ib_dw),
    }
}

/// Number of VGPRs probed by [`vgpr_alloc`].
pub const VGPR_PROBES: u32 = 64;

/// `vgpr_alloc_w{32,64}_f{field}`: how many VGPRs COMPUTE_PGM_RSRC1.VGPRS =
/// `field` allocates. The single lane writes `0xa000_0000 | k` to every
/// v2..v63, then stores each to `out[k]`. A write to a VGPR past the
/// allocation is a NOP and a store from one reads v0 (= 0 for work-item 0),
/// ISA §3.3.2.2, so `out` holds markers up to the allocation and zeros after:
/// no fault either way. Expectation, measured on the R9700 (2026-11-04
/// report v3): each field step is 8 VGPRs for wave32 and 4 for wave64, the
/// encoding `emit_dispatch` uses. (The §3.3.2.1 blocks of 16/8 do not
/// describe this field's unit.)
pub fn vgpr_alloc(wave32: bool, field: u32) -> Case {
    let mut sh = mov_lit(1, 0);
    for k in 2..VGPR_PROBES {
        sh.extend(mov_lit(k as u64, 0xa000_0000 | k));
    }
    for k in 2..VGPR_PROBES {
        sh.extend(store(1, k as u64, 4 * k as u64));
    }
    sh.extend(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap());
    // emit_dispatch encodes VGPRS = ceil(vgprs / g) - 1 with g = 8 (wave32)
    // or 4 (wave64); pick vgprs so the field is exactly `field`.
    let g = if wave32 { 8 } else { 4 };
    let alloc = g * (field + 1);
    let expect = (0..VGPR_PROBES).map(|k| if (2..alloc).contains(&k) { 0xa000_0000 | k } else { 0 }).collect();
    let w = if wave32 { 32 } else { 64 };
    one_lane_case(format!("vgpr_alloc_w{w}_f{field}"), sh, g * (field + 1), wave32, 0, VGPR_PROBES as usize, expect)
}

/// Bytes per COMPUTE_PGM_RSRC2.LDS_SIZE step (also atrium_rdna4_cmd::LDS_SIZE_UNIT). Not the 1024-byte LDS block
/// of ISA §3.3.5: on the R9700 (2026-11-04 report v3) LDS_SIZE=1 kept byte
/// 508 but not 1020, and LDS_SIZE=64 did not reach 65532. 512 fits both; the
/// f2/f128 cases below pin it.
pub const LDS_SIZE_UNIT: u32 = 512;

/// `lds_alloc_f{field}_at_{addr}`: with COMPUTE_PGM_RSRC2.LDS_SIZE = `field`,
/// store a marker at LDS byte `addr`, load it back and write it to `out[0]`.
/// Past the allocation a load returns 0 (§3.3.5.1); the guide also says
/// MEMVIOL traps, but on the R9700 these jobs finished with status=ok.
/// Expectation: LDS_SIZE counts [`LDS_SIZE_UNIT`] bytes, so the marker
/// survives when `addr + 4 <= field * LDS_SIZE_UNIT`, else `out[0]` is 0.
/// These cases still run last.
pub fn lds_alloc(field: u32, addr: u32) -> Case {
    use atrium_rdna4_cmd::regs::compute_pgm_rsrc2 as r2;
    let marker = 0x1d50_0000 | addr;
    let mut sh = mov_lit(1, addr);
    sh.extend(mov_lit(2, marker));
    sh.extend(encode("DS_STORE_B32", "ENC_VDS", "default", &[("ADDR", 1), ("DATA0", 2)]).unwrap());
    sh.extend(encode("S_WAIT_DSCNT", "ENC_SOPP", "default", &[("SIMM16", 0)]).unwrap());
    sh.extend(encode("DS_LOAD_B32", "ENC_VDS", "default", &[("ADDR", 1), ("VDST", 3)]).unwrap());
    sh.extend(encode("S_WAIT_DSCNT", "ENC_SOPP", "default", &[("SIMM16", 0)]).unwrap());
    sh.extend(mov_lit(4, 0));
    sh.extend(store(4, 3, 0));
    sh.extend(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap());
    assert!(field << r2::LDS_SIZE_SHIFT & !r2::LDS_SIZE_MASK == 0);
    let expect = if addr + 4 <= field * LDS_SIZE_UNIT { marker } else { 0 };
    one_lane_case(format!("lds_alloc_f{field}_at_{addr}"), sh, 8, true, field << r2::LDS_SIZE_SHIFT, 1, vec![expect])
}

/// The calibration cases, in run order (the trap-capable LDS ones last).
pub fn calibration() -> Vec<Case> {
    vec![
        vgpr_alloc(true, 0),
        vgpr_alloc(true, 1),
        vgpr_alloc(false, 0),
        vgpr_alloc(false, 1),
        lds_alloc(1, 508),
        lds_alloc(1, 1020),
        lds_alloc(64, 65532),
        // Pin the 512-byte unit: inside and just past two units, and the
        // full 64 KiB at LDS_SIZE = 128.
        lds_alloc(2, 1020),
        lds_alloc(2, 1024),
        lds_alloc(128, 65532),
    ]
}
