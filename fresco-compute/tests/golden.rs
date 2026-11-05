//! fresco-compute against `fresco_vulkan::HeadlessRenderer`, the renderer it
//! replaces, on whatever Vulkan ICD the loader finds (lavapipe in CI:
//! VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json). Needs the
//! bundles built (bundles/*/build.sh). Skips when either is missing.
//!
//! The two can't agree bit for bit: the Vulkan path blends each draw in an
//! 8-bit sRGB attachment (quantising between draws), takes fwidth() from
//! 2x2 quad differences and filters textures in fixed point. So the test
//! bounds the difference instead.

use std::path::Path as FsPath;

use ash::vk;
use fresco_compute as fc;
use fresco_compute::*;
use fresco_vulkan as fv;

#[path = "kernels.rs"]
#[allow(dead_code)]
mod kernels;

fn bundles() -> Option<Vec<std::path::PathBuf>> {
    let root = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../bundles");
    let core = root.join("atrium-core");
    let text = root.join("atrium-text");
    if !core.join("compute/op_rectangle.comp.spv").exists() || !text.join("compute/op_glyph_run.comp.spv").exists() {
        return None;
    }
    Some(vec![core, text])
}

/// Render `scene` with the Vulkan renderer, BGRA8 words.
fn vulkan(scene: &fc::Scene, w: u32, h: u32) -> Option<Vec<u32>> {
    let b = bundles()?;
    let mut r = match fv::HeadlessRenderer::new(w, h) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip: no Vulkan ({e})");
            return None;
        }
    };
    for p in b {
        r.load_bundle(&p).expect("load_bundle");
    }
    let uploads = scene.textures.iter().enumerate().map(|(i, t)| fv::UploadRequest::Texture {
        slot_id: 100 + i as u32,
        bytes: t.data.clone(),
        width: t.width,
        height: t.height,
        format: match t.format {
            fc::TexFormat::Rgba8Srgb => vk::Format::R8G8B8A8_SRGB,
            fc::TexFormat::R8 => vk::Format::R8_UNORM,
        },
    }).collect();
    r.process_uploads(uploads, vec![]).expect("uploads");
    r.set_rect_nodes(scene.rects.iter().map(|n| fv::SceneNode {
        position: n.position, size: n.size, color: n.color, radius: n.radius, _pad: [0.0; 3],
    }).collect());
    r.set_path_nodes(scene.paths.iter().map(|n| fv::PathNode {
        model: n.model, extra: [n.angle, 0.0, 0.0, 0.0], color: n.color,
    }).collect());
    r.set_texture_batches(scene.texture_batches.iter().map(|b| fv::TextureBatch {
        slot_id: 100 + b.texture as u32,
        nodes: b.quads.iter().map(|q| fv::TextureNode { model: *q }).collect(),
    }).collect());
    r.set_glyph_run_batches(scene.glyph_batches.iter().map(|b| {
        let t = &scene.textures[b.atlas];
        let mut nodes = vec![];
        let mut glyphs = vec![];
        for run in &b.runs {
            nodes.push(fv::GlyphRunNode {
                origin: [run.origin[0], run.origin[1], 0.0, 0.0],
                atlas_dim: [t.width as f32, t.height as f32, 0.0, 0.0],
                color: run.color,
                meta: [run.glyphs.len() as i32, glyphs.len() as i32, 0, 0],
            });
            glyphs.extend(run.glyphs.iter().map(|g| fv::GlyphInstance {
                d_offset: g.d_offset, atlas_uv: g.atlas_uv, bearing: g.bearing, ..Default::default()
            }));
        }
        fv::GlyphRunBatch { atlas_slot_id: 100 + b.atlas as u32, nodes, glyphs }
    }).collect());
    r.render_to_buffer().expect("render");
    let px = r.read_pixels_vec().expect("read_pixels");
    Some(px.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], 255])).collect())
}

struct Stats { differ: usize, over2: usize, over8: usize, worst: u32, worst_at: (u32, u32) }

fn compare(a: &[u32], b: &[u32], w: u32) -> Stats {
    let mut s = Stats { differ: 0, over2: 0, over8: 0, worst: 0, worst_at: (0, 0) };
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let d = [0, 8, 16].iter().map(|sh| ((x >> sh) & 255).abs_diff((y >> sh) & 255)).max().unwrap();
        if d > 0 { s.differ += 1; }
        if d > 2 { s.over2 += 1; }
        if d > 8 { s.over8 += 1; }
        if d > s.worst { s.worst = d; s.worst_at = (i as u32 % w, i as u32 / w); }
    }
    s
}

fn check(name: &str, scene: &fc::Scene, w: u32, h: u32) -> Option<Stats> {
    let v = vulkan(scene, w, h)?;
    let f = fc::Frame::pack(scene, w, h, w);
    let c = fc::reference::render(&f);
    let s = compare(&c, &v, w);
    eprintln!(
        "{name}: {}x{h}: {} pixels differ, {} by more than 2, {} by more than 8, worst {} at {:?}",
        w, s.differ, s.over2, s.over8, s.worst, s.worst_at
    );
    if let Some(dir) = std::env::var_os("GOLDEN_DUMP") {
        for (tag, img) in [("compute", &c), ("vulkan", &v)] {
            let mut ppm = format!("P6 {w} {h} 255\n").into_bytes();
            for p in img.iter() {
                let [b, g, r, _] = p.to_le_bytes();
                ppm.extend_from_slice(&[r, g, b]);
            }
            std::fs::write(FsPath::new(&dir).join(format!("{name}-{tag}.ppm")), ppm).unwrap();
        }
    }
    Some(s)
}

#[test]
fn matches_vulkan_renderer() {
    // The Vulkan renderer has no filled paths.
    let scene = fc::Scene { fills: Vec::new(), ..kernels::scene() };
    let Some(s) = check("ops", &scene, kernels::W, kernels::H) else { return };
    let total = (kernels::W * kernels::H) as usize;
    assert!(s.over2 * 100 <= total, "{} of {total} pixels off by more than 2", s.over2);
    assert!(s.worst <= 8, "worst channel delta {}", s.worst);
}

fn desktop() -> Scene {
    let mut s = Scene::default();
    // windows: shadow, body, title bar, controls
    for (i, (x, y, w, h)) in [(80.0, 60.0, 900.0, 600.0), (700.0, 300.0, 800.0, 560.0), (1300.0, 120.0, 520.0, 380.0)].iter().enumerate() {
        s.rects.push(Rect { position: [x - 8.0, y - 4.0], size: [w + 16.0, h + 16.0], color: [0.0, 0.0, 0.0, 0.25], radius: 14.0 });
        s.rects.push(Rect { position: [*x, *y], size: [*w, *h], color: [0.95, 0.95, 0.96, 1.0], radius: 10.0 });
        s.rects.push(Rect { position: [*x, *y], size: [*w, 36.0], color: [0.3 + 0.2 * i as f32, 0.35, 0.5, 1.0], radius: 10.0 });
        for b in 0..3 {
            s.rects.push(Rect { position: [x + 12.0 + 22.0 * b as f32, y + 11.0], size: [14.0, 14.0], color: [0.9, 0.3 + 0.3 * b as f32, 0.2, 1.0], radius: 7.0 });
        }
        for row in 0..10 {
            s.rects.push(Rect { position: [x + 20.0, y + 50.0 + 18.0 * row as f32], size: [w - 40.0 - (row * 7 % 200) as f32, 10.0], color: [0.2, 0.2, 0.25, 0.8], radius: 2.0 });
        }
    }
    // dock
    s.rects.push(Rect { position: [560.0, 1000.0], size: [800.0, 70.0], color: [0.1, 0.1, 0.12, 0.6], radius: 18.0 });
    for k in 0..12 {
        s.rects.push(Rect { position: [580.0 + 64.0 * k as f32, 1010.0], size: [50.0, 50.0], color: [0.2 + 0.05 * k as f32, 0.5, 0.8 - 0.05 * k as f32, 1.0], radius: 12.0 });
    }
    // a chart: path segments
    for k in 0..60 {
        let x = 120.0 + 12.0 * k as f32;
        s.paths.push(Path { model: [x, 500.0 + 60.0 * (k as f32 * 0.3).sin(), 13.0, 2.0], angle: 0.3 * (k as f32 * 0.3).cos(), color: [0.1, 0.5, 0.9, 1.0] });
    }
    s
}


/// Kept under 64 rects: the Vulkan renderer's instance order comes from an
/// atomic slot counter, so past one 64-wide workgroup its z-order between
/// rects is whatever order the workgroups ran in. fresco-compute keeps list
/// order; past 64 the two legitimately differ.
#[test]
fn desktop_matches_vulkan_renderer() {
    assert!(desktop().rects.len() < 64);
    let (w, h) = (1920, 1080);
    let Some(s) = check("desktop", &desktop(), w, h) else { return };
    let total = (w * h) as usize;
    // Measured on lavapipe: ~1.1 M pixels 1 level apart (sRGB encode
    // rounding), ~400 more than 2 and ~50 more than 8, the latter single
    // pixels on rounded corners (analytic fwidth vs 2x2 quad differences)
    // and on non-antialiased path edges (pixel centre on the edge).
    assert!(s.over2 * 1000 <= total, "{} of {total} pixels off by more than 2", s.over2);
    assert!(s.over8 * 10_000 <= total, "{} of {total} pixels off by more than 8", s.over8);
}
