//! End-to-end driver for the `atrium-spv-loader` cache path,
//! used by the in-VM verification script.
//!
//! Proves the loader's *disk* cache works on the production
//! target, not just its in-memory `HashMap`:
//!
//!   1. A first `ShaderCache` loads the SPIR-V — a cold
//!      miss: it spawns `atrium-spv-compile`, which writes
//!      `<hash>.so` + `<hash>.pcmap` into the cache dir.
//!   2. A *second*, independent `ShaderCache` (empty
//!      in-memory state) loads the same SPIR-V — but its
//!      `compile_binary` is pointed at a path that does not
//!      exist. If the loader tried to spawn the compiler it
//!      would fail; success therefore proves the load was
//!      served purely from the on-disk cache, with no
//!      re-spawn.
//!   3. The entry point resolved by that second, disk-cache
//!      load is invoked per the AAPCS64 fragment ABI and the
//!      resulting RGBA printed — proving the dlopen'd handle
//!      still renders correctly.
//!
//! Usage:
//!   loader_e2e_driver <spirv> <compile-bin> <cache-root> \
//!                     [push-const] [int]
//!
//! The optional push-const mirrors `verify/harness.c`: a
//! bare value is an f32 in `push_constants[0..4]`; the
//! trailing literal `int` switches it to an i32.
//!
//! stdout: `r g b a` on success. Exit non-zero on any
//! loader error (including a spurious re-spawn attempt).

use std::path::PathBuf;

use atrium_spv_loader::{LoaderConfig, ShaderCache};

fn main() {
    let mut args = std::env::args().skip(1);
    let spirv_path = args.next().expect("arg1: spirv path");
    let compile_bin = args.next().expect("arg2: atrium-spv-compile path");
    let cache_root = args.next().expect("arg3: cache root dir");
    let pc_arg = args.next();
    let pc_is_int = args.next().as_deref() == Some("int");

    let spirv = std::fs::read(&spirv_path)
        .unwrap_or_else(|e| panic!("reading {spirv_path}: {e}"));

    // 1. Cold load: real compile binary, empty cache dir →
    //    spawns atrium-spv-compile.
    let cache1 = ShaderCache::new(LoaderConfig {
        cache_root: PathBuf::from(&cache_root),
        abi_version: 1,
        compile_binary: PathBuf::from(&compile_bin),
    });
    let _shader1 = cache1.load_or_compile(&spirv)
        .expect("cold load (compile) failed");

    // 2. Disk-cache load: a *fresh* ShaderCache (no in-memory
    //    state) whose compile binary is intentionally bogus.
    //    A cache miss here would try to spawn it and fail —
    //    so reaching a LoadedShader proves the on-disk .so
    //    was reused with no re-compile.
    let cache2 = ShaderCache::new(LoaderConfig {
        cache_root: PathBuf::from(&cache_root),
        abi_version: 1,
        compile_binary: PathBuf::from(
            "/nonexistent/atrium-spv-compile-must-not-be-spawned",
        ),
    });
    let shader2 = cache2.load_or_compile(&spirv)
        .expect("disk-cache load failed — loader tried to re-spawn the \
                 compiler instead of reusing the cached .so");

    let fs_main = shader2.entry_points.fs_main
        .expect("shader exports no atrium_fs_main");

    // Build the optional 16-byte push-constant buffer.
    let mut pc = [0u8; 16];
    let pc_ptr: *const u8 = match &pc_arg {
        Some(v) => {
            if pc_is_int {
                let iv: i32 = v.parse().expect("push-const int parse");
                pc[..4].copy_from_slice(&iv.to_le_bytes());
            } else {
                let fv: f32 = v.parse().expect("push-const f32 parse");
                pc[..4].copy_from_slice(&fv.to_le_bytes());
            }
            pc.as_ptr()
        }
        None => std::ptr::null(),
    };

    let mut out = [0f32; 4];
    let mut depth = 0f32;
    // SAFETY: fs_main is the AAPCS64 fragment entry point
    // resolved from a shader atrium-spv-compile produced;
    // the argument shapes match docs/spec §4.1.
    unsafe {
        fs_main(
            std::ptr::null(), // in_varyings
            std::ptr::null(), // uniforms
            pc_ptr,           // push_constants
            0.0, 0.0, 0.0, 0.0, // frag_coord x/y/z/w
            0,                // samples_mask
            out.as_mut_ptr(),
            &mut depth as *mut f32,
        );
    }

    println!("{} {} {} {}", out[0], out[1], out[2], out[3]);
}
