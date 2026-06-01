//! P4 Route-A empirical probe (level 1): run fresco-vulkan's
//! `HeadlessRenderer` through whatever Vulkan ICD the loader
//! selects.  Point it at atrium-vk-icd + the tier2 daemon to verify
//! the compositor's core Vulkan path (instance / device / image /
//! render-pass clear / copy-to-buffer / readback) runs on Tier-2:
//!
//!   # terminal 1: tier2 daemon on a socket
//!   aqueduct-gpu-host --socket /tmp/p4.sock --backend tier2 --tier2 \
//!       --cache-root /tmp/p4-cache --compile-binary <atrium-spv-compile>
//!
//!   # terminal 2:
//!   VK_DRIVER_FILES=<atrium_icd.json> ATRIUM_VK_ICD_SOCKET=/tmp/p4.sock \
//!       cargo run -p fresco-vulkan --example headless_icd
//!
//! Level 1 exercises NO compute/instancing/bundle — just the device
//! + render-pass-clear + readback.  Exit 0 = the core path works on
//! the selected ICD.

use fresco_vulkan::{HeadlessRenderer, SceneNode};
use std::path::PathBuf;

const W: u32 = 256;
const H: u32 = 256;

fn main() -> std::process::ExitCode {
    println!("P4 level-1: HeadlessRenderer::new (enumerates the ICD's device)...");
    let mut r = match HeadlessRenderer::new(W, H) {
        Ok(r) => { println!("  HeadlessRenderer::new OK ({W}x{H})"); r }
        Err(e) => { eprintln!("  FAIL HeadlessRenderer::new: {e:#}"); return 1.into(); }
    };

    // Clear to an opaque colour (passed R,G,B,A; the renderer maps to
    // the BGRA8 attachment internally) and read it back.
    let clear = [40u8, 80, 160, 255];
    if let Err(e) = r.clear_and_readback(clear) {
        eprintln!("  FAIL clear_and_readback: {e:#}");
        return 1.into();
    }
    let px = match r.read_pixels_vec() {
        Ok(p) => p,
        Err(e) => { eprintln!("  FAIL read_pixels_vec: {e:#}"); return 1.into(); }
    };

    let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
    let center = [px[i], px[i + 1], px[i + 2], px[i + 3]];
    println!("  center pixel (BGRA bytes) = {center:?}");
    // Bar for level 1: the clear actually painted (non-zero, opaque) —
    // exact bytes vary with the sRGB attachment encoding.
    let painted = center != [0, 0, 0, 0] && center[3] == 255;
    if !painted {
        eprintln!("FAIL: center pixel not painted ({center:?})");
        return 1.into();
    }
    println!("PASS level-1: HeadlessRenderer cleared + read back through the ICD");

    // ── Level 2: real pipeline — load the atrium-core bundle (its
    // rect/path/texture compute + graphics shaders must compile on
    // tier2), draw a rect node (compute builds the instance, then
    // instanced draw whose VS reads the instance SSBO), read back. ──
    let bundle = std::env::var("BUNDLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(
            "/Users/girivs/src/bsd/bundles/atrium-core"));
    println!("\nP4 level-2: load_bundle({}) ...", bundle.display());
    if !bundle.join("manifest.json").exists() {
        eprintln!("  SKIP level-2: bundle not found at {}", bundle.display());
        return 0.into();
    }
    if let Err(e) = r.load_bundle(&bundle) {
        eprintln!("  GAP load_bundle: {e:#}");
        eprintln!("  (the bundle's compute/graphics shaders don't yet \
                   compile/run on tier2 — the level-2 parity work)");
        return 2.into();
    }
    println!("  load_bundle OK — {} op pipeline(s) compiled on tier2", r.op_count());

    // One opaque red rect in the lower-right quadrant.
    r.set_rect_nodes(vec![SceneNode {
        position: [128.0, 128.0],
        size:     [96.0, 96.0],
        color:    [1.0, 0.0, 0.0, 1.0],
    }]);
    if let Err(e) = r.render_to_buffer() {
        eprintln!("  GAP render_to_buffer: {e:#}");
        return 2.into();
    }
    let px2 = r.read_pixels_vec().expect("read2");
    let j = (170 * W as usize + 170) * 4; // inside the rect
    let rect_px = [px2[j], px2[j + 1], px2[j + 2], px2[j + 3]];
    println!("  rect pixel (BGRA) @ (170,170) = {rect_px:?}");
    // Red rect → BGRA ~ [0,0,255,255]; bar = a red-dominant opaque px.
    if rect_px[2] > 128 && rect_px[3] == 255 && rect_px[0] < 64 {
        println!("PASS level-2: compositor rect rendered through tier2 \
                  (compute + instanced draw)");
        0.into()
    } else {
        eprintln!("GAP: rect not rendered as expected ({rect_px:?}) — \
                   compute/instancing parity to triage");
        2.into()
    }
}
