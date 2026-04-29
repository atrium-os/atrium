// Build script for tessera-sys: locate libtessera_core and emit link flags.
//
// Phase 0 only links if TESSERA_CORE_LIB is set in the environment;
// otherwise the crate compiles as a pure-rust stub so `cargo check` works
// before the C library is built.

use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=TESSERA_CORE_LIB");
    println!("cargo:rerun-if-env-changed=TESSERA_CORE_INCLUDE");

    if let Ok(libdir) = env::var("TESSERA_CORE_LIB") {
        println!("cargo:rustc-link-search=native={libdir}");
        println!("cargo:rustc-link-lib=static=tessera_core");
        // libmd provides SHA-256 on FreeBSD.
        if cfg!(target_os = "freebsd") {
            println!("cargo:rustc-link-lib=md");
        }
    }
}
