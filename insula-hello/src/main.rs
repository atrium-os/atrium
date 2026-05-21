//! insula-hello — minimum-viable Insula app.
//!
//! Demonstrates the v0 lifecycle:
//!
//!   1. `atrium_init(sdk_major, sdk_minor)` — handshake.
//!   2. `atrium_log(INFO, "hello, …")` — emit something.
//!   3. `atrium_exit(0)` — clean termination.
//!
//! All three calls are defined as `extern "C"` /
//! `#[no_mangle]` in libatrium — the same surface a
//! C / Zig / Swift / Go app would import via the
//! generated `atrium.h` header (see `libatrium/include/
//! atrium.h`).
//!
//! For v0 this Rust app links libatrium as a path
//! crate (rlib). In a real Insula deployment, the
//! binary dynamically loads `libatrium.dylib` (cdylib)
//! from the platform's install location. Either way
//! the surface is the same.

use atrium::{atrium_exit, atrium_init, atrium_log, ATRIUM_LOG_INFO};
use std::ffi::CString;

fn main() {
    let status = atrium_init(1, 0);
    if status != atrium::ATRIUM_OK {
        eprintln!("atrium_init failed: {}", status);
        std::process::exit(1);
    }

    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let msg = CString::new(format!(
        "hello from Insula on {}-{} (insula-hello v{})",
        arch,
        os,
        env!("CARGO_PKG_VERSION"),
    ))
    .expect("the format string contains no NULs");

    // SAFETY: `msg.as_ptr()` is a valid NUL-terminated
    // C string for the duration of this call.
    unsafe {
        atrium_log(ATRIUM_LOG_INFO, msg.as_ptr());
    }

    atrium_exit(0);
}
