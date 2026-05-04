//! fresco-server bin entry point.
//!
//! On macOS this dispatches to the winit + Metal + ivshmem implementation
//! in `main_macos.rs`. On other platforms (FreeBSD-native) the bin is a
//! stub — the FreeBSD-native server lives in the `atrium-compositor`
//! crate, which links the `fresco-server` library directly.

#[cfg(target_os = "macos")]
#[path = "main_macos.rs"]
mod macos_impl;

fn main() {
    #[cfg(target_os = "macos")]
    {
        macos_impl::run();
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "fresco-server bin is macOS-only. \
             FreeBSD-native server lives in the atrium-compositor crate."
        );
        std::process::exit(2);
    }
}
