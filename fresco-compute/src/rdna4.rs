//! Run the two kernels on RDNA4: compiled by `atrium-rdna4`, recorded as one
//! PM4 blob per frame.
//!
//! Submission and memory are the caller's (frescod uses atrium-gpu-rs on
//! `'A'`). The caller uploads [`Rdna4::code`] once, places every binding's
//! bytes from [`Frame::buffers`] in GPU memory (the target can be the
//! scanout buffer itself), zeroes the mask, and submits what
//! [`Rdna4::record`] appends.

use atrium_rdna4::{Blob, Error, Kernel, Options, CODE_ALIGN};

use crate::{binding, Frame, BIN_SPV, RASTER_SPV};

/// The two compiled kernels.
pub struct Rdna4 {
    /// `bin.comp`.
    pub bin: Kernel,
    /// `raster.comp`.
    pub raster: Kernel,
}

/// GPU addresses of the code and the buffers of one frame.
#[derive(Debug, Clone, Copy)]
pub struct Addrs {
    /// Where [`Rdna4::code`]'s bin and raster code were uploaded
    /// ([`CODE_ALIGN`]-aligned).
    pub code: [u64; 2],
    /// Each binding's address, indexed by [`binding`] number.
    pub buffers: [u64; binding::COUNT],
}

impl Rdna4 {
    /// Compile both kernels.
    pub fn new() -> Result<Self, Error> {
        let o = Options::default();
        let r = Rdna4 { bin: Kernel::from_spirv(BIN_SPV, &o)?, raster: Kernel::from_spirv(RASTER_SPV, &o)? };
        for k in [&r.bin, &r.raster] {
            if k.uses_push_constants() {
                return Err(Error::Binding(format!("{}: unexpected push constants", k.name)));
            }
        }
        Ok(r)
    }

    /// The bin and raster code, each to upload at a [`CODE_ALIGN`]-aligned
    /// address.
    pub fn code(&self) -> [Vec<u8>; 2] {
        [self.bin.code(), self.raster.code()]
    }

    /// Bytes to reserve for the code: raster's offset is `code_len()[0]`
    /// rounded up to [`CODE_ALIGN`].
    pub fn code_layout(&self) -> (u64, u64) {
        let raster_off = (self.bin.code().len() as u64).next_multiple_of(CODE_ALIGN);
        (raster_off, raster_off + self.raster.code().len() as u64)
    }

    /// Append `f`'s two dispatches to `ib`: make earlier writes visible, bin,
    /// wait for it and make the masks visible, raster. The mask must be
    /// zero when the IB starts.
    pub fn record(&self, ib: &mut Blob, a: &Addrs, f: &Frame) -> Result<(), Error> {
        let bind: Vec<((u32, u32), u64)> =
            a.buffers.iter().enumerate().map(|(b, &va)| ((0, b as u32), va)).collect();
        ib.acquire_all();
        self.bin.record(ib, a.code[0], &bind, None, f.bin_groups())?;
        ib.cs_partial_flush();
        ib.acquire_all();
        self.raster.record(ib, a.code[1], &bind, None, f.raster_groups())?;
        ib.cs_partial_flush();
        Ok(())
    }
}
