//! Rust port of `atrium-kmod/test_scanout.c` — proves the safe
//! bindings drive the full Atrium scanout chain end-to-end.

use atrium_gpu::abi::*;
use atrium_gpu::{Display, Gpu};

fn main() -> std::io::Result<()> {
    let gpu = Gpu::open()?;
    let dpy = Display::open()?;
    dpy.bind(&gpu)?;

    println!("family={}", gpu.family()?);

    let connectors = dpy.connectors()?;
    println!("connectors: {connectors:?}");
    let c = &connectors[0];
    let mode = dpy.preferred_mode(c.id)?;
    println!("mode: {}x{} @ {} mHz", mode.width, mode.height, mode.refresh_mhz);

    let bytes = (mode.width as u64) * (mode.height as u64) * 4;
    let flags = ATRIUM_GPU_BO_GPU_VISIBLE | ATRIUM_GPU_BO_CPU_VISIBLE
              | ATRIUM_GPU_BO_COHERENT    | ATRIUM_GPU_BO_SCANOUT;
    let mut bo = gpu.alloc(bytes, flags)?;

    // Solid fill (BGRA = 0xff 2266aa).
    {
        let fb: &mut [u32] = bo.as_mut_typed::<u32>();
        for px in fb.iter_mut() { *px = 0xff_22_66_aa; }
    }
    dpy.set_mode(c.id, &bo, mode)?;
    dpy.page_flip(c.id, &bo)?;
    println!("page_flip 1 (solid) ok");

    // Gradient (red x, green y).
    {
        let fb: &mut [u32] = bo.as_mut_typed::<u32>();
        for y in 0..mode.height {
            for x in 0..mode.width {
                let r = ((x as u32) * 255) / mode.width;
                let g = ((y as u32) * 255) / mode.height;
                fb[(y * mode.width + x) as usize] = 0xff00_0000 | (r << 16) | (g << 8);
            }
        }
    }
    dpy.page_flip(c.id, &bo)?;
    println!("page_flip 2 (gradient) ok");
    Ok(())
}
