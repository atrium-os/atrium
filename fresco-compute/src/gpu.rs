//! What the GPU renderer needs from a GPU architecture, and nothing else.
//!
//! The renderer itself (damage, window surfaces, compose, staging, fences:
//! frescod's `GpuCompute`) is the same on every GPU. What differs per
//! architecture is the machine code for the three kernels (bin, raster,
//! compose) and the command stream that copies memory, dispatches them and
//! signals the end of a frame. That is [`Arch`] and [`Stream`]. RDNA4 is
//! the first ([`crate::rdna4::Rdna4Arch`]); an RDNA3 or Intel Xe backend
//! is another implementation of the same two traits, plus an entry in
//! [`by_name`] and [`for_device`].
//!
//! Addresses are GPU virtual addresses in the caller's address space; the
//! caller allocates, uploads and submits (frescod on the `'A'` ABI, which is
//! vendor-neutral).

use crate::compose::{self, Compose};
use crate::{binding, Frame};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// One GPU architecture's kernels and command encoding.
pub trait Arch: Send {
    /// Short name, as `FRESCOD_GPU_ARCH` takes it (`rdna4`).
    fn name(&self) -> &'static str;

    /// The three kernels' code, laid out as one blob to upload once at a
    /// [`Arch::code_align`]-aligned address.
    fn code(&self) -> Vec<u8>;

    /// Offsets of bin, raster and compose in [`Arch::code`].
    fn code_offsets(&self) -> [u64; 3];

    /// Alignment the code's upload address needs.
    fn code_align(&self) -> u64;

    /// A new, empty command stream for one frame.
    fn stream(&self) -> Box<dyn Stream + '_>;
}

/// One frame's commands, recorded in order.
pub trait Stream {
    /// Copy `len` bytes from `src` to `dst`; later commands see the copy.
    fn copy(&mut self, src: u64, dst: u64, len: u64);

    /// Bin `f`, then raster the tile rectangles `rects`
    /// (`[tx0, ty0, tx1, ty1)`, see [`crate::damage::rects`]). `code` is
    /// where [`Arch::code`] was uploaded; `buffers` is each binding's
    /// address by [`binding`] number. The frame's mask must be zero.
    fn frame(&mut self, code: u64, buffers: &[u64; binding::COUNT], f: &Frame, rects: &[[u32; 4]]) -> Result<(), Error>;

    /// Compose the window surfaces over the screen rects `rects` in pixels
    /// (`[x0, y0, x1, y1)`), or the whole target if `rects` is empty.
    /// `buffers` is by [`compose::binding`] number.
    fn compose(&mut self, code: u64, buffers: &[u64; compose::binding::COUNT], c: &Compose, rects: &[[u32; 4]]) -> Result<(), Error>;

    /// Once everything before it is done, write `1` to `fence` and raise
    /// the interrupt that signals the submission's syncobj.
    fn end(&mut self, fence: u64);

    /// The encoded stream, to place in the ring and submit.
    fn bytes(&self) -> Vec<u8>;

    /// Its length in the units the submit call counts (dwords on AMD).
    fn len_units(&self) -> u32;
}

/// The architectures built in, by name.
pub fn names() -> &'static [&'static str] {
    &[
        #[cfg(feature = "rdna4")]
        "rdna4",
    ]
}

/// Compile the kernels for the architecture `name`.
pub fn by_name(name: &str) -> Result<Box<dyn Arch>, Error> {
    match name {
        #[cfg(feature = "rdna4")]
        "rdna4" => Ok(Box::new(crate::rdna4::Rdna4Arch::new()?)),
        other => Err(format!("GPU architecture {other:?} is not built in (have: {})", names().join(", ")).into()),
    }
}

/// The architecture a device is, from what the driver reports about
/// itself. The `'A'` ABI has no ISA field yet, so this reads the vendor
/// string; `None` when nothing built in matches (frescod then needs
/// `FRESCOD_GPU_ARCH`).
pub fn for_device(vendor: &str) -> Option<&'static str> {
    let v = vendor.to_ascii_lowercase();
    names().iter().copied().find(|n| v.contains(n))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_vendor_string_names_its_architecture() {
        #[cfg(feature = "rdna4")]
        assert_eq!(super::for_device("Atrium AMD RDNA4 (gpusim)"), Some("rdna4"));
        assert_eq!(super::for_device("Intel Xe2"), None);
        assert!(super::by_name("xe2").is_err());
    }
}
