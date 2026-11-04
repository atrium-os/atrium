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
        let r = |o: &O, v: &mut Vec<VR>| {
            if let O::R(x, _) = o {
                v.push(*x)
            }
        };
        let (mut u, mut d) = (vec![], vec![]);
        match self {
            MI::Valu { dst, sdst, src, .. } => {
                r(dst, &mut d);
                if let Some(s) = sdst {
                    r(s, &mut d);
                }
                for s in src {
                    r(s, &mut u);
                }
            }
            MI::Salu { dst, src, .. } => {
                r(dst, &mut d);
                for s in src {
                    r(s, &mut u);
                }
            }
            MI::Smem { sdst, soff, .. } => {
                r(sdst, &mut d);
                if let Some(x) = soff {
                    r(x, &mut u);
                }
            }
            MI::Ds { vdst, addr, data, .. } => {
                if let Some(x) = vdst {
                    r(x, &mut d);
                }
                r(addr, &mut u);
                if let Some(x) = data {
                    r(x, &mut u);
                }
            }
            MI::Global { vdst, vaddr, data, .. } => {
                if let Some(x) = vdst {
                    r(x, &mut d);
                }
                r(vaddr, &mut u);
                if let Some(x) = data {
                    r(x, &mut u);
                }
            }
            MI::Scratch { vdst, data, .. } => {
                if let Some(x) = vdst {
                    r(x, &mut d);
                }
                if let Some(x) = data {
                    r(x, &mut u);
                }
            }
            _ => {}
        }
        (u, d)
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
            _ => {}
        }
        v
    }
}
