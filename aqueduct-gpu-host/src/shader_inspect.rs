//! SPIR-V module diagnostic dump.
//!
//! Walks a module and produces a [`ModuleReport`] describing version,
//! generator, capabilities, extensions, entry points, instruction
//! counts, and per-`OpLoopMerge` loop-control state. Powers the
//! `aqueduct-shader-tool inspect` subcommand and is useful for
//! debugging "why did the validator reject this?" or "did annotate
//! actually patch all the loops I expected?"
//!
//! Parses defensively: a malformed module produces a partial report
//! plus a list of parse warnings, not a panic.

#![warn(missing_docs)]

use std::fmt;

/// SPIR-V module magic number.
const SPIRV_MAGIC: u32 = 0x07230203;

/// One entry point declared by the module.
#[derive(Debug, Clone)]
pub struct EntryPoint {
    /// SPIR-V `ExecutionModel` value (5 = GLCompute, 0 = Vertex,
    /// 4 = Fragment, etc.).
    pub execution_model: u32,
    /// Name string from the `OpEntryPoint` instruction.
    pub name: String,
}

impl EntryPoint {
    /// Human-readable execution-model name. Returns the raw number
    /// as a string for unrecognised values.
    pub fn model_name(&self) -> String {
        match self.execution_model {
            0  => "Vertex",
            1  => "TessellationControl",
            2  => "TessellationEvaluation",
            3  => "Geometry",
            4  => "Fragment",
            5  => "GLCompute",
            6  => "Kernel",
            5267 => "TaskNV",
            5268 => "MeshNV",
            5364 => "RayGenerationKHR",
            5365 => "IntersectionKHR",
            5366 => "AnyHitKHR",
            5367 => "ClosestHitKHR",
            5368 => "MissKHR",
            5369 => "CallableKHR",
            _ => return format!("model_{}", self.execution_model),
        }.to_string()
    }
}

/// Information about one `OpLoopMerge` instruction.
#[derive(Debug, Clone)]
pub struct LoopMergeInfo {
    /// Word offset within the module.
    pub word_offset: usize,
    /// `LoopControl` mask.
    pub loop_control: u32,
    /// Iteration-literal operands carried by the mask, in spec order.
    /// Empty if the mask carries no literal-bearing bits.
    pub literals: Vec<u32>,
}

impl LoopMergeInfo {
    /// Whether this loop satisfies the strict-mode validator
    /// (carries at least one literal-bearing LoopControl bit). Per
    /// SPIR-V spec §3.23, the literal-bearing bits are
    /// DependencyLength, MinIterations, MaxIterations,
    /// IterationMultiple, PeelCount, PartialCount.
    pub fn satisfies_strict_validator(&self) -> bool {
        const LITERAL_BITS: u32 = 0x008 | 0x010 | 0x020 | 0x040 | 0x080 | 0x100;
        self.loop_control & LITERAL_BITS != 0
    }

    /// Decode `LoopControl` mask into a list of set-bit names.
    /// Bit values per SPIR-V spec §3.23.
    pub fn mask_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        let m = self.loop_control;
        if m & 0x001 != 0 { names.push("Unroll"); }
        if m & 0x002 != 0 { names.push("DontUnroll"); }
        if m & 0x004 != 0 { names.push("DependencyInfinite"); }
        if m & 0x008 != 0 { names.push("DependencyLength"); }
        if m & 0x010 != 0 { names.push("MinIterations"); }
        if m & 0x020 != 0 { names.push("MaxIterations"); }
        if m & 0x040 != 0 { names.push("IterationMultiple"); }
        if m & 0x080 != 0 { names.push("PeelCount"); }
        if m & 0x100 != 0 { names.push("PartialCount"); }
        names
    }
}

/// Aggregate report from [`inspect`].
#[derive(Debug, Clone, Default)]
pub struct ModuleReport {
    /// Bytes in the module.
    pub byte_len: usize,
    /// SPIR-V version word (e.g. `0x0001_0600` for 1.6). 0 if header
    /// couldn't be parsed.
    pub version: u32,
    /// Generator magic + version (word 2 of the header).
    pub generator: u32,
    /// ID bound (word 3 of the header).
    pub id_bound: u32,
    /// Capabilities declared via `OpCapability`. Sorted, deduplicated.
    pub capabilities: Vec<u32>,
    /// Extensions imported via `OpExtension`. Order preserved.
    pub extensions: Vec<String>,
    /// Entry points declared via `OpEntryPoint`.
    pub entry_points: Vec<EntryPoint>,
    /// Total instructions in the module.
    pub instruction_count: usize,
    /// All `OpLoopMerge` instructions with their loop-control state.
    pub loops: Vec<LoopMergeInfo>,
    /// `OpFunction` count.
    pub function_count: usize,
    /// Non-fatal parse issues encountered while walking the module.
    pub warnings: Vec<String>,
}

impl ModuleReport {
    /// Whether every loop carries a literal bound (strict-mode
    /// validator pre-check).
    pub fn all_loops_bounded(&self) -> bool {
        self.loops.iter().all(|l| l.satisfies_strict_validator())
    }

    /// Lowercase-hex version string, e.g. "1.6".
    pub fn version_string(&self) -> String {
        let major = (self.version >> 16) & 0xFF;
        let minor = (self.version >>  8) & 0xFF;
        format!("{major}.{minor}")
    }
}

impl fmt::Display for ModuleReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  size           : {} bytes", self.byte_len)?;
        writeln!(f, "  SPIR-V version : {}", self.version_string())?;
        writeln!(f, "  generator      : {:#010x}", self.generator)?;
        writeln!(f, "  id_bound       : {}", self.id_bound)?;
        writeln!(f, "  instructions   : {}", self.instruction_count)?;
        writeln!(f, "  functions      : {}", self.function_count)?;
        writeln!(f, "  loops          : {}", self.loops.len())?;

        if !self.capabilities.is_empty() {
            writeln!(f, "  capabilities   :")?;
            for c in &self.capabilities {
                writeln!(f, "    [{c:5}] {}", capability_name(*c))?;
            }
        }
        if !self.extensions.is_empty() {
            writeln!(f, "  extensions     :")?;
            for e in &self.extensions {
                writeln!(f, "    {e}")?;
            }
        }
        if !self.entry_points.is_empty() {
            writeln!(f, "  entry points   :")?;
            for ep in &self.entry_points {
                writeln!(f, "    {:<12}  {}", ep.model_name(), ep.name)?;
            }
        }
        for (i, l) in self.loops.iter().enumerate() {
            let names = l.mask_names();
            let names = if names.is_empty() { "(none)".to_string() } else { names.join("|") };
            let lits = if l.literals.is_empty() {
                "".to_string()
            } else {
                format!(" literals={:?}", l.literals)
            };
            let mark = if l.satisfies_strict_validator() { "✓" } else { "✗" };
            writeln!(f, "  loop[{i}]@w{:<5} {} mask={:#x} ({}){}",
                     l.word_offset, mark, l.loop_control, names, lits)?;
        }
        if !self.warnings.is_empty() {
            writeln!(f, "  warnings       :")?;
            for w in &self.warnings {
                writeln!(f, "    {w}")?;
            }
        }
        Ok(())
    }
}

/// Inspect a SPIR-V byte stream. Always returns a [`ModuleReport`];
/// hard structural errors (bad magic, byte misalignment) populate
/// `warnings` rather than aborting.
pub fn inspect(bytes: &[u8]) -> ModuleReport {
    let mut report = ModuleReport::default();
    report.byte_len = bytes.len();

    if bytes.len() < 20 {
        report.warnings.push(format!("module too short ({} bytes, need ≥20)", bytes.len()));
        return report;
    }
    if bytes.len() % 4 != 0 {
        report.warnings.push(format!("not word-aligned ({} bytes)", bytes.len()));
        return report;
    }

    let words: Vec<u32> = bytes.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    if words[0] != SPIRV_MAGIC {
        report.warnings.push(format!("bad SPIR-V magic {:#010x}", words[0]));
        return report;
    }
    report.version   = words[1];
    report.generator = words[2];
    report.id_bound  = words[3];
    // words[4] reserved.

    const OP_CAPABILITY:   u16 = 17;
    const OP_EXTENSION:    u16 = 10;
    const OP_ENTRY_POINT:  u16 = 15;
    const OP_FUNCTION:     u16 = 54;
    const OP_LOOP_MERGE:   u16 = 246;

    let mut idx = 5;
    while idx < words.len() {
        let w0 = words[idx];
        let wc = (w0 >> 16) as usize;
        let opcode = (w0 & 0xFFFF) as u16;
        if wc == 0 {
            report.warnings.push(format!("zero word_count at word {idx}"));
            break;
        }
        if idx + wc > words.len() {
            report.warnings.push(format!(
                "truncated instruction at word {idx}: word_count={wc}, remaining={}",
                words.len() - idx,
            ));
            break;
        }
        report.instruction_count += 1;

        match opcode {
            OP_CAPABILITY => {
                if wc >= 2 {
                    let cap = words[idx + 1];
                    if !report.capabilities.contains(&cap) {
                        report.capabilities.push(cap);
                    }
                }
            }
            OP_EXTENSION => {
                let name = read_literal_string(&words[idx + 1..idx + wc]);
                report.extensions.push(name);
            }
            OP_ENTRY_POINT => {
                // OpEntryPoint layout:
                //   word 1: execution_model
                //   word 2: function_id
                //   word 3+: name (literal string), then interface ids
                if wc >= 3 {
                    let model = words[idx + 1];
                    // Read name starting at word idx+3 until NUL.
                    let (name, _consumed_words) = read_string_until_nul(&words[idx + 3..idx + wc]);
                    report.entry_points.push(EntryPoint {
                        execution_model: model,
                        name,
                    });
                }
            }
            OP_FUNCTION => {
                report.function_count += 1;
            }
            OP_LOOP_MERGE => {
                if wc >= 4 {
                    let mask = words[idx + 3];
                    let literals = words[idx + 4..idx + wc].to_vec();
                    report.loops.push(LoopMergeInfo {
                        word_offset: idx,
                        loop_control: mask,
                        literals,
                    });
                } else {
                    report.warnings.push(format!(
                        "OpLoopMerge at word {idx} has only {wc} words (need ≥4)"
                    ));
                }
            }
            _ => {}
        }

        idx += wc;
    }

    report.capabilities.sort();
    report
}

/// Read a NUL-terminated literal string from `words`. Consumes all
/// supplied words; caller must constrain `words` to the operand range.
fn read_literal_string(words: &[u32]) -> String {
    let (s, _) = read_string_until_nul(words);
    s
}

/// Read bytes from packed u32 words until a NUL byte is hit. Returns
/// the decoded string and the number of words consumed (including
/// the word containing the NUL).
fn read_string_until_nul(words: &[u32]) -> (String, usize) {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    let mut consumed = 0usize;
    let mut hit_nul = false;
    for w in words {
        consumed += 1;
        for shift in [0u32, 8, 16, 24] {
            let b = ((w >> shift) & 0xff) as u8;
            if b == 0 { hit_nul = true; break; }
            bytes.push(b);
        }
        if hit_nul { break; }
    }
    (String::from_utf8_lossy(&bytes).into_owned(), consumed)
}

/// Human-readable name for a few SPIR-V capability codes we care
/// about (validator denylist + commonly-allowed). Falls back to a
/// generic string for unknowns.
fn capability_name(c: u32) -> &'static str {
    match c {
        0    => "Matrix",
        1    => "Shader",
        2    => "Geometry",
        3    => "Tessellation",
        4    => "Addresses",
        5    => "Linkage",
        6    => "Kernel",
        7    => "Vector16",
        8    => "Float16Buffer",
        9    => "Float16",
        10   => "Float64",
        11   => "Int64",
        12   => "Int64Atomics",
        13   => "ImageBasic",
        14   => "ImageReadWrite",
        15   => "ImageMipmap",
        17   => "Pipes",
        18   => "Groups",
        19   => "DeviceEnqueue",
        20   => "LiteralSampler",
        21   => "AtomicStorage",
        22   => "Int16",
        23   => "TessellationPointSize",
        24   => "GeometryPointSize",
        25   => "ImageGatherExtended",
        27   => "StorageImageMultisample",
        32   => "Int8",
        33   => "InputAttachment",
        34   => "SparseResidency",
        35   => "MinLod",
        36   => "Sampled1D",
        37   => "Image1D",
        38   => "SampledCubeArray",
        39   => "SampledBuffer",
        40   => "ImageBuffer",
        41   => "ImageMSArray",
        42   => "StorageImageExtendedFormats",
        43   => "ImageQuery",
        44   => "DerivativeControl",
        45   => "InterpolationFunction",
        46   => "TransformFeedback",
        47   => "GeometryStreams",
        48   => "StorageImageReadWithoutFormat",
        49   => "StorageImageWriteWithoutFormat",
        50   => "MultiViewport",
        4477 => "RayQueryKHR (inline ray-tracing)",
        4479 => "RayTracingKHR (forbidden)",
        5266 => "MeshShadingNV (forbidden)",
        5283 => "MeshShadingEXT (forbidden)",
        5340 => "RayTracingNV (forbidden)",
        5347 => "PhysicalStorageBufferAddresses (forbidden)",
        5357 => "CooperativeMatrixNV (deferred)",
        6022 => "CooperativeMatrixKHR (deferred)",
        _    => "<unknown>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_header() -> Vec<u8> {
        let words: [u32; 5] = [SPIRV_MAGIC, 0x0001_0600, 0xCAFEBABE, 42, 0];
        let mut v = Vec::with_capacity(20);
        for w in &words { v.extend_from_slice(&w.to_le_bytes()); }
        v
    }

    fn push_inst(out: &mut Vec<u8>, opcode: u16, operands: &[u32]) {
        let wc = 1 + operands.len();
        let w0 = ((wc as u32) << 16) | (opcode as u32);
        out.extend_from_slice(&w0.to_le_bytes());
        for o in operands { out.extend_from_slice(&o.to_le_bytes()); }
    }

    #[test]
    fn parses_header_fields() {
        let m = minimal_header();
        let r = inspect(&m);
        assert_eq!(r.byte_len, 20);
        assert_eq!(r.version_string(), "1.6");
        assert_eq!(r.generator, 0xCAFE_BABE);
        assert_eq!(r.id_bound, 42);
        assert_eq!(r.instruction_count, 0);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn captures_capability_and_extension() {
        let mut m = minimal_header();
        push_inst(&mut m, 17, &[5347]);   // OpCapability PhysicalStorageBufferAddresses
        // OpExtension "SPV_KHR_ray_tracing\0\0\0\0"
        let ext = b"SPV_KHR_ray_tracing\0";
        let mut padded = ext.to_vec();
        while padded.len() % 4 != 0 { padded.push(0); }
        let nwords = padded.len() / 4;
        let mut words = Vec::new();
        for chunk in padded.chunks_exact(4) {
            words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        let mut operands = words.clone();
        operands.insert(0, 0); // dummy; we'll replace with proper instr header below
        // Actually use push_inst:
        // OpExtension = opcode 10. Word_count = 1 + nwords.
        let w0 = (((1 + nwords) as u32) << 16) | 10;
        m.extend_from_slice(&w0.to_le_bytes());
        for w in &words { m.extend_from_slice(&w.to_le_bytes()); }

        let r = inspect(&m);
        assert_eq!(r.capabilities, vec![5347]);
        assert_eq!(r.extensions, vec!["SPV_KHR_ray_tracing".to_string()]);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn captures_loop_merge_state() {
        let mut m = minimal_header();
        // Bounded loop: MaxIterations | literal 1024
        push_inst(&mut m, 246, &[1, 2, 0x20, 1024]);
        // Unrolled (Unroll bit only) — strict validator would reject
        push_inst(&mut m, 246, &[3, 4, 0x01]);

        let r = inspect(&m);
        assert_eq!(r.loops.len(), 2);
        assert!(r.loops[0].satisfies_strict_validator());
        assert_eq!(r.loops[0].literals, vec![1024]);
        assert!(!r.loops[1].satisfies_strict_validator());
        assert!(r.loops[1].mask_names().contains(&"Unroll"));
        assert!(!r.all_loops_bounded());
    }

    #[test]
    fn captures_entry_point_name() {
        let mut m = minimal_header();
        // OpEntryPoint GLCompute(5) func_id=1 "main"
        let name = b"main\0\0\0\0"; // pad to 8 bytes
        let nwords = name.len() / 4;
        let mut words = vec![5u32, 1u32]; // model, func id
        for chunk in name.chunks_exact(4) {
            words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        let wc = 1 + words.len();
        let w0 = ((wc as u32) << 16) | 15; // OpEntryPoint = 15
        m.extend_from_slice(&w0.to_le_bytes());
        for w in &words { m.extend_from_slice(&w.to_le_bytes()); }
        // Suppress unused warning for nwords
        let _ = nwords;

        let r = inspect(&m);
        assert_eq!(r.entry_points.len(), 1);
        assert_eq!(r.entry_points[0].model_name(), "GLCompute");
        assert_eq!(r.entry_points[0].name, "main");
    }

    #[test]
    fn warns_on_truncated_instruction_no_panic() {
        let mut m = minimal_header();
        // Claim word_count = 4 but provide only 1 word
        let w0 = (4u32 << 16) | 17u32;
        m.extend_from_slice(&w0.to_le_bytes());
        let r = inspect(&m);
        assert!(!r.warnings.is_empty(), "expected at least one warning");
        assert!(r.warnings.iter().any(|w| w.contains("truncated")));
    }

    #[test]
    fn warns_on_bad_magic_no_panic() {
        let mut m = minimal_header();
        m[0] ^= 0xFF;
        let r = inspect(&m);
        assert!(r.warnings.iter().any(|w| w.contains("bad SPIR-V magic")));
    }
}
