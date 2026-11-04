//! atrium-rdna4-cmd — GFX12 command blobs.
//!
//! Builds the opaque blob that GPU ABI v2.1 §5.7 executes by reference (and
//! that the R9700 oracle runner submits through the stock amdgpu driver).
//! Register offsets and fields come from `regs.rs`, generated from Linux's
//! MIT-licensed GFX12 headers (`spec/`). This crate never hard-codes an offset.
//!
//! Scope today: PM4 type-3 packets, a single compute dispatch, and GPU
//! timestamps.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod regs;
use regs::Reg;

/// Dword offset of the GC register segment 0 in the MMIO map. Taken from the
/// IP-discovery table of GC 12.0.x parts; the R9700 runner reads the live value
/// from sysfs and records it with every result, so a mismatch is caught.
pub const GC_BASE_SEG0: u32 = 0x1260;
/// First dword of the SH (persistent shader state) register window that
/// `SET_SH_REG` offsets are relative to.
pub const SH_REG_START: u32 = 0x2c00;

/// PM4 type-3 opcodes used here.
pub mod op {
    /// No-op / padding.
    pub const NOP: u32 = 0x10;
    /// Launch a compute grid.
    pub const DISPATCH_DIRECT: u32 = 0x15;
    /// Write SH registers.
    pub const SET_SH_REG: u32 = 0x76;
    /// End-of-pipe event with a memory write.
    pub const RELEASE_MEM: u32 = 0x49;
    /// Wait for and invalidate / write back caches.
    pub const ACQUIRE_MEM: u32 = 0x58;
}

/// Absolute dword address of a GC register.
pub fn abs(r: Reg) -> u32 {
    assert_eq!(r.base_idx, 0, "only GC segment 0 is mapped so far");
    GC_BASE_SEG0 + r.offset
}

/// A command blob under construction.
#[derive(Default, Debug, Clone)]
pub struct Blob {
    /// The dwords.
    pub dw: Vec<u32>,
    compute: bool,
}

impl Blob {
    /// A blob for a compute queue (sets the PM4 shader-type bit).
    pub fn compute() -> Self {
        Blob { dw: Vec::new(), compute: true }
    }

    /// Type-3 header for `opcode` with `body` dwords following.
    pub fn header(&self, opcode: u32, body: usize) -> u32 {
        assert!((1..=0x3fff).contains(&body));
        (3 << 30) | (((body as u32) - 1) << 16) | (opcode << 8) | if self.compute { 1 << 1 } else { 0 }
    }

    /// Append a packet.
    pub fn packet(&mut self, opcode: u32, body: &[u32]) -> &mut Self {
        let h = self.header(opcode, body.len());
        self.dw.push(h);
        self.dw.extend_from_slice(body);
        self
    }

    /// Write consecutive SH registers starting at `first`.
    pub fn set_sh(&mut self, first: Reg, values: &[u32]) -> &mut Self {
        let a = abs(first);
        assert!(a >= SH_REG_START && a < SH_REG_START + 0x400, "{first:?} is not an SH register");
        let mut body = vec![a - SH_REG_START];
        body.extend_from_slice(values);
        self.packet(op::SET_SH_REG, &body)
    }

    /// Pad with NOPs to a multiple of `align` dwords (IBs are padded to 8).
    pub fn pad(&mut self, align: usize) -> &mut Self {
        while self.dw.len() % align != 0 {
            let left = align - self.dw.len() % align;
            if left == 1 {
                self.dw.push(0xffff_1000); // type-2 style filler the CP skips
            } else {
                let h = self.header(op::NOP, left - 1);
                self.dw.push(h);
                self.dw.extend(std::iter::repeat(0).take(left - 1));
            }
        }
        self
    }

    /// Write the 64-bit GPU timestamp to `va` once all earlier work in the
    /// queue has finished (RELEASE_MEM, BOTTOM_OF_PIPE_TS, data = timestamp,
    /// sent after write confirm). Ticks run at the device's counter
    /// frequency (amdgpu `gpu_counter_freq`).
    pub fn timestamp(&mut self, va: u64) -> &mut Self {
        assert_eq!(va & 7, 0, "timestamp VA must be 8-byte aligned");
        const BOTTOM_OF_PIPE_TS: u32 = 0x28;
        let event = BOTTOM_OF_PIPE_TS | (5 << 8); // EVENT_INDEX 5: end-of-pipe
        let sel = (3 << 29) | (3 << 24); // DATA_SEL timestamp, INT_SEL after write confirm, DST_SEL memory
        self.packet(op::RELEASE_MEM, &[event, sel, va as u32, (va >> 32) as u32, 0, 0, 0])
    }

    /// Make memory written before this IB (by the CPU or by earlier work)
    /// visible to the shaders after it: ACQUIRE_MEM over the whole address
    /// range, invalidating the instruction (GLI), scalar (GLK), vector
    /// (GLV), GL1 and GL2 caches, with GL2 and GLM written back first. The
    /// same GCR_CNTL as Linux's `gfx_v12_0_emit_mem_sync` (nvd.h bit
    /// positions). Put it at the start of every IB: SMEM loads and the
    /// instruction fetch go through caches the vector path does not flush.
    pub fn acquire_all(&mut self) -> &mut Self {
        const GLI_INV: u32 = 1 << 0;
        const GLM_WB: u32 = 1 << 4;
        const GLM_INV: u32 = 1 << 5;
        const GLK_INV: u32 = 1 << 7;
        const GLV_INV: u32 = 1 << 8;
        const GL1_INV: u32 = 1 << 9;
        const GL2_INV: u32 = 1 << 14;
        const GL2_WB: u32 = 1 << 15;
        let gcr = GLI_INV | GLM_WB | GLM_INV | GLK_INV | GLV_INV | GL1_INV | GL2_INV | GL2_WB;
        // CP_COHER_CNTL, CP_COHER_SIZE (lo, hi: everything), CP_COHER_BASE
        // (lo, hi), POLL_INTERVAL, GCR_CNTL.
        self.packet(op::ACQUIRE_MEM, &[0, 0xffff_ffff, 0x00ff_ffff, 0, 0, 0xa, gcr])
    }

    /// Little-endian bytes.
    pub fn bytes(&self) -> Vec<u8> {
        self.dw.iter().flat_map(|d| d.to_le_bytes()).collect()
    }
}

/// Everything one compute dispatch needs.
#[derive(Debug, Clone)]
pub struct Dispatch {
    /// GPU VA of the shader; must be 256-byte aligned.
    pub shader_va: u64,
    /// VGPRs used per thread.
    pub vgprs: u32,
    /// User SGPR values (s0..), at most 16. The workgroup id is not placed
    /// after them: GFX12 loads it into TTMP9 (X) and TTMP7 ({Z, Y}), per the
    /// RDNA4 ISA guide §3.5.3.4 ("CS SGPR Load").
    pub user_sgprs: Vec<u32>,
    /// Threads per workgroup.
    pub threads: [u32; 3],
    /// Workgroups.
    pub groups: [u32; 3],
    /// Wave32 (true) or wave64.
    pub wave32: bool,
    /// Workgroup-shared memory (LDS) per workgroup, in bytes.
    pub lds_bytes: u32,
}

/// Bytes per COMPUTE_PGM_RSRC2.LDS_SIZE step, measured on the R9700
/// (2026-11-04): 512, not the 1 KiB LDS block of the ISA guide §3.3.5.
pub const LDS_SIZE_UNIT: u32 = 512;

/// Append the state and launch for `d` to `b`.
pub fn emit_dispatch(b: &mut Blob, d: &Dispatch) {
    use regs::*;
    assert_eq!(d.shader_va & 0xff, 0, "shader VA must be 256-byte aligned");
    assert!(d.user_sgprs.len() <= 16);
    let granule = if d.wave32 { 8 } else { 4 };
    let vgpr_field = (d.vgprs.max(1) + granule - 1) / granule - 1;
    let rsrc1 = (vgpr_field << compute_pgm_rsrc1::VGPRS_SHIFT)
        // FLOAT_MODE: round to nearest even; denormals preserved for f32 (0x30)
        // and f16/f64 (0xc0). GIR and its interpreter are IEEE with denormals,
        // and the exact f32 division sequence needs them on.
        | (0xf0 << compute_pgm_rsrc1::FLOAT_MODE_SHIFT)
        | (1 << compute_pgm_rsrc1::MEM_ORDERED_SHIFT);
    let rsrc2 = ((d.user_sgprs.len() as u32) << compute_pgm_rsrc2::USER_SGPR_SHIFT)
        | (0 << compute_pgm_rsrc2::TIDIG_COMP_CNT_SHIFT) // local id in v0
        // Workgroup ids: X in TTMP9 always; TGID_Y/Z_EN make the CP load
        // TTMP7 = {Z, Y} too (RDNA4 ISA guide §3.5.3.4).
        | compute_pgm_rsrc2::TGID_X_EN_MASK
        | compute_pgm_rsrc2::TGID_Y_EN_MASK
        | compute_pgm_rsrc2::TGID_Z_EN_MASK
        | (d.lds_bytes.div_ceil(LDS_SIZE_UNIT) << compute_pgm_rsrc2::LDS_SIZE_SHIFT);
    assert!(d.lds_bytes <= 64 * 1024, "LDS is 64 KiB per workgroup");
    b.set_sh(COMPUTE_PGM_LO, &[(d.shader_va >> 8) as u32, (d.shader_va >> 40) as u32]);
    b.set_sh(COMPUTE_PGM_RSRC1, &[rsrc1, rsrc2]);
    b.set_sh(COMPUTE_PGM_RSRC3, &[0]);
    b.set_sh(COMPUTE_RESOURCE_LIMITS, &[0]);
    b.set_sh(COMPUTE_TMPRING_SIZE, &[0]);
    // All CUs on every shader engine.
    for se in [COMPUTE_STATIC_THREAD_MGMT_SE0, COMPUTE_STATIC_THREAD_MGMT_SE1,
               COMPUTE_STATIC_THREAD_MGMT_SE2, COMPUTE_STATIC_THREAD_MGMT_SE3] {
        b.set_sh(se, &[0xffff_ffff]);
    }
    b.set_sh(COMPUTE_START_X, &[0, 0, 0]);
    b.set_sh(COMPUTE_NUM_THREAD_X, &d.threads);
    if !d.user_sgprs.is_empty() {
        b.set_sh(COMPUTE_USER_DATA_0, &d.user_sgprs);
    }
    let initiator = (1 << compute_dispatch_initiator::COMPUTE_SHADER_EN_SHIFT)
        | (1 << compute_dispatch_initiator::FORCE_START_AT_000_SHIFT)
        | if d.wave32 { 1 << compute_dispatch_initiator::CS_W32_EN_SHIFT } else { 0 };
    b.packet(op::DISPATCH_DIRECT, &[d.groups[0], d.groups[1], d.groups[2], initiator]);
}
