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
