//! Run the two kernels on RDNA4: compiled by `atrium-rdna4`, recorded as one
//! PM4 blob per frame.
//!
//! Submission and memory are the caller's (frescod uses atrium-gpu-rs on
//! `'A'`). The caller uploads [`Rdna4::code`] once, places every binding's
//! bytes from [`Frame::buffers`] in GPU memory (the target can be the
//! scanout buffer itself), zeroes the mask, and submits what
//! [`Rdna4::record`] appends.

use atrium_rdna4::{Blob, Error, Kernel, Options, CODE_ALIGN};

use crate::{binding, Frame, BIN_SPV, RASTER_LOCAL, RASTER_SPV, TILE};

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
        let h = &f.header;
        self.record_tiles(ib, a, f, &[[0, 0, h.tiles_x, h.tiles_y]])
    }

    /// Like [`Rdna4::record`], rastering only the tile rectangles `rects`
    /// (`[tx0, ty0, tx1, ty1)`, see [`crate::damage::rects`]); the rest of
    /// the target keeps what it held. Each rectangle is one raster dispatch
    /// over its workgroups (COMPUTE_START), so the kernel is unchanged.
    pub fn record_tiles(&self, ib: &mut Blob, a: &Addrs, f: &Frame, rects: &[[u32; 4]]) -> Result<(), Error> {
        if rects.is_empty() {
            return Ok(());
        }
        let bind: Vec<((u32, u32), u64)> =
            a.buffers.iter().enumerate().map(|(b, &va)| ((0, b as u32), va)).collect();
        ib.acquire_all();
        self.bin.record(ib, a.code[0], &bind, None, f.bin_groups())?;
        ib.cs_partial_flush();
        ib.acquire_all();
        let per = TILE / RASTER_LOCAL; // raster workgroups per tile edge
        let [gx, gy, _] = f.raster_groups();
        for r in rects {
            let (x0, y0) = (r[0] * per, r[1] * per);
            let (x1, y1) = ((r[2] * per).min(gx), (r[3] * per).min(gy));
            if x1 > x0 && y1 > y0 {
                self.raster.record_at(ib, a.code[1], &bind, None, [x0, y0, 0], [x1 - x0, y1 - y0, 1])?;
            }
        }
        ib.cs_partial_flush();
        Ok(())
    }
}
