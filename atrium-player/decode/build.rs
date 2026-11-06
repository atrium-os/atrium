// Compile the C shims against the system's dav1d, libvpx, libopus and
// libvorbis (FreeBSD: multimedia/dav1d, multimedia/libvpx, audio/opus,
// audio/libvorbis; Linux: their -dev packages).
fn main() {
    let mut cc = cc::Build::new();
    cc.warnings(true).extra_warnings(true).flag_if_supported("-Wno-unused-parameter").flag_if_supported("-fstack-protector-strong");
    for lib in ["dav1d", "vpx", "opus", "vorbis", "ogg"] {
        let l = pkg_config::Config::new().probe(lib).unwrap_or_else(|e| panic!("{lib}: {e}"));
        for p in l.include_paths {
            cc.include(p);
        }
    }
    for f in ["dav1d", "vpx", "opus", "vorbis"] {
        cc.file(format!("shim/{f}.c"));
        println!("cargo:rerun-if-changed=shim/{f}.c");
    }
    println!("cargo:rerun-if-changed=shim/shim.h");
    cc.compile("atrium_media_shim");
}
