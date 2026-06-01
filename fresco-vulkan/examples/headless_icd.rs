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

use fresco_vulkan::HeadlessRenderer;

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
    if painted {
        println!("PASS: HeadlessRenderer cleared + read back through the ICD");
        0.into()
    } else {
        eprintln!("FAIL: center pixel not painted ({center:?})");
        1.into()
    }
}
