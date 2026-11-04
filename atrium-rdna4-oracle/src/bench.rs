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
    let m = atrium_spv_frontend::translate(&spv).expect("frontend");
    let mut g = atrium_gir::build::build(&m, 0).expect("gir");
    atrium_gir::passes::optimize(&mut g);
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
    ib.timestamp(TS_VA);
    emit_dispatch(&mut ib, &Dispatch { shader_va: CODE_VA, vgprs: bin.vgprs, user_sgprs: user, threads: bin.local_size, groups: [groups, 1, 1], wave32: true });
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
    v
}
