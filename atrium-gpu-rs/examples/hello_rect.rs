//! D1 step 2(a) smoke test — first software-rasterized pixel through the
//! Atrium native stack. Renders a rounded rect with a linear gradient
//! using tiny-skia, directly into the scanout BO's mmap'd memory, then
//! page-flips.
//!
//! No fresco-server, no Metal, no QEMU host compositing — pure FreeBSD
//! userspace + atrium_virtio_gpu kmod + virtio-gpu device.
//!
//! Note: the kmod hardcodes scanout format = `B8G8R8A8_UNORM`. tiny-skia
//! renders premultiplied RGBA. We do an in-place R↔B byte swap after
//! rasterization, before the page flip. Step 2(b) will plumb a format
//! parameter through `SET_MODE` so the swap goes away.
//!
//! Build:  cargo build --release --example hello_rect --features raster
//! Run:    /mnt/host/atrium-gpu-rs/target/release/examples/hello_rect

use atrium_gpu::abi::*;
use atrium_gpu::{Display, Gpu};
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, PixmapMut,
    Point, Rect, Shader, SpreadMode, Stroke, Transform,
};

/// Convert RGBA-laid-out pixels (what tiny-skia writes) to the BGRA
/// layout the kmod's hardcoded scanout expects. Single byte swap of
/// channels 0 and 2 per pixel.
fn rgba_to_bgra_in_place(buf: &mut [u32]) {
    for p in buf.iter_mut() {
        let r = *p & 0x0000_00ff;
        let b = (*p & 0x00ff_0000) >> 16;
        *p = (*p & 0xff00_ff00) | b | (r << 16);
    }
}

fn main() -> std::io::Result<()> {
    let gpu = Gpu::open()?;
    let dpy = Display::open()?;
    dpy.bind(&gpu)?;

    let connectors = dpy.connectors()?;
    let c = connectors[0].clone();
    let mode = dpy.preferred_mode(c.id)?;
    println!("hello_rect: {}x{} @ {} mHz", mode.width, mode.height, mode.refresh_mhz);

    let bytes = u64::from(mode.width) * u64::from(mode.height) * 4;
    let flags = ATRIUM_GPU_BO_GPU_VISIBLE
        | ATRIUM_GPU_BO_CPU_VISIBLE
        | ATRIUM_GPU_BO_COHERENT
        | ATRIUM_GPU_BO_SCANOUT;
    let mut bo = gpu.alloc(bytes, flags)?;

    // Render directly into the BO. tiny-skia's PixmapMut is exactly the
    // shape of our mmap'd region, so this is zero-copy.
    {
        let buf = bo.as_mut_slice();
        let mut pixmap = PixmapMut::from_bytes(buf, mode.width, mode.height)
            .expect("PixmapMut from BO mmap");

        // Dark navy background.
        pixmap.fill(Color::from_rgba8(0x14, 0x18, 0x22, 0xff));

        // Rounded rect with a diagonal gradient fill.
        let cx = mode.width as f32 / 2.0;
        let cy = mode.height as f32 / 2.0;
        let rw = (mode.width  as f32 * 0.55).max(200.0);
        let rh = (mode.height as f32 * 0.45).max(150.0);
        let x = cx - rw / 2.0;
        let y = cy - rh / 2.0;
        let r = 32.0;

        // Build a rounded-rect path with arcs at each corner.
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + rw - r, y);
        pb.quad_to(x + rw, y, x + rw, y + r);
        pb.line_to(x + rw, y + rh - r);
        pb.quad_to(x + rw, y + rh, x + rw - r, y + rh);
        pb.line_to(x + r, y + rh);
        pb.quad_to(x, y + rh, x, y + rh - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
        let path = pb.finish().expect("rounded-rect path");

        let mut fill = Paint::default();
        fill.shader = LinearGradient::new(
            Point::from_xy(x, y),
            Point::from_xy(x + rw, y + rh),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(0xff, 0x66, 0x88, 0xff)),
                GradientStop::new(1.0, Color::from_rgba8(0x44, 0x99, 0xff, 0xff)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(0xff, 0, 0xff, 0xff)));
        fill.anti_alias = true;
        pixmap.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);

        // White stroke outline.
        let mut stroke_paint = Paint::default();
        stroke_paint.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xff, 0xff, 0xc0));
        stroke_paint.anti_alias = true;
        let mut stroke = Stroke::default();
        stroke.width = 3.0;
        pixmap.stroke_path(&path, &stroke_paint, &stroke, Transform::identity(), None);

        // A solid accent rect in the top-left for "we control every pixel".
        let mut accent = Paint::default();
        accent.shader = Shader::SolidColor(Color::from_rgba8(0xff, 0xcc, 0x33, 0xff));
        let accent_path = PathBuilder::from_rect(
            Rect::from_xywh(48.0, 48.0, 96.0, 24.0).unwrap(),
        );
        pixmap.fill_path(&accent_path, &accent, FillRule::Winding, Transform::identity(), None);
    }

    // Swap R↔B for virtio-gpu BGRA scanout.
    rgba_to_bgra_in_place(bo.as_mut_typed::<u32>());

    dpy.set_mode(c.id, &bo, mode)?;
    dpy.page_flip(c.id, &bo)?;
    println!("hello_rect: rendered + page-flipped — check the QEMU window");
    Ok(())
}
