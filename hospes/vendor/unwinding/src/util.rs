use gimli::{EndianSlice, NativeEndian, Pointer};

pub type StaticSlice = EndianSlice<'static, NativeEndian>;

pub unsafe fn get_unlimited_slice<'a>(start: *const u8) -> &'a [u8] {
    // Create the largest possible slice for this address.
    let start = start as usize;
    let end = start.saturating_add(isize::MAX as _);
    let len = end - start;
    unsafe { core::slice::from_raw_parts(start as *const _, len) }
}

pub unsafe fn deref_pointer(ptr: Pointer) -> usize {
    match ptr {
        Pointer::Direct(x) => x as _,
        Pointer::Indirect(x) => unsafe { *(x as *const _) },
    }
}

#[cfg(feature = "libc")]
pub use libc::c_int;

#[cfg(not(feature = "libc"))]
#[allow(non_camel_case_types)]
pub type c_int = i32;

#[cfg(all(
    any(feature = "panic", feature = "panic-handler-dummy"),
    feature = "libc"
))]
pub fn abort() -> ! {
    unsafe { libc::abort() };
}

#[cfg(all(
    any(feature = "panic", feature = "panic-handler-dummy"),
    not(feature = "libc")
))]
pub fn abort() -> ! {
    core::intrinsics::abort_immediate();
}

/// Atrium patch: an `.eh_frame` section with the machine's CFI vendor set, so AArch64's
/// `DW_CFA_AARCH64_negate_ra_state` (0x2d) parses instead of failing as SPARC's
/// `DW_CFA_GNU_window_save`, which aborted every unwind through pac-ret code.
pub fn eh_frame(section: &'static [u8]) -> gimli::EhFrame<StaticSlice> {
    #[allow(unused_mut)]
    let mut f = gimli::EhFrame::new(section, gimli::NativeEndian);
    #[cfg(target_arch = "aarch64")]
    f.set_vendor(gimli::Vendor::AArch64);
    f
}
