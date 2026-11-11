//! VGPRs of the RDNA4 kernels: `cargo run --features rdna4 --example rdna4_regs`
//! (ATRIUM_RDNA4_PRESSURE=1 also prints the peak live VGPRs).
fn main() {
    let r = fresco_compute::rdna4::Rdna4::new().expect("compile");
    println!("bin vgprs {} raster vgprs {} scratch {}", r.bin.bin.vgprs, r.raster.bin.vgprs, r.raster.bin.scratch_dwords);
}
