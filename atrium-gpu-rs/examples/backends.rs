//! `examples/backends` — print the kmod's backend list via
//! `IOC_GPU_LIST_BACKENDS`.
//!
//! Today the kmod has exactly one backend (`atrium-gpu-v1` over
//! virtio-gpu). In future this is what atrium-pkg's
//! shader-precompile pass enumerates at install time so it can
//! compile shaders against every locally-installed target.
//!
//! Run inside the FreeBSD VM:
//!
//! ```sh
//! /mnt/host/atrium-gpu-rs/target/aarch64-unknown-freebsd/release/examples/backends
//! ```

use atrium_gpu::Gpu;

fn main() -> std::io::Result<()> {
    let gpu = Gpu::open()?;
    let backends = gpu.backends()?;
    println!("kmod reports {} backend(s):", backends.len());
    for (i, b) in backends.iter().enumerate() {
        println!(
            "  [{i}] vendor=0x{:04x} gen={} name={:?} features=0x{:x}",
            b.vendor_id, b.generation_id, b.name, b.feature_flags,
        );
    }
    Ok(())
}
