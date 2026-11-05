//! gpu_reset — trigger one coordinated device reset (FLR) through the root-only
//! control node, for recovery tests: e.g. run frescod, then this, and check
//! the screen comes back (GPU ABI v2.1 §10.3, NOTE_RESET).
use atrium_gpu::amd::{gpu_reset, GpuCtl};

fn main() {
    if let Err(e) = GpuCtl::open().and_then(|c| gpu_reset(&c)) {
        eprintln!("gpu_reset: {e}");
        std::process::exit(1);
    }
    println!("gpu_reset: device reset done");
}
