//! Instruction emission on top of `atrium-rdna4-isa`: operand codes, inline
//! constants, literals and branch fixups.

use atrium_rdna4_isa::{encode, instruction, is_base_condition, ENCODINGS};

/// Source-operand code of `exec_lo`.
pub const EXEC_LO: u16 = 126;
/// Source-operand code of `null`.
pub const NULL: u16 = 124;
/// Source-operand code that means "a 32-bit literal follows".
pub const LITERAL: u16 = 255;

/// Source-operand code of VGPR `n`.
pub fn v(n: u8) -> u16 {
    256 + n as u16
}

/// The inline-constant operand code for `bits`, if one exists.
pub fn inline_const(bits: u32) -> Option<u16> {
    let i = bits as i32;
    if (0..=64).contains(&i) {
        return Some(128 + i as u16);
    }
    if (-16..=-1).contains(&i) {
        return Some((192 - i) as u16);
    }
    let f = f32::from_bits(bits);
    let table = [(0.5, 240), (-0.5, 241), (1.0, 242), (-1.0, 243), (2.0, 244), (-2.0, 245), (4.0, 246), (-4.0, 247)];
    table.iter().find(|(x, _)| *x == f && bits != 0x8000_0000).map(|&(_, c)| c)
}

/// A machine-code buffer.
#[derive(Default)]
pub struct Asm {
    /// The code, in dwords.
    pub words: Vec<u32>,
}

/// Find the base (non-literal, non-DPP) condition for `inst` in `enc`.
fn condition(inst: &str, enc: &str) -> &'static str {
    let def = instruction(inst).unwrap_or_else(|| panic!("unknown instruction {inst}"));
    def.encodings
        .iter()
        .find(|e| ENCODINGS[e.encoding].name == enc && is_base_condition(e.condition))
        .unwrap_or_else(|| panic!("{inst} has no base {enc} form"))
        .condition
}

impl Asm {
    /// Current position in dwords.
    pub fn pos(&self) -> usize {
        self.words.len()
    }

    /// Emit one instruction. Panics on an encoding error: the lowering only
    /// produces fields it knows to be in range.
    pub fn emit(&mut self, inst: &str, enc: &str, fields: &[(&str, u64)]) {
        let w = encode(inst, enc, condition(inst, enc), fields)
            .unwrap_or_else(|e| panic!("encode {inst} {enc} {fields:?}: {e:?}"));
        self.words.extend(w);
    }

    /// `v_mov_b32 vdst, literal` (VOP1 with a trailing literal).
    pub fn v_mov_lit(&mut self, vdst: u8, bits: u32) {
        match inline_const(bits) {
            Some(c) => self.emit("V_MOV_B32", "ENC_VOP1", &[("VDST", vdst as u64), ("SRC0", c as u64)]),
            None => {
                self.emit("V_MOV_B32", "ENC_VOP1", &[("VDST", vdst as u64), ("SRC0", LITERAL as u64)]);
                self.words.push(bits);
            }
        }
    }

    /// VOP3 with up to three sources (VOP1/VOP2/VOP3/VOPC opcodes alike).
    pub fn vop3(&mut self, inst: &str, vdst: u64, srcs: &[u16]) {
        let names = ["SRC0", "SRC1", "SRC2"];
        let mut f: Vec<(&str, u64)> = vec![("VDST", vdst)];
        for (i, s) in srcs.iter().enumerate() {
            f.push((names[i], *s as u64));
        }
        self.emit(inst, "ENC_VOP3", &f);
    }

    /// SOP2 `sdst = op(s0, s1)`.
    pub fn sop2(&mut self, inst: &str, sdst: u16, s0: u16, s1: u16) {
        self.emit(inst, "ENC_SOP2", &[("SDST", sdst as u64), ("SSRC0", s0 as u64), ("SSRC1", s1 as u64)]);
    }

    /// SOP1 `sdst = op(s0)`.
    pub fn sop1(&mut self, inst: &str, sdst: u16, s0: u16) {
        self.emit(inst, "ENC_SOP1", &[("SDST", sdst as u64), ("SSRC0", s0 as u64)]);
    }

    /// SOPP with an immediate.
    pub fn sopp(&mut self, inst: &str, simm16: u16) {
        self.emit(inst, "ENC_SOPP", &[("SIMM16", simm16 as u64)]);
    }

    /// Emit a SOPP branch with a zero offset; returns its position for
    /// [`Asm::patch_branch`].
    pub fn branch(&mut self, inst: &str) -> usize {
        let p = self.pos();
        self.sopp(inst, 0);
        p
    }

    /// Point the branch at `at` to dword position `target`.
    pub fn patch_branch(&mut self, at: usize, target: usize) {
        let off = target as i64 - (at as i64 + 1);
        assert!((-32768..32768).contains(&off), "branch out of range");
        self.words[at] = (self.words[at] & 0xffff_0000) | (off as u16 as u32);
    }
}
