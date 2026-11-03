//! atrium-rdna4-isa — RDNA4 (GFX12) instruction encodings.
//!
//! Every table in this crate is generated from AMD's machine-readable ISA
//! (`spec/amdgpu_isa_rdna4.xml.xz`, MIT) by `gen/gen_tables.py` into
//! `src/tables.rs`. Nothing here encodes an instruction by hand-written bit
//! layouts; this file only interprets the tables.
//!
//! Scope today: encode an instruction from named field values, and decode a
//! word stream back to an instruction and its fields. Operand parsing (an
//! assembler syntax) and the wait-counter/hazard model the compiler needs are
//! later layers (docs/spec/atrium-rdna4-vulkan-backend.md §3).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod tables;
pub use tables::{ENCODINGS, INSTRUCTIONS, ISA_RELEASE};

/// One microcode field: a name and its bit ranges `(offset, count)`, low part first.
#[derive(Debug)]
pub struct FieldDef {
    /// Field name as AMD spells it (`OP`, `SDST`, `VADDR`, ...).
    pub name: &'static str,
    /// Bit ranges within the instruction, `(bit offset, bit count)`, in order.
    pub ranges: &'static [(u32, u32)],
}

/// One instruction encoding (microcode format).
#[derive(Debug)]
pub struct EncodingDef {
    /// Encoding name (`ENC_SOP1`, `ENC_VOP3`, `VOP3_INST_LITERAL`, ...).
    pub name: &'static str,
    /// AMD's ordering; base encodings first.
    pub order: i32,
    /// Instruction size in bits (32, 64 or 96).
    pub bits: u32,
    /// Bits that identify this encoding (and, for most, the opcode).
    pub id_mask: u128,
    /// Valid identifier patterns under `id_mask`.
    pub ids: &'static [u128],
    /// Microcode fields.
    pub fields: &'static [FieldDef],
}

/// One operand of an instruction in one encoding.
#[derive(Debug)]
pub struct OperandDef {
    /// Microcode field carrying it; empty for implicit operands.
    pub field: &'static str,
    /// AMD operand type (`OPR_SREG`, `OPR_VGPR`, `OPR_SRC`, ...).
    pub kind: &'static str,
    /// Size in bits.
    pub size: u32,
    /// Read by the instruction.
    pub input: bool,
    /// Written by the instruction.
    pub output: bool,
    /// Not encoded in a field.
    pub implicit: bool,
}

/// An instruction's use of one encoding.
#[derive(Debug)]
pub struct InstEncodingDef {
    /// Index into [`ENCODINGS`].
    pub encoding: usize,
    /// `default`, or a variant condition such as `has_lit_0` or `has_dpp8`.
    pub condition: &'static str,
    /// Opcode value in this encoding's `OP` field.
    pub opcode: u32,
    /// Operands in AMD's order.
    pub operands: &'static [OperandDef],
}

/// Instruction flags (bit positions in [`InstDef::flags`]).
pub mod flag {
    /// Any branch.
    pub const BRANCH: u32 = 1 << 0;
    /// Conditional branch.
    pub const CONDITIONAL: u32 = 1 << 1;
    /// Indirect branch.
    pub const INDIRECT: u32 = 1 << 2;
    /// Ends the program.
    pub const TERMINATOR: u32 = 1 << 3;
    /// Executed by the sequencer immediately.
    pub const IMMEDIATE: u32 = 1 << 4;
}

/// One instruction.
#[derive(Debug)]
pub struct InstDef {
    /// Mnemonic (`S_LOAD_B96`, `V_ADD_F32`, ...).
    pub name: &'static str,
    /// See [`flag`].
    pub flags: u32,
    /// AMD functional group (`SALU`, `VALU`, `VMEM`, `SMEM`, ...).
    pub group: &'static str,
    /// AMD functional subgroup.
    pub subgroup: &'static str,
    /// Encodings this instruction can use.
    pub encodings: &'static [InstEncodingDef],
}

/// Errors from encoding or decoding.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// No instruction with that mnemonic.
    UnknownInstruction(String),
    /// The instruction has no encoding with that name and condition.
    NoSuchEncoding(String),
    /// The encoding has no field with that name.
    UnknownField(String),
    /// A value does not fit its field.
    FieldOverflow(String),
    /// The field is fixed by the encoding identifier (`ENCODING`, `OP`).
    FixedField(String),
    /// The opcode's identifier pattern is not in the encoding's table.
    NoIdentifier,
    /// Not enough words for the encoding found.
    Truncated,
    /// No encoding/opcode matches the words.
    Unrecognised(u32),
}

impl EncodingDef {
    /// Size in 32-bit words.
    pub fn words(&self) -> usize {
        (self.bits / 32) as usize
    }
    /// Find a field by name.
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }
}

impl FieldDef {
    /// Total width in bits.
    pub fn width(&self) -> u32 {
        self.ranges.iter().map(|r| r.1).sum()
    }
    fn mask_in(&self) -> u128 {
        self.ranges.iter().fold(0u128, |m, &(o, c)| m | (((1u128 << c) - 1) << o))
    }
    fn insert(&self, word: u128, value: u64) -> u128 {
        let mut w = word & !self.mask_in();
        let mut v = value as u128;
        for &(o, c) in self.ranges {
            w |= (v & ((1u128 << c) - 1)) << o;
            v >>= c;
        }
        w
    }
    fn extract(&self, word: u128) -> u64 {
        let mut v = 0u128;
        let mut shift = 0;
        for &(o, c) in self.ranges {
            v |= ((word >> o) & ((1u128 << c) - 1)) << shift;
            shift += c;
        }
        v as u64
    }
}

/// True for the condition names AMD uses for an encoding's base form:
/// `default`, or `Nothas_...` (no literal/DPP follows).
pub fn is_base_condition(c: &str) -> bool {
    c == "default" || c.starts_with("Not")
}

/// Look up an instruction by mnemonic (case-insensitive).
pub fn instruction(name: &str) -> Option<&'static InstDef> {
    INSTRUCTIONS.iter().find(|i| i.name.eq_ignore_ascii_case(name))
}

fn to_words(v: u128, n: usize) -> Vec<u32> {
    (0..n).map(|i| (v >> (32 * i)) as u32).collect()
}

fn from_words(w: &[u32]) -> u128 {
    w.iter().enumerate().fold(0u128, |a, (i, &x)| a | ((x as u128) << (32 * i)))
}

/// Encode `inst` in `encoding` (with `condition`; `"default"` selects the base
/// form whatever AMD calls it) from
/// named field values. Fields not given are zero. `ENCODING` and `OP` are set
/// from the tables and may not be given. Returns little-endian 32-bit words.
pub fn encode(inst: &str, encoding: &str, condition: &str, fields: &[(&str, u64)]) -> Result<Vec<u32>, Error> {
    let def = instruction(inst).ok_or_else(|| Error::UnknownInstruction(inst.into()))?;
    let ie = def
        .encodings
        .iter()
        .find(|e| ENCODINGS[e.encoding].name == encoding
            && (e.condition == condition || (condition == "default" && is_base_condition(e.condition))))
        .ok_or_else(|| Error::NoSuchEncoding(format!("{inst} {encoding} {condition}")))?;
    let enc = &ENCODINGS[ie.encoding];
    let mut w: u128 = 0;
    if let Some(op) = enc.field("OP") {
        w = op.insert(w, ie.opcode as u64);
    }
    // The identifier fixes ENCODING (and usually OP). Pick the one consistent
    // with the opcode already placed.
    let id = enc
        .ids
        .iter()
        .find(|&&id| (w & enc.id_mask & !enc.field("ENCODING").map_or(0, |f| f.mask_in())) == (id & enc.id_mask & !enc.field("ENCODING").map_or(0, |f| f.mask_in())))
        .ok_or(Error::NoIdentifier)?;
    w = (w & !enc.id_mask) | id;
    for &(name, value) in fields {
        if name == "ENCODING" || name == "OP" {
            return Err(Error::FixedField(name.into()));
        }
        let f = enc.field(name).ok_or_else(|| Error::UnknownField(name.into()))?;
        if f.width() < 64 && value >> f.width() != 0 {
            return Err(Error::FieldOverflow(name.into()));
        }
        if f.mask_in() & enc.id_mask != 0 {
            return Err(Error::FixedField(name.into()));
        }
        w = f.insert(w, value);
    }
    Ok(to_words(w, enc.words()))
}

/// A decoded instruction.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The instruction.
    pub inst: &'static str,
    /// The encoding name.
    pub encoding: &'static str,
    /// Every field of the encoding except `ENCODING` and `OP`, with its value.
    pub fields: Vec<(&'static str, u64)>,
    /// Words consumed.
    pub words: usize,
}

/// Decode one instruction from the start of `words`, considering only
/// encodings with the given conditions (base encodings: `&["default"]`).
///
/// Literal and DPP variants share their base encoding's identifier; which one
/// applies depends on operand values (for example a source field selecting
/// "literal follows"). Choosing among them is the disassembler layer's job;
/// this function decodes the base form. Dual-issue VOPD (two opcodes, `OPX`
/// and `OPY`) is not handled yet.
pub fn decode(words: &[u32]) -> Result<Decoded, Error> {
    let first = *words.first().ok_or(Error::Truncated)?;
    let mut order: Vec<usize> = (0..ENCODINGS.len()).collect();
    order.sort_by_key(|&i| ENCODINGS[i].order);
    for ei in order {
        let enc = &ENCODINGS[ei];
        if !enc.name.starts_with("ENC_") && enc.name != "VOP3_SDST_ENC" {
            continue; // variant encodings (literal, DPP, dual-issue VOPD): not decoded here
        }
        let n = enc.words();
        let avail = from_words(&words[..n.min(words.len())]);
        if !enc.ids.iter().any(|&id| avail & enc.id_mask == id) {
            continue;
        }
        if words.len() < n {
            return Err(Error::Truncated);
        }
        let w = from_words(&words[..n]);
        let opcode = enc.field("OP").map(|f| f.extract(w) as u32);
        let hit = INSTRUCTIONS.iter().find_map(|i| {
            i.encodings
                .iter()
                .find(|e| e.encoding == ei && is_base_condition(e.condition) && Some(e.opcode) == opcode.or(Some(e.opcode)))
                .map(|_| i)
        });
        let Some(inst) = hit else { continue };
        let fields = enc
            .fields
            .iter()
            .filter(|f| f.name != "ENCODING" && f.name != "OP")
            .map(|f| (f.name, f.extract(w)))
            .collect();
        return Ok(Decoded { inst: inst.name, encoding: enc.name, fields, words: n });
    }
    Err(Error::Unrecognised(first))
}
