//! sizes: compiled size and register use of the desktop and Fresco
//! compute shaders. MAX_VGPRS=n caps the VGPR budget (forces spills).
fn report(n: &str, bytes: &[u8]) {
    let m = atrium_spv_frontend::translate_with_options(bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    atrium_gir::passes::optimize(&mut g);
    let cap = std::env::var("MAX_VGPRS").ok().and_then(|v| v.parse().ok()).unwrap_or(256);
    match atrium_rdna4_compiler::compile_with(&g, &atrium_rdna4_compiler::Options { max_vgprs: cap }) {
        Ok(b) => println!("{n:<14} {:4} dwords {:3} vgprs {:3} scratch dwords", b.code.len(), b.vgprs, b.scratch_dwords),
        Err(e) => println!("{n:<14} {e:?}"),
    }
}

fn main() {
    for (n, p, _) in atrium_rdna4_oracle::cases::DESKTOP {
        report(n, &std::fs::read(format!("{}/../bundles/{p}", env!("CARGO_MANIFEST_DIR"))).unwrap());
    }
    for n in ["bin", "raster"] {
        report(&format!("fresco_{n}"), &std::fs::read(format!("{}/../fresco-compute/shaders/{n}.comp.spv", env!("CARGO_MANIFEST_DIR"))).unwrap());
    }
}
