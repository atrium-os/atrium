//! Timing cases: the desktop shaders at large sizes, run both by our backend
//! (through `amdgpu_runner` with GPU timestamps) and by the system Vulkan
//! driver (`runner/vk_bench.c`, RADV on the R9700) on the same SPIR-V,
//! inputs and buffer placement (CPU-visible VRAM). The inputs are generated
//! on the GPU box by `runner/bench_tool.c`, which mirrors
//! [`crate::cases::desktop`]'s generator, so the script stays small.
//! Correctness at these sizes is checked by comparing the two drivers'
//! outputs; the small sizes are checked against the GIR interpreter by the
//! ordinary cases.

use crate::cases::{CODE_VA, IB_OFFSET};
use atrium_rdna4_cmd::{emit_dispatch, Blob, Dispatch};

/// GPU address of the timestamp buffer (two u64: start, end).
pub const TS_VA: u64 = 0x1_0001_0000;
/// GPU address of the first benchmark data buffer; buffers are 256 MiB apart.
pub const BENCH_VA: u64 = 0x2_0000_0000;

/// One benchmark: everything the script needs for both drivers.
#[derive(Debug, Clone)]
pub struct Bench {
    /// Name (`<shader>_<nodes>`).
    pub name: String,
    /// `plain` or `glyph` (input layout for `bench_tool gen`).
    pub kind: &'static str,
    /// Scene nodes.
    pub nodes: u32,
    /// Bytes per instance record.
    pub record: u32,
    /// Code buffer: shader at 0, indirect buffer at [`IB_OFFSET`].
    pub code: Vec<u8>,
    /// SPIR-V for the Vulkan run.
    pub spv: Vec<u8>,
    /// `job.txt` for `amdgpu_runner`.
    pub job: String,
    /// `bench.txt` for `vk_bench`.
    pub vk: String,
    /// Buffer names, in binding order.
    pub buffers: Vec<&'static str>,
    /// Our code size (dwords), VGPRs and SGPRs, for the report.
    pub stats: (usize, u32, u32),
    /// Input files carried in the script (`kind == "files"`): name, bytes.
    pub inputs: Vec<(String, Vec<u8>)>,
    /// Output buffer both drivers must agree on (`kind == "files"`).
    pub compare: Option<&'static str>,
}

/// The glyph total `bench_tool gen glyph <nodes>` produces (same generator
/// and draw order as `cases::desktop`).
pub fn glyph_total(nodes: u32) -> u32 {
    let mut r = crate::cases::Rng(0x5eed ^ nodes as u64);
    for _ in 0..4 + 16 * nodes as usize {
        r.next();
    }
    (0..nodes).map(|_| r.next() % 4).sum()
}

fn entry_name(spv: &[u8]) -> String {
    let w: Vec<u32> = spv.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let mut i = 5;
    while i < w.len() {
        let (len, op) = ((w[i] >> 16) as usize, w[i] & 0xffff);
        if op == 15 {
            let bytes: Vec<u8> = w[i + 3..i + len].iter().flat_map(|x| x.to_le_bytes()).collect();
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            return String::from_utf8_lossy(&bytes[..end]).into_owned();
        }
        i += len.max(1);
    }
    "main".into()
}

/// The benchmark for desktop shader `shader` (path under `bundles/`) at
/// `nodes` scene nodes, submitted `repeat` times per driver. `None` when the
/// SPIR-V is not built.
pub fn desktop(shader: &str, spv_path: &str, nodes: u32, record: u32, repeat: u32) -> Option<Bench> {
    let spv = std::fs::read(format!("{}/../bundles/{spv_path}", env!("CARGO_MANIFEST_DIR"))).ok()?;
    let m = atrium_spv_frontend::translate_with_options(&spv, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).expect("frontend");
    let mut g = atrium_gir::build::build(&m, 0).expect("gir");
    atrium_gir::passes::optimize(&mut g);
    atrium_gir::verify::verify(&g).expect("verify");
    let bin = atrium_rdna4_compiler::compile(&g).expect("rdna4 compile");
    let glyph = spv_path.contains("glyph");
    let n = nodes as u64;
    let max_inst = if glyph { glyph_total(nodes) as u64 } else { n };
    let (names, sizes): (Vec<&'static str>, Vec<u64>) = if glyph {
        (vec!["scene", "glyphs", "instances", "counter"], vec![16 + 64 * n, 48 * max_inst, record as u64 * max_inst, 4])
    } else {
        (vec!["scene", "instances", "counter"], vec![16 + 64 * n, record as u64 * max_inst, 4])
    };
    assert_eq!(bin.abi.buffers.len(), names.len());
    // The R9700 box has no resizable BAR: CPU-visible VRAM is 256 MiB, and
    // buffers beyond it spill (the first glyph_run runs took ~20 ms).
    assert!(sizes.iter().sum::<u64>() < 240 << 20, "{shader} at {nodes} nodes exceeds visible VRAM");
    let va = |i: usize| BENCH_VA + (i as u64) * 0x1000_0000;
    let groups = (nodes + 63) / 64 + 1;

    let mut user = vec![];
    for (set, binding) in &bin.abi.buffers {
        assert_eq!(*set, 0);
        let a = va(*binding as usize);
        user.extend([a as u32, (a >> 32) as u32]);
    }
    let mut ib = Blob::compute();
    ib.acquire_all();
    ib.timestamp(TS_VA);
    emit_dispatch(&mut ib, &Dispatch { shader_va: CODE_VA, vgprs: bin.vgprs, user_sgprs: user, threads: bin.local_size, groups: [groups, 1, 1], wave32: true, lds_bytes: bin.lds_bytes, scratch: None });
    ib.timestamp(TS_VA + 8);
    ib.pad(8);
    let mut code: Vec<u8> = bin.code.iter().flat_map(|d| d.to_le_bytes()).collect();
    assert!(code.len() <= IB_OFFSET as usize);
    code.resize(IB_OFFSET as usize, 0);
    code.extend(ib.bytes());

    let name = format!("{shader}_{nodes}");
    let input = |b: &str| if b == "scene" || b == "glyphs" { format!("{b}.in") } else { "-".into() };
    let mut job = format!("# atrium-rdna4-oracle bench {name}\nip compute\ntimeout_ms 10000\ndomain vram\nrepeat {repeat}\ntimestamps ts\n");
    job += &format!("bo code {CODE_VA:x} {} code.in 0\nbo ts {TS_VA:x} 4096 - 0\n", 0x2000);
    let mut vk = format!("spv shader.spv\nentry {}\ngroups {groups} 1 1\nrepeat {repeat}\n", entry_name(&spv));
    for (i, b) in names.iter().enumerate() {
        job += &format!("bo {b} {:x} {} {} 1\n", va(i), sizes[i], input(b));
        vk += &format!("buf {i} {} {} {b}\n", sizes[i], input(b));
    }
    job += &format!("ib code {IB_OFFSET} {}\n", ib.dw.len());
    Some(Bench {
        name,
        kind: if glyph { "glyph" } else { "plain" },
        nodes,
        record,
        code,
        spv,
        job,
        vk,
        buffers: names,
        stats: (bin.code.len(), bin.vgprs, bin.sgprs),
        inputs: vec![],
        compare: None,
    })
}

/// Every benchmark: the four desktop shaders at a small size (launch cost
/// dominates) and a large one (throughput).
pub fn all() -> Vec<Bench> {
    let mut v = vec![];
    for (nodes, repeat) in [(1u32 << 12, 50u32), (1 << 20, 20)] {
        for (n, p, rec) in crate::cases::DESKTOP {
            v.extend(desktop(n, p, nodes, rec, repeat));
        }
    }
    // glyph_run scaling: 1M took ~20 ms on both drivers, far above its
    // memory traffic; intermediate sizes show where it stops scaling.
    let (n, p, rec) = crate::cases::DESKTOP[3];
    for nodes in [1u32 << 16, 1 << 18] {
        v.extend(desktop(n, p, nodes, rec, 20));
    }
    v.extend(fresco(false, 20));
    v.extend(fresco(true, 20));
    v
}

/// The Fresco compute renderer on a 1920x1080 desktop-like scene: bin.comp
/// (`raster == false`) or raster.comp from the reference tile masks. The
/// frame's buffers travel in the script; both drivers must produce the same
/// mask / target.
pub fn fresco(raster: bool, repeat: u32) -> Option<Bench> {
    use fresco_compute::{binding, reference, Frame, BIN_SPV, RASTER_SPV};
    let (w, h, pitch) = (1920u32, 1080u32, 1920u32);
    let f = Frame::pack(&crate::fresco::desktop_scene(), w, h, pitch);
    let mut init = f.buffers();
    if raster {
        init[binding::MASK as usize] = reference::bin(&f).iter().flat_map(|x| x.to_le_bytes()).collect();
    }
    let spv = if raster { RASTER_SPV } else { BIN_SPV };
    let k = atrium_rdna4::Kernel::from_spirv(spv, &atrium_rdna4::Options::default()).ok()?;
    let groups = if raster { f.raster_groups() } else { f.bin_groups() };
    let names = ["frame", "ops", "tex", "texels", "mask", "target", "lut"];
    let sizes: Vec<u64> = (0..binding::COUNT)
        .map(|b| match b as u32 {
            binding::MASK => 4 * f.mask_words() as u64,
            binding::TARGET => 4 * f.target_words() as u64,
            _ => init[b].len() as u64,
        })
        .map(|n| n.max(16))
        .collect();
    assert!(sizes.iter().sum::<u64>() < 240 << 20);
    let va = |i: usize| BENCH_VA + (i as u64) * 0x1000_0000;
    let bind: Vec<((u32, u32), u64)> = k.buffers().iter().map(|&(s, b)| ((s, b), va(b as usize))).collect();
    let mut ib = Blob::compute();
    ib.acquire_all();
    ib.timestamp(TS_VA);
    emit_dispatch(&mut ib, &k.dispatch(CODE_VA, &bind, None, groups).ok()?);
    ib.timestamp(TS_VA + 8);
    ib.pad(8);
    let mut code = k.code();
    let ib_off = (IB_OFFSET as usize).max(code.len().next_multiple_of(0x1000));
    code.resize(ib_off, 0);
    code.extend(ib.bytes());
    let code_size = code.len().next_multiple_of(0x1000);
    let name = if raster { "fresco_raster_1080p" } else { "fresco_bin_1080p" }.to_string();
    let has_input = |b: usize| (b as u32 != binding::TARGET) && (raster || b as u32 != binding::MASK);
    let mut job = format!("# atrium-rdna4-oracle bench {name}\nip compute\ntimeout_ms 10000\ndomain vram\nrepeat {repeat}\ntimestamps ts\n");
    job += &format!("bo code {CODE_VA:x} {code_size} code.in 0\nbo ts {TS_VA:x} 4096 - 0\n");
    let mut vk = format!("spv shader.spv\nentry {}\ngroups {} {} {}\nrepeat {repeat}\n", entry_name(spv), groups[0], groups[1], groups[2]);
    let mut inputs = vec![];
    for (b, n) in names.iter().enumerate() {
        if !k.buffers().contains(&(0, b as u32)) {
            continue;
        }
        let input = if has_input(b) { format!("{n}.in") } else { "-".into() };
        if has_input(b) {
            inputs.push((format!("{n}.in"), init[b].clone()));
        }
        job += &format!("bo {n} {:x} {} {input} 1\n", va(b), sizes[b]);
        vk += &format!("buf {b} {} {input} {n}\n", sizes[b]);
    }
    job += &format!("ib code {ib_off} {}\n", ib.dw.len());
    let stats = (k.bin.code.len(), k.bin.vgprs, k.bin.sgprs);
    Some(Bench {
        name,
        kind: "files",
        nodes: f.ops.len() as u32,
        record: 0,
        code,
        spv: spv.to_vec(),
        job,
        vk,
        buffers: names.to_vec(),
        stats,
        inputs,
        compare: Some(if raster { "target" } else { "mask" }),
    })
}
