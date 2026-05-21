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

use atrium::{
    atrium_container_path, atrium_exit, atrium_init, atrium_log,
    atrium_storage_open, ATRIUM_LOG_INFO, ATRIUM_STORAGE_WRITE,
};
use std::ffi::CString;
use std::io::Write;

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

    // Demonstrate storage: log the container path (if
    // any), then write a small marker file inside it.
    // Quietly skip both steps if we're not running
    // inside a host-provisioned container.
    let mut buf = [0u8; 4096];
    let n = unsafe {
        atrium_container_path(buf.as_mut_ptr() as *mut _, buf.len())
    };
    if n > 0 {
        let path = std::str::from_utf8(&buf[..n as usize])
            .unwrap_or("<non-utf8>");
        let path_log = CString::new(format!(
            "container is {}",
            path
        ))
        .unwrap();
        unsafe { atrium_log(ATRIUM_LOG_INFO, path_log.as_ptr()); }

        let file_path = CString::new("hello-from-insula.txt").unwrap();
        let fd = unsafe {
            atrium_storage_open(file_path.as_ptr(), ATRIUM_STORAGE_WRITE)
        };
        if fd >= 0 {
            use std::os::fd::FromRawFd;
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            match writeln!(f, "Hello from insula-hello at {:?}",
                              std::time::SystemTime::now()) {
                Ok(()) => {
                    let ok = CString::new("wrote hello-from-insula.txt").unwrap();
                    unsafe { atrium_log(ATRIUM_LOG_INFO, ok.as_ptr()); }
                }
                Err(e) => {
                    let err = CString::new(format!(
                        "writeln failed: {}", e
                    )).unwrap();
                    unsafe { atrium_log(2, err.as_ptr()); }
                }
            }
        } else {
            let err = CString::new(format!(
                "atrium_storage_open returned {}", fd
            )).unwrap();
            unsafe { atrium_log(2, err.as_ptr()); }
        }
    }

    atrium_exit(0);
}
