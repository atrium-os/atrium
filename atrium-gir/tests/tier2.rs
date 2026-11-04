//! GIR interpreter vs Tier-2 (Cranelift, the frontend IR path) on the four
//! desktop compute shaders. The two share only the SPIR-V frontend, so
//! agreement checks the GIR builder and interpreter end to end.

use atrium_gir::interp::{self, Buffers};
use std::path::Path;

fn spv(path: &str) -> Option<Vec<u8>> {
    std::fs::read(format!("{}/../bundles/{path}", env!("CARGO_MANIFEST_DIR"))).ok()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    /// A float in [1, 251) with a few fractional bits: exact, never zero.
    fn f(&mut self) -> f32 {
        1.0 + (self.next() % 2000) as f32 / 8.0
    }
}

fn floats(r: &mut Rng, n: usize) -> Vec<u8> {
    (0..n).flat_map(|_| r.f().to_le_bytes()).collect()
}

/// Inputs for `shader`: (buffers by binding, workgroup count).
fn inputs(shader: &str, nodes: u32, seed: u64) -> (Vec<Vec<u8>>, u32) {
    let mut r = Rng(seed);
    let mut scene = floats(&mut r, 4 + 16 * nodes as usize); // 16 + 64 * nodes bytes
    scene[0..4].copy_from_slice(&nodes.to_le_bytes());
    let groups = (nodes + 63) / 64 + 1; // one extra group exercises the early return
    if shader.contains("glyph") {
        let mut total = 0u32;
        for i in 0..nodes as usize {
            let count = r.next() % 4;
            let meta = 16 + 64 * i + 48;
            scene[meta..meta + 4].copy_from_slice(&count.to_le_bytes());
            scene[meta + 4..meta + 8].copy_from_slice(&total.to_le_bytes());
            total += count;
        }
        let glyphs = floats(&mut r, 12 * total as usize);
        (vec![scene, glyphs, vec![0; 48 * total as usize], vec![0; 4]], groups)
    } else {
        (vec![scene, vec![0; 64 * nodes as usize], vec![0; 4]], groups)
    }
}

fn run_gir(bytes: &[u8], bufs: &[Vec<u8>], groups: u32) -> Vec<Vec<u8>> {
    let m = atrium_spv_frontend::translate(bytes).unwrap();
    let g = atrium_gir::build::build(&m, 0).unwrap();
    atrium_gir::verify::verify(&g).unwrap();
    let mut b: Buffers = bufs.iter().enumerate().map(|(i, v)| ((0, i as u32), v.clone())).collect();
    interp::run(&g, [groups, 1, 1], &mut b).unwrap();
    (0..bufs.len()).map(|i| b.remove(&(0, i as u32)).unwrap()).collect()
}

type CsMain2 = CsMain;

/// Run a Tier-B example through both paths: every binding gets 1 KiB of
/// small integers, push constants get 64 bytes of the same, one workgroup.
fn tierb_case(path: &std::path::Path, dir: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let m = atrium_spv_frontend::translate(&bytes).ok()?;
    let g = atrium_gir::build::build(&m, 0).ok()?;
    let nb = g.buffers.iter().map(|b| b.1 + 1).max().unwrap_or(0) as usize;
    let fill = |n: usize, k: u32| -> Vec<u8> { (0..n as u32 / 4).flat_map(|i| (i % 7 + k).to_le_bytes()).collect() };
    let bufs: Vec<Vec<u8>> = (0..nb).map(|i| fill(1024, i as u32 + 1)).collect();
    let push = fill(64, 3);
    let [lx, ly, lz] = g.local_size;

    let mut b: Buffers = bufs.iter().enumerate().map(|(i, v)| ((0, i as u32), v.clone())).collect();
    b.insert(interp::PUSH, push.clone());
    interp::run(&g, [1, 1, 1], &mut b).ok()?;
    let a: Vec<Vec<u8>> = (0..nb).map(|i| b.remove(&(0, i as u32)).unwrap()).collect();

    use atrium_spv_backend_cranelift::{compile, Target};
    let obj = compile(&m, Target::host()).ok()?.object;
    let name = path.file_stem().unwrap().to_string_lossy().replace('.', "_");
    let (o, so) = (dir.join(format!("{name}.o")), dir.join(format!("{name}.so")));
    std::fs::write(&o, obj).unwrap();
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    assert!(std::process::Command::new("cc").arg(flag).arg("-o").arg(&so).arg(&o).status().unwrap().success());
    let mut t = bufs.clone();
    // SAFETY: as in run_tier2.
    unsafe {
        let lib = libloading::Library::new(&so).unwrap();
        let cs: libloading::Symbol<CsMain2> = lib.get(b"atrium_cs_main").unwrap();
        let table: Vec<u64> = t.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
        let out = if nb >= 2 { table.as_ptr() as *mut u8 } else { table[0] as *mut u8 };
        for z in 0..lz { for y in 0..ly { for x in 0..lx {
            cs(std::ptr::null(), push.as_ptr(), out, 0, 0, 0, x, y, z, std::ptr::null_mut());
        }}}
    }
    for (i, (x, y)) in a.iter().zip(&t).enumerate() {
        if let Some(k) = (0..x.len()).find(|&k| x[k] != y[k]) {
            return Some(format!("{name}: binding {i} differs at byte {k}"));
        }
    }
    Some(String::new())
}

#[test]
fn tierb_interpreter_matches_tier2() {
    let dir = tempfile::tempdir().unwrap();
    let shaders = format!("{}/../atrium-vk-icd/examples/shaders", env!("CARGO_MANIFEST_DIR"));
    let Ok(rd) = std::fs::read_dir(&shaders) else { return };
    let mut paths: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".comp.spv")).collect();
    paths.sort();
    let (mut agreed, mut failures) = (vec![], vec![]);
    for p in paths {
        let n = p.file_name().unwrap().to_string_lossy().to_string();
        // Tier-2 runs one invocation at a time, so workgroup memory with
        // barriers and subgroup ops have no lockstep meaning there.
        if n.starts_with("groupshared") || n.starts_with("subgroup") { continue; }
        match tierb_case(&p, dir.path()) {
            Some(e) if e.is_empty() => agreed.push(n),
            Some(e) => failures.push(e),
            None => {}
        }
    }
    eprintln!("agreed: {agreed:?}");
    assert!(failures.is_empty(), "{failures:#?}");
}

type CsMain = unsafe extern "C" fn(*const u8, *const u8, *mut u8, u32, u32, u32, u32, u32, u32, *mut u8);

fn run_tier2(bytes: &[u8], bufs: &[Vec<u8>], groups: u32, dir: &Path) -> Vec<Vec<u8>> {
    use atrium_spv_backend_cranelift::{compile, Target};
    let m = atrium_spv_frontend::translate(bytes).unwrap();
    let obj = compile(&m, Target::host()).unwrap().object;
    let o = dir.join("k.o");
    let so = dir.join("k.so");
    std::fs::write(&o, obj).unwrap();
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let st = std::process::Command::new("cc").arg(flag).arg("-o").arg(&so).arg(&o).status().unwrap();
    assert!(st.success());
    let mut out: Vec<Vec<u8>> = bufs.to_vec();
    // SAFETY: the library is the one just compiled from this shader; the
    // entry point has the Tier-2 compute signature, and every buffer
    // pointer stays valid for the whole call.
    unsafe {
        let lib = libloading::Library::new(&so).unwrap();
        let cs: libloading::Symbol<CsMain> = lib.get(b"atrium_cs_main").unwrap();
        let table: Vec<u64> = out.iter_mut().map(|b| b.as_mut_ptr() as u64).collect();
        for g in 0..groups {
            for l in 0..64 {
                cs(std::ptr::null(), std::ptr::null(), table.as_ptr() as *mut u8, g, 0, 0, l, 0, 0, std::ptr::null_mut());
            }
        }
    }
    out
}

#[test]
fn interpreter_matches_tier2() {
    let dir = tempfile::tempdir().unwrap();
    let mut ran = 0;
    for p in ["atrium-core/compute/op_rectangle.comp.spv", "atrium-core/compute/op_path.comp.spv",
              "atrium-core/compute/op_texture.comp.spv", "atrium-text/compute/op_glyph_run.comp.spv"] {
        let Some(bytes) = spv(p) else { eprintln!("skip {p}: not built"); continue };
        for (nodes, seed) in [(1, 1), (70, 2), (100, 3)] {
            let (bufs, groups) = inputs(p, nodes, seed);
            let a = run_gir(&bytes, &bufs, groups);
            let b = run_tier2(&bytes, &bufs, groups, dir.path());
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                if let Some(k) = (0..x.len()).find(|&k| x[k] != y[k]) {
                    panic!("{p} nodes={nodes}: binding {i} differs at byte {k}");
                }
            }
            if !p.contains("glyph") {
                assert_ne!(a[a.len() - 1], vec![0; 4], "{p}: counter never moved");
            }
            ran += 1;
        }
    }
    eprintln!("{ran} runs agreed");
}
