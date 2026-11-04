//! The Fresco compute renderer (fresco-compute's bin.comp and raster.comp)
//! as oracle cases, checked against its Rust reference renderer: the tile
//! masks bit for bit, the target pixel for pixel.

use crate::cases::{code_buffer, data_va, CODE_VA};
use crate::{Buffer, Case};
use atrium_rdna4_cmd::{emit_dispatch, Blob};
use fresco_compute::*;

/// The fresco-compute test scene: every op kind, overlaps, translucency, a
/// rounded corner, a rotated path and an edge-clamped texture, on a 72x40
/// target with padding (copied from fresco-compute/tests/kernels.rs).
pub fn scene() -> Scene {
    let rgba: Vec<u8> = (0..16u32)
        .flat_map(|i| [(i * 16) as u8, (255 - i * 16) as u8, (i * 37 % 256) as u8, if i % 3 == 0 { 128 } else { 255 }])
        .collect();
    let atlas: Vec<u8> = (0..64u32).map(|i| ((i * 29 + 7) % 256) as u8).collect();
    Scene {
        rects: vec![
            Rect { position: [2.0, 3.0], size: [30.0, 20.0], color: [0.9, 0.2, 0.1, 1.0], radius: 6.0 },
            Rect { position: [20.5, 10.25], size: [25.0, 18.0], color: [0.1, 0.4, 0.9, 0.5], radius: 0.0 },
            Rect { position: [-5.0, 30.0], size: [20.0, 20.0], color: [1.0, 1.0, 1.0, 0.75], radius: 30.0 },
        ],
        paths: vec![
            Path { model: [50.0, 20.0, 30.0, 3.0], angle: 0.6, color: [0.0, 0.8, 0.2, 1.0] },
            Path { model: [36.0, 5.0, 20.0, 2.0], angle: 0.0, color: [0.8, 0.8, 0.0, 0.6] },
        ],
        textures: vec![
            Texture { width: 4, height: 4, format: TexFormat::Rgba8Srgb, data: rgba },
            Texture { width: 8, height: 8, format: TexFormat::R8, data: atlas },
        ],
        texture_batches: vec![TextureBatch { texture: 0, quads: vec![[44.0, 24.0, 24.0, 14.0], [66.0, -2.0, 10.0, 10.0]] }],
        glyph_batches: vec![GlyphBatch {
            atlas: 1,
            runs: vec![GlyphRun {
                origin: [4.0, 36.0],
                color: [0.05, 0.05, 0.1, 1.0],
                glyphs: vec![
                    Glyph { d_offset: [0.0, 0.0], atlas_uv: [0.0, 0.0, 4.0, 8.0], bearing: [0.0, 7.0] },
                    Glyph { d_offset: [5.0, 0.0], atlas_uv: [4.0, 0.0, 4.0, 8.0], bearing: [1.0, 6.0] },
                ],
            }],
        }],
    }
}

/// Target size of the scene.
pub const W: u32 = 72;
/// Target height.
pub const H: u32 = 40;
/// Target pitch in pixels.
pub const PITCH: u32 = 80;

fn bytes(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// One kernel over the frame's buffers, `expect` naming the checked
/// binding and its expected contents.
fn case(name: &str, spv: &[u8], init: Vec<Vec<u8>>, groups: [u32; 3], expect: (u32, Vec<u8>)) -> Option<Case> {
    let k = atrium_rdna4::Kernel::from_spirv(spv, &atrium_rdna4::Options::default()).ok()?;
    let mut buffers = vec![];
    let mut bind = vec![];
    for (i, (set, b)) in k.buffers().iter().enumerate() {
        let va = data_va(i);
        let init = init.get(*b as usize).cloned().unwrap_or_default();
        let size = (init.len().max(expect.1.len()) as u64).next_multiple_of(0x1000).max(0x1000);
        assert!(size <= 0x10_0000, "{name}: buffer {b} larger than its 1 MiB slot");
        buffers.push(Buffer {
            name: format!("b{set}_{b}"),
            va,
            size,
            init,
            expect: (*b == expect.0).then(|| expect.1.clone()),
            unordered: None,
        });
        bind.push(((*set, *b), va));
    }
    let mut ib = Blob::compute();
    ib.acquire_all();
    emit_dispatch(&mut ib, &k.dispatch(CODE_VA, &bind, None, groups).ok()?);
    ib.pad(8);
    let code: Vec<u32> = k.code().chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let (code, ib_off, ib_dw) = code_buffer(&code, &ib);
    buffers.insert(0, code);
    Some(Case { name: name.into(), buffers, ib: ("code".into(), ib_off, ib_dw), ip: "compute" })
}

/// bin.comp: the per-tile op masks.
pub fn bin() -> Option<Case> {
    let f = Frame::pack(&scene(), W, H, PITCH);
    let mask = bytes(&reference::bin(&f));
    case("fresco_bin", BIN_SPV, f.buffers(), f.bin_groups(), (binding::MASK, mask))
}

/// raster.comp, from the reference masks: the BGRA8 target.
pub fn raster() -> Option<Case> {
    let f = Frame::pack(&scene(), W, H, PITCH);
    let mut init = f.buffers();
    init[binding::MASK as usize] = bytes(&reference::bin(&f));
    let target = bytes(&reference::render(&f));
    case("fresco_raster", RASTER_SPV, init, f.raster_groups(), (binding::TARGET, target))
}

<<<<<<< Updated upstream
/// A 1920x1080 desktop-like scene for timing: overlapping rounded windows,
/// translucent panels, strokes, image quads and text runs (deterministic).
pub fn desktop_scene() -> Scene {
    let mut r = crate::cases::Rng(0xf2e5c0);
    let mut f = |lo: f32, hi: f32| lo + (r.next() % 10_000) as f32 / 10_000.0 * (hi - lo);
    let mut s = scene();
    s.rects.clear();
    s.paths.clear();
    s.texture_batches[0].quads.clear();
    s.glyph_batches[0].runs.clear();
    for i in 0..96 {
        let (w, h) = (f(120.0, 900.0), f(80.0, 640.0));
        s.rects.push(Rect {
            position: [f(-60.0, 1920.0 - w * 0.5), f(-40.0, 1080.0 - h * 0.5)],
            size: [w, h],
            color: [f(0.0, 1.0), f(0.0, 1.0), f(0.0, 1.0), if i % 3 == 0 { f(0.3, 0.9) } else { 1.0 }],
            radius: if i % 2 == 0 { f(0.0, 24.0) } else { 0.0 },
        });
    }
    for _ in 0..256 {
        s.paths.push(Path {
            model: [f(0.0, 1920.0), f(0.0, 1080.0), f(10.0, 400.0), f(1.0, 6.0)],
            angle: f(0.0, 3.14),
            color: [f(0.0, 1.0), f(0.0, 1.0), f(0.0, 1.0), f(0.5, 1.0)],
        });
    }
    for _ in 0..48 {
        s.texture_batches[0].quads.push([f(0.0, 1800.0), f(0.0, 1000.0), f(16.0, 256.0), f(16.0, 256.0)]);
    }
    for _ in 0..60 {
        let origin = [f(0.0, 1700.0), f(12.0, 1080.0)];
        let glyphs = (0..24)
            .map(|g| Glyph { d_offset: [g as f32 * 9.0, 0.0], atlas_uv: [(g % 2) as f32 * 4.0, 0.0, 4.0, 8.0], bearing: [0.0, 7.0] })
            .collect();
        s.glyph_batches[0].runs.push(GlyphRun { origin, color: [0.05, 0.05, 0.1, 1.0], glyphs });
    }
    s
=======
/// A whole frame as frescod records it ([`fresco_compute::rdna4::Rdna4`]):
/// bin, CS_PARTIAL_FLUSH and cache sync, raster, in one IB, from a zero
/// mask. Checks the target.
pub fn frame() -> Option<Case> {
    frame_of("fresco_frame", &scene(), W, H, PITCH)
}

/// [`frame`] for any scene and target.
pub fn frame_of(name: &str, scene: &Scene, w: u32, h: u32, pitch: u32) -> Option<Case> {
    use fresco_compute::rdna4::{Addrs, Rdna4};
    let k = Rdna4::new().ok()?;
    let f = Frame::pack(scene, w, h, pitch);
    let (raster_off, code_end) = k.code_layout();
    // Buffers back to back from the first data address, each page aligned
    // (a 1080p target is larger than data_va's 1 MiB slots).
    let init = f.buffers();
    let mut next = data_va(0);
    let va: [u64; binding::COUNT] = std::array::from_fn(|b| {
        let v = next;
        next += (init[b].len() as u64).next_multiple_of(0x1000).max(0x1000);
        v
    });
    let a = Addrs { code: [CODE_VA, CODE_VA + raster_off], buffers: va };
    let mut ib = Blob::compute();
    k.record(&mut ib, &a, &f).ok()?;
    ib.pad(8);
    let [bin, raster] = k.code();
    let mut code = bin;
    code.resize(raster_off as usize, 0);
    code.extend(raster);
    let ib_off = code_end.next_multiple_of(0x1000);
    code.resize(ib_off as usize, 0);
    code.extend(ib.bytes());
    let size = (code.len() as u64).next_multiple_of(0x1000);
    let mut buffers = vec![Buffer { name: "code".into(), va: CODE_VA, size, init: code, expect: None, unordered: None }];
    let target = bytes(&reference::render(&f));
    for (b, init) in init.into_iter().enumerate() {
        let expect = (b as u32 == binding::TARGET).then(|| target.clone());
        let size = (init.len() as u64).next_multiple_of(0x1000).max(0x1000);
        buffers.push(Buffer { name: format!("b0_{b}"), va: va[b], size, init, expect, unordered: None });
    }
    Some(Case { name: name.into(), buffers, ib: ("code".into(), ib_off, ib.dw.len() as u64), ip: "compute" })
>>>>>>> Stashed changes
}
