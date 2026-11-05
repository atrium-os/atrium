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

/// `compose.comp` for RDNA4: the broker's pass that stacks window surfaces
/// into the scanout buffer ([`crate::compose`]).
pub struct Compose {
    pub kernel: Kernel,
}

impl Compose {
    pub fn new() -> Result<Self, Error> {
        let kernel = Kernel::from_spirv(crate::compose::COMPOSE_SPV, &Options::default())?;
        if kernel.uses_push_constants() {
            return Err(Error::Binding("compose: unexpected push constants".into()));
        }
        Ok(Compose { kernel })
    }

    /// The code, to upload at a [`CODE_ALIGN`]-aligned address.
    pub fn code(&self) -> Vec<u8> {
        self.kernel.code()
    }

    /// Append the pass to `ib`, over the screen rects `rects` in pixels
    /// (`[x0, y0, x1, y1)`, widened to whole workgroups), or the whole
    /// target if `rects` is empty: make earlier writes (the surfaces'
    /// rasters) visible, compose, wait. `buffers` is each binding's address
    /// by [`crate::compose::binding`] number.
    pub fn record(
        &self,
        ib: &mut Blob,
        code_va: u64,
        buffers: &[u64; crate::compose::binding::COUNT],
        c: &crate::compose::Compose,
        rects: &[[u32; 4]],
    ) -> Result<(), Error> {
        use crate::compose::COMPOSE_LOCAL as L;
        let bind: Vec<((u32, u32), u64)> = buffers.iter().enumerate().map(|(b, &va)| ((0, b as u32), va)).collect();
        let [gx, gy, _] = c.groups();
        ib.acquire_all();
        if rects.is_empty() {
            self.kernel.record_at(ib, code_va, &bind, None, [0, 0, 0], [gx, gy, 1])?;
        }
        for r in rects {
            let (x0, y0) = (r[0] / L, r[1] / L);
            let (x1, y1) = (r[2].div_ceil(L).min(gx), r[3].div_ceil(L).min(gy));
            if x1 > x0 && y1 > y0 {
                self.kernel.record_at(ib, code_va, &bind, None, [x0, y0, 0], [x1 - x0, y1 - y0, 1])?;
            }
        }
        ib.cs_partial_flush();
        Ok(())
    }
}

/// RDNA4 as a [`crate::gpu::Arch`]: the three kernels, and PM4 for the
/// copies and the end-of-frame fence.
pub struct Rdna4Arch {
    k: Rdna4,
    c: Compose,
    offsets: [u64; 3],
}

impl Rdna4Arch {
    pub fn new() -> Result<Self, Error> {
        let (k, c) = (Rdna4::new()?, Compose::new()?);
        let (raster_off, end) = k.code_layout();
        let offsets = [0, raster_off, end.next_multiple_of(CODE_ALIGN)];
        Ok(Rdna4Arch { k, c, offsets })
    }
}

/// Bytes per CP DMA copy: well under GFX12 DMA_DATA's 26-bit byte count.
const DMA_CHUNK: u64 = 1 << 20;
/// PM4 DMA_DATA (CP copy, memory to memory) and RELEASE_MEM.
const IT_DMA_DATA: u32 = 0x50;
const IT_RELEASE_MEM: u32 = 0x49;

impl crate::gpu::Arch for Rdna4Arch {
    fn name(&self) -> &'static str {
        "rdna4"
    }

    fn code(&self) -> Vec<u8> {
        let [bin, raster] = self.k.code();
        let mut code = bin;
        code.resize(self.offsets[1] as usize, 0);
        code.extend(raster);
        code.resize(self.offsets[2] as usize, 0);
        code.extend(self.c.code());
        code
    }

    fn code_offsets(&self) -> [u64; 3] {
        self.offsets
    }

    fn code_align(&self) -> u64 {
        CODE_ALIGN
    }

    fn stream(&self) -> Box<dyn crate::gpu::Stream + '_> {
        Box::new(Rdna4Stream { arch: self, ib: Blob::compute() })
    }
}

struct Rdna4Stream<'a> {
    arch: &'a Rdna4Arch,
    ib: Blob,
}

impl crate::gpu::Stream for Rdna4Stream<'_> {
    fn copy(&mut self, src: u64, dst: u64, len: u64) {
        let mut done = 0;
        while done < len {
            let n = (len - done).min(DMA_CHUNK);
            let (s, d) = (src + done, dst + done);
            // CP_SYNC (bit 31): later packets wait for the copy.
            self.ib.packet(IT_DMA_DATA, &[1 << 31, s as u32, (s >> 32) as u32, d as u32, (d >> 32) as u32, n as u32]);
            done += n;
        }
    }

    fn frame(&mut self, code: u64, buffers: &[u64; binding::COUNT], f: &Frame, rects: &[[u32; 4]]) -> Result<(), crate::gpu::Error> {
        let o = self.arch.offsets;
        let a = Addrs { code: [code + o[0], code + o[1]], buffers: *buffers };
        Ok(self.arch.k.record_tiles(&mut self.ib, &a, f, rects)?)
    }

    fn compose(&mut self, code: u64, buffers: &[u64; crate::compose::binding::COUNT], c: &crate::compose::Compose, rects: &[[u32; 4]]) -> Result<(), crate::gpu::Error> {
        Ok(self.arch.c.record(&mut self.ib, code + self.arch.offsets[2], buffers, c, rects)?)
    }

    fn end(&mut self, fence: u64) {
        // End of pipe: write the fence, then interrupt.
        self.ib.packet(IT_RELEASE_MEM, &[5 << 8, (2 << 29) | (2 << 24), fence as u32, (fence >> 32) as u32, 1, 0]);
    }

    fn bytes(&self) -> Vec<u8> {
        self.ib.bytes()
    }

    fn len_units(&self) -> u32 {
        self.ib.dw.len() as u32
    }
}
