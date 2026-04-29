// Link against libfresco.a and its sole base-system dep, libmd.
//
// libfresco is built out-of-tree (../libfresco/). Cargo doesn't track
// its source files, so re-running `bmake` over there before
// `cargo build` here is the user's job.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let libfresco = manifest_dir.parent().unwrap().join("libfresco");

    println!("cargo:rerun-if-changed={}/libfresco.a", libfresco.display());
    println!("cargo:rerun-if-changed={}/include/fresco.h", libfresco.display());

    println!("cargo:rustc-link-search=native={}", libfresco.display());
    println!("cargo:rustc-link-lib=static=fresco");
    println!("cargo:rustc-link-lib=md");
}
