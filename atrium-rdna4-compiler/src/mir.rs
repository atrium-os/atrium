//! Machine IR: RDNA4 instructions on virtual registers, in linear order.

/// A virtual register (a group of `size` consecutive physical registers).
pub type VR = u32;

/// Register class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Vector registers (one value per lane).
    V,
    /// Scalar registers (one value per wave; lane masks live here).
    S,
}

/// A virtual register's shape.
#[derive(Debug, Clone, Copy)]
pub struct VRegInfo {
    /// Class.
    pub class: Class,
    /// Number of consecutive registers.
    pub size: u8,
}

/// An operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum O {
    /// Component `.1` of virtual register `.0`.
    R(VR, u8),
    /// Physical VGPR.
    PV(u8),
    /// Physical SGPR.
    PS(u8),
    /// `exec_lo`.
    Exec,
    /// `vcc_lo`.
    Vcc,
    /// 32-bit immediate (inline constant or literal).
    Imm(u32),
}

/// A machine instruction.
#[derive(Debug, Clone)]
pub enum MI {
    /// VALU op in VOP3 form. `dst` is a VGPR, or an SGPR for compares;
    /// `sdst` is the extra scalar destination of `v_div_scale`.
    Valu {
        /// Instruction name.
        name: &'static str,
        /// Destination.
        dst: O,
        /// Scalar destination (VOP3 SDST form only).
        sdst: Option<O>,
        /// Sources.
        src: Vec<O>,
        /// NEG modifier bits (bit i negates source i).
        neg: u8,
    },
    /// SALU op (SOP1 with one source, SOP2 with two).
    Salu {
        /// Instruction name.
        name: &'static str,
        /// Destination.
        dst: O,
        /// Sources.
        src: Vec<O>,
    },
    /// Global memory op: address = SGPR pair `saddr` + `vaddr` + `ioff`.
    Global {
        /// Instruction name.
        name: &'static str,
        /// Loaded or returned data.
        vdst: Option<O>,
        /// 32-bit offset VGPR.
        vaddr: O,
        /// First SGPR of the 64-bit base address.
        saddr: u8,
        /// Stored data.
        data: Option<O>,
        /// Immediate byte offset.
        ioff: i32,
        /// Temporal hint (atomics: 1 = return the old value).
        th: u8,
        /// Coherence scope.
        scope: u8,
    },
    /// Scalar memory load: `sdst` = the dwords at SGPR pair `sbase` +
    /// `soff` (an SGPR, if any) + `ioff`. Ignores EXEC; returns out of
    /// order (KMCNT).
    Smem {
        /// Instruction name.
        name: &'static str,
        /// Destination SGPR group.
        sdst: O,
        /// First SGPR of the 64-bit base address.
        sbase: u8,
        /// Dynamic byte offset SGPR.
        soff: Option<O>,
        /// Immediate byte offset (signed 24-bit).
        ioff: i32,
    },
    /// LDS (workgroup memory) op: address = `addr` + `off` bytes.
    Ds {
        /// Instruction name.
        name: &'static str,
        /// Loaded or returned data.
        vdst: Option<O>,
        /// Byte-address VGPR.
        addr: O,
        /// Stored data / atomic operand.
        data: Option<O>,
        /// Immediate byte offset (0..=65535).
        off: u16,
    },
    /// Scratch (per-lane private memory) access in ST mode: address =
    /// the wave's SCRATCH_BASE + swizzle(`ioff`, lane) (RDNA4 ISA guide
    /// §11.2). Register spills and reloads.
    Scratch {
        /// Instruction name.
        name: &'static str,
        /// Loaded data.
        vdst: Option<O>,
        /// Stored data.
        data: Option<O>,
        /// Byte offset into the lane's scratch space.
        ioff: i32,
    },
    /// Workgroup execution and LDS memory barrier.
    Barrier,
    /// Export to a graphics target (position, primitive, colour, depth):
    /// one VGPR per enabled component.
    Export {
        /// Target (`TGT` field).
        tgt: u8,
        /// Component sources; `None` = not written.
        src: [Option<O>; 4],
        /// Last export of the shader.
        done: bool,
    },
    /// `buffer_store_b128` with an index (`idxen`): the attribute ring.
    BufStore {
        /// The 4 data VGPRs.
        data: O,
        /// Index VGPR.
        vindex: O,
        /// First SGPR of the buffer descriptor (4-aligned).
        rsrc: u8,
        /// Byte offset SGPR.
        soff: O,
        /// Immediate byte offset.
        ioff: u32,
    },
    /// `ds_param_load`: attribute `attr`, channel `chan` of the primitive
    /// (per quad: P0, P1−P0, P2−P0), `m0` holding the primitive mask.
    /// Counted on EXPCNT.
    ParamLoad {
        /// Destination VGPR.
        vdst: O,
        /// Attribute index.
        attr: u8,
        /// Channel.
        chan: u8,
    },
    /// `v_interp_p10_f32` / `v_interp_p2_f32` (VINTERP).
    Interp {
        /// Instruction name.
        name: &'static str,
        /// Destination.
        vdst: O,
        /// Sources.
        src: [O; 3],
    },
    /// `v_mov_b32` with a DPP16 quad permutation.
    MovDpp {
        /// Destination.
        vdst: O,
        /// Source VGPR.
        src: O,
        /// `DPP_CTRL`.
        ctrl: u16,
    },
    /// SOPP with an immediate (`s_sendmsg`).
    Sopp {
        /// Instruction name.
        name: &'static str,
        /// The immediate.
        imm: u16,
    },
    /// Branch target.
    Label(u32),
    /// SOPP branch to a label.
    Branch {
        /// Instruction name.
        name: &'static str,
        /// Label id.
        target: u32,
    },
    /// Start of a loop (liveness marker, emits nothing).
    LoopBegin,
    /// End of a loop, after its back branch (liveness marker).
    LoopEnd,
    /// End of program.
    Endpgm,
}

impl MI {
    /// Virtual registers read and written: (uses, defs).
    pub fn regs(&self) -> (Vec<VR>, Vec<VR>) {
        let (mut u, mut d) = (vec![], vec![]);
        self.each_reg(|x, def| if def { d.push(x) } else { u.push(x) });
        (u, d)
    }

    /// Every virtual register operand, with whether it is a definition, in
    /// the order [`Self::regs`] lists them; allocates nothing.
    pub fn each_reg(&self, mut f: impl FnMut(VR, bool)) {
        let mut r = |o: &O, def: bool| {
            if let O::R(x, _) = o {
                f(*x, def)
            }
        };
        match self {
            MI::Valu { dst, sdst, src, .. } => {
                r(dst, true);
                if let Some(s) = sdst {
                    r(s, true);
                }
                for s in src {
                    r(s, false);
                }
            }
            MI::Salu { dst, src, .. } => {
                r(dst, true);
                for s in src {
                    r(s, false);
                }
            }
            MI::Smem { sdst, soff, .. } => {
                r(sdst, true);
                if let Some(x) = soff {
                    r(x, false);
                }
            }
            MI::Ds { vdst, addr, data, .. } => {
                if let Some(x) = vdst {
                    r(x, true);
                }
                r(addr, false);
                if let Some(x) = data {
                    r(x, false);
                }
            }
            MI::Global { vdst, vaddr, data, .. } => {
                if let Some(x) = vdst {
                    r(x, true);
                }
                r(vaddr, false);
                if let Some(x) = data {
                    r(x, false);
                }
            }
            MI::Scratch { vdst, data, .. } => {
                if let Some(x) = vdst {
                    r(x, true);
                }
                if let Some(x) = data {
                    r(x, false);
                }
            }
            MI::Export { src, .. } => {
                for x in src.iter().flatten() {
                    r(x, false);
                }
            }
            MI::BufStore { data, vindex, soff, .. } => {
                r(data, false);
                r(vindex, false);
                r(soff, false);
            }
            MI::ParamLoad { vdst, .. } => r(vdst, true),
            MI::Interp { vdst, src, .. } => {
                r(vdst, true);
                for x in src {
                    r(x, false);
                }
            }
            MI::MovDpp { vdst, src, .. } => {
                r(vdst, true);
                r(src, false);
            }
            _ => {}
        }
    }

    /// Every register operand, mutably, each with whether it is a
    /// definition and how many consecutive registers it covers.
    pub fn operands_mut(&mut self) -> Vec<(&mut O, bool, u8)> {
        fn width(name: &str) -> u8 {
            if name.contains("ATOMIC") {
                1
            } else if name.ends_with("B128") {
                4
            } else if name.ends_with("B96") {
                3
            } else if name.ends_with("B64") {
                2
            } else {
                1
            }
        }
        let mut v: Vec<(&mut O, bool, u8)> = vec![];
        match self {
            MI::Valu { dst, sdst, src, .. } => {
                v.push((dst, true, 1));
                if let Some(s) = sdst {
                    v.push((s, true, 1));
                }
                v.extend(src.iter_mut().map(|s| (s, false, 1)));
            }
            MI::Salu { dst, src, .. } => {
                v.push((dst, true, 1));
                v.extend(src.iter_mut().map(|s| (s, false, 1)));
            }
            MI::Global { name, vdst, vaddr, data, .. } => {
                let w = width(name);
                if let Some(x) = vdst {
                    v.push((x, true, w));
                }
                v.push((vaddr, false, 1));
                if let Some(x) = data {
                    v.push((x, false, w));
                }
            }
            MI::Ds { name, vdst, addr, data, .. } => {
                let w = width(name);
                if let Some(x) = vdst {
                    v.push((x, true, w));
                }
                v.push((addr, false, 1));
                if let Some(x) = data {
                    v.push((x, false, w));
                }
            }
            MI::Scratch { name, vdst, data, .. } => {
                let w = width(name);
                if let Some(x) = vdst {
                    v.push((x, true, w));
                }
                if let Some(x) = data {
                    v.push((x, false, w));
                }
            }
            MI::Smem { name, sdst, soff, .. } => {
                let w = match *name {
                    "S_LOAD_B32" => 1,
                    "S_LOAD_B64" => 2,
                    "S_LOAD_B96" => 3,
                    "S_LOAD_B128" => 4,
                    "S_LOAD_B256" => 8,
                    _ => 16,
                };
                v.push((sdst, true, w));
                if let Some(x) = soff {
                    v.push((x, false, 1));
                }
            }
            MI::Export { src, .. } => {
                v.extend(src.iter_mut().flatten().map(|x| (x, false, 1)));
            }
            MI::BufStore { data, vindex, soff, .. } => {
                v.push((data, false, 4));
                v.push((vindex, false, 1));
                v.push((soff, false, 1));
            }
            MI::ParamLoad { vdst, .. } => v.push((vdst, true, 1)),
            MI::Interp { vdst, src, .. } => {
                v.push((vdst, true, 1));
                v.extend(src.iter_mut().map(|x| (x, false, 1)));
            }
            MI::MovDpp { vdst, src, .. } => {
                v.push((vdst, true, 1));
                v.push((src, false, 1));
            }
            _ => {}
        }
        v
    }
}
