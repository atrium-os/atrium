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
    atrium_net_connect, atrium_storage_open, ATRIUM_LOG_INFO,
    ATRIUM_NET_TCP, ATRIUM_STORAGE_WRITE,
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

    // Optional network call — gated on
    // ATRIUM_NET_TEST_HOST=host:port. Used by the
    // per-app netd enforcement integration test; in
    // normal demo runs this env var is unset and the
    // block is skipped.
    if let Ok(host_port) = std::env::var("ATRIUM_NET_TEST_HOST") {
        if let Some((host, port_str)) = host_port.split_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                let host_c = CString::new(host).unwrap();
                let fd = unsafe {
                    atrium_net_connect(host_c.as_ptr(), port, ATRIUM_NET_TCP)
                };
                let line = if fd >= 0 {
                    unsafe { libc::close(fd); }
                    format!("net-connect OK to {}:{} (fd was {})", host, port, fd)
                } else {
                    format!("net-connect FAIL to {}:{} (code {})", host, port, fd)
                };
                let cstr = CString::new(line).unwrap();
                unsafe { atrium_log(ATRIUM_LOG_INFO, cstr.as_ptr()); }
            }
        }
    }

    atrium_exit(0);
}
