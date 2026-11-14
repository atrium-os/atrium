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
        fills: fresco_compute::fuzz::fills(),
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
    let opts = atrium_rdna4::Options { max_vgprs: Some(crate::cases::cap()), max_sgprs: Some(crate::cases::scap()), ..Default::default() };
    let k = atrium_rdna4::Kernel::from_spirv(spv, &opts).ok()?;
    let (ring, ring_buf) = crate::cases::scratch_ring(k.bin.scratch_dwords);
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
    emit_dispatch(&mut ib, &k.dispatch_with(CODE_VA, &bind, None, groups, ring.map(|r| (r.va, r.waves))).ok()?);
    ib.pad(8);
    let code: Vec<u32> = k.code().chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let (code, ib_off, ib_dw) = code_buffer(&code, &ib);
    buffers.insert(0, code);
    buffers.extend(ring_buf);
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
}

/// A whole frame as frescod records it ([`fresco_compute::rdna4::Rdna4`]):
/// bin, CS_PARTIAL_FLUSH and cache sync, raster, in one IB, from a zero
/// mask. Checks the target.
pub fn frame() -> Option<Case> {
    frame_of("fresco_frame", &scene(), W, H, PITCH)
}

/// [`frame`] for any scene and target.
pub fn frame_of(name: &str, scene: &Scene, w: u32, h: u32, pitch: u32) -> Option<Case> {
    damaged_frame_of(name, None, scene, w, h, pitch)
}

/// A damage-limited redraw as frescod records it: the target starts with
/// `before` rendered, only the tiles whose signature changed are rastered
/// (one dispatch per rectangle, at a nonzero COMPUTE_START), and the result
/// must equal a full render of `scene`. Checks the tile signatures and the
/// workgroup-id offset of COMPUTE_START_X/Y on the device.
pub fn damage() -> Option<Case> {
    let before = scene();
    let mut after = scene();
    after.rects[1].position[0] += 9.0; // moves into the 3rd and 4th tile columns
    after.paths[1].color = [0.2, 0.2, 0.9, 1.0];
    damaged_frame_of("fresco_damage", Some(&before), &after, W, H, PITCH)
}

fn damaged_frame_of(name: &str, before: Option<&Scene>, scene: &Scene, w: u32, h: u32, pitch: u32) -> Option<Case> {
    use fresco_compute::damage;
    let f = Frame::pack(scene, w, h, pitch);
    let mut init = f.buffers();
    let (hx, hy) = (f.header.tiles_x, f.header.tiles_y);
    let rects = match before {
        Some(b) => {
            let fb = Frame::pack(b, w, h, pitch);
            init[binding::TARGET as usize] = bytes(&reference::render(&fb));
            let d = damage::dirty(&damage::tile_signatures(&f), Some(&damage::tile_signatures(&fb)));
            damage::rects(&d, hx, hy, 16)
        }
        None => vec![[0, 0, hx, hy]],
    };
    frame_case(name, &f, init, &rects, false)
}

/// Exact group opacity (`ISOLATE`, fresco-scene-groups.md §2.4): the test
/// scene with its elements wrapped in nested isolated groups (two levels;
/// then four, and a fifth past ISOLATE_DEPTH) and a stray POP, against the
/// reference.
pub fn isolate() -> Option<Case> {
    let s = scene();
    let p = |item| Placed { item, clip: None };
    let push = |o: f32| p(Node::Push(o.to_bits()));
    let mut order = vec![p(Node::Pop), p(Node::Rect(0)), push(0.6), p(Node::Rect(1)), push(0.5), p(Node::Rect(2))];
    order.extend((0..s.paths.len()).map(|i| p(Node::Path(i))));
    order.push(p(Node::Pop));
    order.extend((0..s.fills.len()).map(|i| p(Node::Fill(i))));
    order.push(p(Node::Pop));
    order.extend([push(0.7), push(0.9), push(0.8)]);
    for (b, t) in s.texture_batches.iter().enumerate() {
        order.extend((0..t.quads.len()).map(|q| p(Node::Quad(b, q))));
    }
    order.extend([push(0.75), push(0.5)]);
    for (b, g) in s.glyph_batches.iter().enumerate() {
        order.extend((0..g.runs.len()).map(|r| p(Node::Run(b, r))));
    }
    // The last groups are left open: the kernel closes them after the last op.
    let (f, _) = Frame::pack_ordered(&s, &order, W, H, PITCH, None);
    let (hx, hy) = (f.header.tiles_x, f.header.tiles_y);
    frame_case("fresco_isolate", &f, f.buffers(), &[[0, 0, hx, hy]], false)
}

/// A hostile frame (F2): any frame [`Frame::validate`] accepts, e.g. from
/// [`fresco_compute::fuzz`]. Each buffer's bytes END at a page boundary
/// with an unmapped page after it, so a read or write one word past what
/// the header declares faults on the device. Checks the target against
/// the reference.
pub fn hostile(name: &str, f: &Frame) -> Option<Case> {
    f.validate(f.target_words()).ok()?;
    let (hx, hy) = (f.header.tiles_x, f.header.tiles_y);
    frame_case(name, f, f.buffers(), &[[0, 0, hx, hy]], true)
}

fn frame_case(name: &str, f: &Frame, init: Vec<Vec<u8>>, rects: &[[u32; 4]], guard: bool) -> Option<Case> {
    frame_case_sliced(name, f, init, rects, guard, None)
}

/// [`frame`]'s scene rastered as frescod splits a costly frame
/// (`fresco_compute::cost::split`, fresco-f4-split.md §F4j): bin parts by
/// op workgroups, then raster parts by tile rows and column pieces, each
/// part as it would be its own submit. Must equal the whole frame.
pub fn sliced() -> Option<Case> {
    let f = Frame::pack(&scene(), W, H, PITCH);
    let (hx, hy) = (f.header.tiles_x, f.header.tiles_y);
    let whole = [[0, 0, hx, hy]];
    let budget = (fresco_compute::cost::estimate(&f, &whole) / 12).max(1);
    frame_case_sliced("fresco_sliced", &f, f.buffers(), &whole, false, Some(budget))
}

fn frame_case_sliced(name: &str, f: &Frame, init: Vec<Vec<u8>>, rects: &[[u32; 4]], guard: bool, slice: Option<u64>) -> Option<Case> {
    use fresco_compute::rdna4::{Addrs, Rdna4};
    let k = Rdna4::new().ok()?;
    let (raster_off, code_end) = k.code_layout();
    // Buffers back to back from the first data address, each page aligned
    // (a 1080p target is larger than data_va's 1 MiB slots). With `guard`
    // each is end-aligned in its pages and an unmapped page follows it.
    let mut next = data_va(0);
    let pages = |n: usize| (n as u64).next_multiple_of(0x1000).max(0x1000);
    let base: [u64; binding::COUNT] = std::array::from_fn(|b| {
        let v = next;
        next += pages(init[b].len()) + if guard { 0x1000 } else { 0 };
        v
    });
    let lead: [u64; binding::COUNT] =
        std::array::from_fn(|b| if guard { pages(init[b].len()) - init[b].len() as u64 } else { 0 });
    let va: [u64; binding::COUNT] = std::array::from_fn(|b| base[b] + lead[b]);
    let a = Addrs { code: [CODE_VA, CODE_VA + raster_off], buffers: va };
    let mut ib = Blob::compute();
    match slice {
        None => k.record_tiles(&mut ib, &a, f, rects).ok()?,
        Some(budget) => {
            for p in fresco_compute::cost::split(f, rects, budget) {
                k.record_part(&mut ib, &a, f, p.bin, &p.tiles).ok()?;
            }
        }
    }
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
    let target = bytes(&reference::render(f));
    for (b, init) in init.into_iter().enumerate() {
        let pad = vec![0u8; lead[b] as usize];
        let expect = (b as u32 == binding::TARGET).then(|| [&pad[..], &target].concat());
        let size = pages(init.len());
        let init = [pad, init].concat();
        buffers.push(Buffer { name: format!("b0_{b}"), va: base[b], size, init, expect, unordered: None });
    }
    Some(Case { name: name.into(), buffers, ib: ("code".into(), ib_off, ib.dw.len() as u64), ip: "compute" })
}

/// `n` [`hostile`] cases from the F2 fuzz corpus ([`fresco_compute::fuzz`]).
pub fn hostile_corpus(seed: u64, n: usize) -> Vec<Case> {
    let mut r = fresco_compute::fuzz::Rng(seed);
    (0..n)
        .filter_map(|i| hostile(&format!("fresco_hostile_{seed:x}_{i}"), &fresco_compute::fuzz::frame(&mut r)))
        .collect()
}

/// compose.comp (F4) over a layer list and heap: the target against the
/// reference. With `guard`, each buffer ends at a page with an unmapped
/// page after it.
pub fn compose_case(name: &str, c: &fresco_compute::compose::Compose, heap: &[u32], guard: bool) -> Option<Case> {
    use fresco_compute::compose::{binding as cb, render};
    c.validate(c.target_words(), heap.len()).ok()?;
    let k = fresco_compute::rdna4::Compose::new().ok()?;
    let [hdr, layers, lut] = c.buffers();
    let mut init = vec![Vec::new(); cb::COUNT];
    init[cb::HEADER as usize] = hdr;
    init[cb::LAYERS as usize] = layers;
    init[cb::HEAP as usize] = bytes(heap);
    init[cb::TARGET as usize] = vec![0; c.target_words() * 4];
    init[cb::LUT as usize] = lut;
    let pages = |n: usize| (n as u64).next_multiple_of(0x1000).max(0x1000);
    let mut next = data_va(0);
    let base: [u64; cb::COUNT] = std::array::from_fn(|b| {
        let v = next;
        next += pages(init[b].len()) + if guard { 0x1000 } else { 0 };
        v
    });
    let lead: [u64; cb::COUNT] = std::array::from_fn(|b| if guard { pages(init[b].len()) - init[b].len() as u64 } else { 0 });
    let va: [u64; cb::COUNT] = std::array::from_fn(|b| base[b] + lead[b]);
    let mut ib = Blob::compute();
    k.record(&mut ib, CODE_VA, &va, c, &[]).ok()?;
    ib.pad(8);
    let mut code = k.code();
    let ib_off = (code.len() as u64).next_multiple_of(0x1000);
    code.resize(ib_off as usize, 0);
    code.extend(ib.bytes());
    let size = (code.len() as u64).next_multiple_of(0x1000);
    let mut buffers = vec![Buffer { name: "code".into(), va: CODE_VA, size, init: code, expect: None, unordered: None }];
    let target = bytes(&render(c, heap));
    for (b, init) in init.into_iter().enumerate() {
        let pad = vec![0u8; lead[b] as usize];
        let expect = (b as u32 == cb::TARGET).then(|| [&pad[..], &target].concat());
        let size = pages(init.len());
        buffers.push(Buffer { name: format!("c0_{b}"), va: base[b], size, init: [pad, init].concat(), expect, unordered: None });
    }
    Some(Case { name: name.into(), buffers, ib: ("code".into(), ib_off, ib.dw.len() as u64), ip: "compute" })
}

/// The compose oracle case: one fixed fuzz layer list (translucent,
/// clipped and scrolled layers) for the R9700 corpus.
pub fn compose() -> Option<Case> {
    let mut r = fresco_compute::fuzz::Rng(0xc0_4e);
    loop {
        let (c, heap) = fresco_compute::fuzz::compose_case(&mut r);
        let n = c.header.layer_count as usize;
        let ls = &c.layers[..n];
        if n >= 4 && ls.iter().any(|l| l.opacity > 0.0 && l.opacity < 1.0) && ls.iter().any(|l| l.origin != [l.rect[0], l.rect[1]]) {
            return compose_case("fresco_compose", &c, &heap, false);
        }
    }
}

/// `n` guarded compose cases from the fuzz corpus.
pub fn compose_corpus(seed: u64, n: usize) -> Vec<Case> {
    let mut r = fresco_compute::fuzz::Rng(seed);
    (0..n)
        .filter_map(|i| {
            let (c, heap) = fresco_compute::fuzz::compose_case(&mut r);
            compose_case(&format!("fresco_compose_{seed:x}_{i}"), &c, &heap, true)
        })
        .collect()
}

/// A scrolled frame as frescod's GPU renderer records it
/// (fresco-scene-groups.md §4): the target starts with the test scene
/// rendered, the next frame shows it moved by `d` whole pixels, and the
/// command stream copies what is still on screen there through a scratch
/// buffer with shift.comp (two passes, a barrier after each), then rasters
/// only the tiles [`damage::plan`] leaves to draw. The result must equal,
/// bit for bit, what the CPU renderer makes of the same frames
/// ([`shifted_reference`]): the moved scene's full render, except that a
/// filled path moved by whole pixels can differ from a fresh raster by one
/// step in a few pixels' channels.
pub fn shift() -> Option<Case> {
    shift_by("fresco_shift", [0, -7])
}

/// [`shift`] by any `d`.
pub fn shift_by(name: &str, d: [i32; 2]) -> Option<Case> {
    use fresco_compute::damage;
    use fresco_compute::gpu::Arch;
    let before = scene();
    let after = moved(&before, d);
    let (fb, f) = (Frame::pack(&before, W, H, PITCH), Frame::pack(&after, W, H, PITCH));
    let sig = damage::tile_signatures(&f);
    let mut held = damage::tile_signatures(&fb);
    let s = damage::Shift { area: [0, 0, W, H], d, over: vec![], over_before: vec![] };
    let (moves, out) = damage::plan(W, H, &sig, &mut held, &[s]);
    if moves.is_empty() {
        return None;
    }
    let rects = damage::tiles_after_plan(W, H, &sig, &held, &out.pixels, 16);

    let target_words = f.target_words() as u32;
    let mut scratch_words = 0;
    let mut hdrs = Vec::new();
    for m in &moves {
        let size = [m.dst[2] - m.dst[0], m.dst[3] - m.dst[1]];
        let sr = m.src();
        let (src, dst) = (sr[1] * PITCH + sr[0], m.dst[1] * PITCH + m.dst[0]);
        hdrs.push((size, scratch_words, src, dst));
        scratch_words += size[0] * size[1];
    }
    // Header pairs (out, back), 256 bytes apart.
    let mut header = Vec::new();
    for &(size, at, src, dst) in &hdrs {
        for h in [
            damage::move_header(size, (src, PITCH, target_words), (at, size[0], scratch_words)),
            damage::move_header(size, (at, size[0], scratch_words), (dst, PITCH, target_words)),
        ] {
            header.resize(header.len().next_multiple_of(256), 0);
            header.extend(bytes(&h));
        }
    }

    let arch = fresco_compute::rdna4::Rdna4Arch::new().ok()?;
    let mut init = f.buffers();
    init[binding::TARGET as usize] = bytes(&reference::render(&fb));
    let pages = |n: usize| (n as u64).next_multiple_of(0x1000).max(0x1000);
    let mut next = data_va(0);
    let mut alloc = |n: usize| {
        let v = next;
        next += pages(n);
        v
    };
    let va: [u64; binding::COUNT] = std::array::from_fn(|b| alloc(init[b].len()));
    let header_va = alloc(header.len());
    let scratch_va = alloc(scratch_words as usize * 4);

    let mut ib = arch.stream();
    let surf = va[binding::TARGET as usize];
    for (i, &(size, ..)) in hdrs.iter().enumerate() {
        ib.shift(CODE_VA, header_va + 512 * i as u64, surf, scratch_va, size).ok()?;
    }
    ib.barrier();
    for (i, &(size, ..)) in hdrs.iter().enumerate() {
        ib.shift(CODE_VA, header_va + 512 * i as u64 + 256, scratch_va, surf, size).ok()?;
    }
    ib.barrier();
    if !rects.is_empty() {
        ib.frame(CODE_VA, &va, &f, &rects).ok()?;
    }
    let mut blob = Blob::compute();
    blob.dw = ib.bytes().chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    blob.pad(8);

    let mut code = arch.code();
    let ib_off = (code.len() as u64).next_multiple_of(0x1000);
    code.resize(ib_off as usize, 0);
    code.extend(blob.bytes());
    let size = (code.len() as u64).next_multiple_of(0x1000);
    let mut buffers = vec![Buffer { name: "code".into(), va: CODE_VA, size, init: code, expect: None, unordered: None }];
    let target = bytes(&shifted_reference(&before, &after, d));
    for (b, init) in init.into_iter().enumerate() {
        let expect = (b as u32 == binding::TARGET).then(|| target.clone());
        buffers.push(Buffer { name: format!("b0_{b}"), va: va[b], size: pages(init.len()), init, expect, unordered: None });
    }
    buffers.push(Buffer { name: "shift_hdr".into(), va: header_va, size: pages(header.len()), init: header, expect: None, unordered: None });
    buffers.push(Buffer { name: "scratch".into(), va: scratch_va, size: pages(scratch_words as usize * 4), init: vec![], expect: None, unordered: None });
    Some(Case { name: name.into(), buffers, ib: ("code".into(), ib_off, blob.dw.len() as u64), ip: "compute" })
}

/// What the CPU renderer draws for `after` over `before`'s render when
/// the whole target scrolled by `d`: [`damage::shift`], then the tiles
/// left to draw from a full render of `after`.
pub fn shifted_reference(before: &Scene, after: &Scene, d: [i32; 2]) -> Vec<u32> {
    use fresco_compute::damage;
    let (fb, f) = (Frame::pack(before, W, H, PITCH), Frame::pack(after, W, H, PITCH));
    let sig = damage::tile_signatures(&f);
    let mut held = damage::tile_signatures(&fb);
    let s = damage::Shift { area: [0, 0, W, H], d, over: vec![], over_before: vec![] };
    let mut px = reference::render(&fb);
    let out = damage::shift(&mut px, PITCH, H, &sig, &mut held, &[s]);
    let want = reference::render(&f);
    for r in damage::tiles_after_plan(W, H, &sig, &held, &out.pixels, 16) {
        for y in r[1] * TILE..(r[3] * TILE).min(H) {
            for x in r[0] * TILE..(r[2] * TILE).min(W) {
                let i = (y * PITCH + x) as usize;
                px[i] = want[i];
            }
        }
    }
    px
}

/// `s` with everything moved by `d` whole pixels.
pub fn moved(s: &Scene, d: [i32; 2]) -> Scene {
    let (dx, dy) = (d[0] as f32, d[1] as f32);
    let mut s = s.clone();
    for r in &mut s.rects {
        r.position[0] += dx;
        r.position[1] += dy;
    }
    for p in &mut s.paths {
        p.model[0] += dx;
        p.model[1] += dy;
    }
    for f in &mut s.fills {
        f.transform[4] += dx;
        f.transform[5] += dy;
        if let Some(c) = &mut f.clip {
            *c = [c[0] + dx, c[1] + dy, c[2] + dx, c[3] + dy];
        }
    }
    for b in &mut s.texture_batches {
        for q in &mut b.quads {
            q[0] += dx;
            q[1] += dy;
        }
    }
    for b in &mut s.glyph_batches {
        for r in &mut b.runs {
            r.origin[0] += dx;
            r.origin[1] += dy;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CPU model the gpusim case checks against is a scroll: within
    /// one step per channel of a full render (filled paths are not exact
    /// under whole-pixel moves), and exact without fills.
    #[test]
    fn shifted_reference_is_a_scroll() {
        for d in [[0, -7], [0, 9], [-5, 3], [5, -7], [-9, 0]] {
            let s = scene();
            let got = shifted_reference(&s, &moved(&s, d), d);
            let want = reference::render(&Frame::pack(&moved(&s, d), W, H, PITCH));
            for (i, (a, b)) in got.iter().zip(&want).enumerate() {
                let close = (0..4).all(|c| ((a >> (8 * c)) & 0xff).abs_diff((b >> (8 * c)) & 0xff) <= 1);
                assert!(close, "d {d:?} at word {i}: {a:#x} vs {b:#x}");
            }
            let mut s = scene();
            s.fills.clear();
            assert_eq!(shifted_reference(&s, &moved(&s, d), d), reference::render(&Frame::pack(&moved(&s, d), W, H, PITCH)), "d {d:?}");
        }
    }
}
