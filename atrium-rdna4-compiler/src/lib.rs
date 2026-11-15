//! atrium-rdna4-compiler — the RDNA4 (GFX12) machine layer of the shader
//! compiler: GIR → RDNA4 machine code (docs/spec/atrium-gpu-ir.md §7).
//!
//! Pipeline: [`isel`] (GIR → machine IR on virtual registers, EXEC-masked
//! control flow) → [`regalloc`] (live intervals with loop extension,
//! first-fit) → [`emit`] (waits, hazards, encoding, branch fixups).
//!
//! G3 scope: compute kernels using storage buffers; all data in VGPRs, lane
//! masks and buffer addresses in SGPRs. Uniform values on the scalar unit,
//! workgroup memory, push constants and subgroup ops come next.

#![warn(missing_docs)]

pub mod asm;
pub mod coalesce;
pub mod emit;
pub mod isel;
pub mod mir;
pub mod regalloc;
pub mod spill;

use atrium_gir::Function;

/// Compilation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not handled by this backend yet.
    Unsupported(String),
    /// A bug in the compiler.
    Internal(String),
}

/// How resources reach the shader (the interim ABI).
///
/// User SGPRs, in order: for a vertex shader with varyings, the attribute
/// ring's buffer descriptor (4 SGPRs, stride 16 × [`Binary::vs_params`]);
/// for every vertex shader, the draw's first vertex and first instance
/// (`gl_BaseVertex`, `gl_BaseInstance`); then the 64-bit
/// address of each of `buffers`; then, if the shader has push constants,
/// the address of the push-constant block (read with uniform loads).
///
/// * Compute: user SGPRs start at `s0`; the workgroup id is architected
///   (X in `ttmp9`, Y/Z in `ttmp7` halves); `v0` holds the packed local
///   invocation id.
/// * Vertex (an NGG primitive shader, GFX12): `s2` is `gs_tg_info`, `s3`
///   `merged_wave_info`, `s5` the attribute ring offset (512-byte units),
///   user SGPRs start at `s8`; `v0` is the packed
///   primitive, `v3` the vertex id and `v4` the instance id, both counted
///   from zero.
/// * Fragment: user SGPRs start at `s0`, the primitive mask follows them;
///   input VGPRs as [`Binary::ps_input_ena`] says (perspective centre
///   barycentrics in `v0`–`v1` always, then the enabled position
///   components and front face).
#[derive(Debug, Clone)]
pub struct Abi {
    /// Shader stage.
    pub stage: Stage,
    /// Storage buffers in user-SGPR order, as (set, binding).
    pub buffers: Vec<(u32, u32)>,
    /// The kernel reads push constants.
    pub push: bool,
    /// Vertex shader: number of varyings written to the attribute ring.
    pub vs_params: u8,
}

impl Abi {
    /// The ABI for `f`: its buffers in (set, binding) order.
    pub fn for_function(f: &Function) -> Abi {
        let vs_params = if f.stage == Stage::Vertex {
            f.outputs.iter().filter(|o| matches!(o.kind, atrium_gir::OutputKind::Location(_))).count() as u8
        } else {
            0
        };
        Abi { stage: f.stage, buffers: f.buffers.clone(), push: f.push_bytes > 0, vs_params }
    }
    /// First user SGPR.
    pub fn user_base(&self) -> u8 {
        if self.stage == Stage::Vertex { 8 } else { 0 }
    }
    /// User SGPRs before the buffer addresses (vertex: first vertex and
    /// first instance).
    pub fn fixed_user(&self) -> u8 {
        if self.stage == Stage::Vertex { 2 + self.attr_ring_sgprs() } else { 0 }
    }
    /// SGPRs of the attribute ring descriptor (4 when the vertex shader
    /// has varyings, at the first user SGPR).
    pub fn attr_ring_sgprs(&self) -> u8 {
        if self.vs_params > 0 { 4 } else { 0 }
    }
    /// SGPR of `gl_BaseVertex` (vertex shaders; `gl_BaseInstance` follows).
    pub fn base_vertex_sgpr(&self) -> u8 {
        self.user_base() + self.attr_ring_sgprs()
    }
    /// Number of user SGPRs.
    pub fn user_sgprs(&self) -> u8 {
        self.fixed_user() + 2 * self.buffers.len() as u8 + if self.push { 2 } else { 0 }
    }
    /// SGPR holding the address of `buffers[i]` (first of a pair).
    pub fn buffer_sgpr(&self, i: usize) -> u8 {
        self.user_base() + self.fixed_user() + 2 * i as u8
    }
    /// First user SGPR of the push-constant block address.
    pub fn push_sgpr(&self) -> Option<u8> {
        self.push.then(|| self.buffer_sgpr(self.buffers.len()))
    }
    /// SGPRs the hardware fills before the shader starts (system and user
    /// SGPRs, and the fragment primitive mask); allocation starts after.
    pub fn input_sgprs(&self) -> u8 {
        self.user_base() + self.user_sgprs() + if self.stage == Stage::Fragment { 1 } else { 0 }
    }
}

/// `SPI_PS_INPUT_ENA` bits.
pub mod ps_input {
    /// Perspective barycentrics at the pixel centre (2 VGPRs).
    pub const PERSP_CENTER: u32 = 1 << 1;
    /// Window x of the pixel centre (1 VGPR each, x..w in order).
    pub const POS_X_FLOAT: u32 = 1 << 8;
    /// Window y.
    pub const POS_Y_FLOAT: u32 = 1 << 9;
    /// Depth.
    pub const POS_Z_FLOAT: u32 = 1 << 10;
    /// W (`1/gl_FragCoord.w`).
    pub const POS_W_FLOAT: u32 = 1 << 11;
    /// Front face (sign: front when > 0).
    pub const FRONT_FACE: u32 = 1 << 12;
    /// Linear (no-perspective) barycentrics at the pixel centre (2 VGPRs).
    pub const LINEAR_CENTER: u32 = 1 << 4;
}

/// A compiled kernel.
#[derive(Debug, Clone)]
pub struct Binary {
    /// Machine code.
    pub code: Vec<u32>,
    /// VGPRs per lane.
    pub vgprs: u32,
    /// SGPRs used.
    pub sgprs: u32,
    /// The ABI the code expects.
    pub abi: Abi,
    /// Workgroup size.
    pub local_size: [u32; 3],
    /// Workgroup-shared memory (LDS) in bytes.
    pub lds_bytes: u32,
    /// Scratch (spill) space per lane, in dwords; 0 = the dispatch needs
    /// no scratch.
    pub scratch_dwords: u32,
    /// Fragment shaders: `SPI_PS_INPUT_ENA` (and `_ADDR`) for the input
    /// VGPRs the code reads; 0 otherwise.
    pub ps_input_ena: u32,
    /// Graphics: the exports at the end of the shader, as (target, enabled
    /// component mask); position 0 is target 12, primitive 20, MRT n is n,
    /// depth (MRTZ) 8.
    pub exports: Vec<(u8, u8)>,
    /// Vertex shaders: the location of each attribute-ring parameter, in
    /// parameter order (parameter `i` is at byte 16 × `i` of a vertex).
    pub vs_params: Vec<u32>,
    /// Fragment shaders: each `ds_param_load` attribute index's location
    /// and interpolation; `SPI_PS_INPUT_CNTL_i` must point attribute `i`
    /// at the vertex shader's parameter for that location (and set
    /// `FLAT_SHADE` for flat ones).
    pub ps_inputs: Vec<(u32, atrium_gir::Interp)>,
}

use atrium_gir::Stage;

/// Compile options.
#[derive(Debug, Clone)]
pub struct Options {
    /// VGPR budget per lane (at most 256). Values that do not fit are
    /// spilled to scratch.
    pub max_vgprs: u32,
    /// SGPR budget (at most 106). Values that do not fit spill into
    /// VGPR lanes.
    pub max_sgprs: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options { max_vgprs: 256, max_sgprs: 106 }
    }
}

/// Compile an optimised GIR kernel.
pub fn compile(f: &Function) -> Result<Binary, Error> {
    compile_with(f, &Options::default())
}

/// [`compile`] with options.
pub fn compile_with(f: &Function, o: &Options) -> Result<Binary, Error> {
    atrium_gir::verify::verify(f).map_err(|e| Error::Internal(format!("invalid GIR: {e:?}")))?;
    if f.workgroup_bytes > 64 * 1024 {
        return Err(Error::Unsupported("more than 64 KiB of workgroup memory".into()));
    }
    let abi = Abi::for_function(f);
    if abi.user_sgprs() > 16 {
        return Err(Error::Unsupported("more than 16 user SGPRs (8 buffers, or 7 plus push constants; one fewer in a vertex shader)".into()));
    }
    let u = atrium_gir::passes::uniformity::analyse(f);
    let mut is = isel::Isel::new(f, &u, &abi);
    is.run()?;
    coalesce::run(&mut is.mi, &is.vregs);
    coalesce::merge(&mut is.mi, &is.vregs);
    if std::env::var_os("ATRIUM_RDNA4_PRESSURE").is_some() {
        let (start, end) = regalloc::intervals(&is.mi, is.vregs.len());
        let (peak, at) = regalloc::pressure(&is.vregs, &start, &end);
        eprintln!("rdna4 {}: peak VGPR pressure {peak} at instruction {at} of {}", f.name, is.mi.len());
        if std::env::var_os("ATRIUM_RDNA4_PRESSURE_LIVE").is_some() {
            let loops: Vec<usize> = is.mi.iter().enumerate().filter(|(_, m)| matches!(m, mir::MI::LoopBegin | mir::MI::LoopEnd)).map(|(i, _)| i).collect();
            eprintln!("  loop markers {loops:?}");
            for r in 0..is.vregs.len() {
                if start[r] != usize::MAX && is.vregs[r].class == mir::Class::V && start[r] <= at && end[r] >= at {
                    let def = is.mi.iter().position(|m| m.regs().1.contains(&(r as u32)));
                    let name = def.map(|d| format!("{:?}", is.mi[d]).chars().take(90).collect::<String>()).unwrap_or_default();
                    eprintln!("  v{r} x{} [{}..{}] def@{def:?} {name}", is.vregs[r].size, start[r], end[r]);
                }
            }
        }
    }
    let v_first = is.input_vgprs();
    let (asg, scratch_dwords) = spill::allocate(&mut is.mi, &mut is.vregs, v_first, abi.input_sgprs(), o.max_vgprs.min(256), o.max_sgprs.min(106))?;
    let code = emit::emit(&is.mi, &is.vregs, &asg);
    let (ps_input_ena, exports) = (is.ps_input_ena, is.exports.clone());
    let mut vs_params: Vec<u32> = f.outputs.iter().filter_map(|o| match o.kind {
        atrium_gir::OutputKind::Location(l) if f.stage == Stage::Vertex => Some(l),
        _ => None,
    }).collect();
    vs_params.sort();
    let ps_inputs = if f.stage == Stage::Fragment { f.inputs.iter().map(|i| (i.location, i.interp)).collect() } else { vec![] };
    Ok(Binary {
        code,
        vgprs: asg.vgprs.max(v_first as u32),
        sgprs: asg.sgprs.max(abi.input_sgprs() as u32),
        abi,
        local_size: f.local_size,
        lds_bytes: f.workgroup_bytes,
        scratch_dwords,
        ps_input_ena,
        exports,
        vs_params,
        ps_inputs,
    })
}

/// Disassembly-style listing (instruction names per decoded word group).
pub fn listing(code: &[u32]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i < code.len() {
        match atrium_rdna4_isa::decode(&code[i..]) {
            Ok(d) => {
                let mut n = d.words.max(1);
                let has_lit = d.fields.iter().any(|(k, v)| (k.starts_with("SRC") || k.starts_with("SSRC")) && *v == 255);
                // DPP16 (src0 250) and DPP8 (233) forms carry a second dword.
                let dpp = d.words == 1 && d.fields.iter().any(|(k, v)| *k == "SRC0" && (*v == 250 || *v == 233));
                let lit = if has_lit && i + n < code.len() {
                    n += 1;
                    format!(" lit=0x{:08x}", code[i + n - 1])
                } else if dpp && i + n < code.len() {
                    n += 1;
                    format!(" dpp=0x{:08x}", code[i + n - 1])
                } else {
                    String::new()
                };
                s.push_str(&format!("{i:4}: {:<24} {:?}{lit}\n", d.inst, d.fields));
                i += n;
            }
            Err(e) => {
                s.push_str(&format!("{i:4}: <{:08x}> {e:?}\n", code[i]));
                i += 1;
            }
        }
    }
    s
}
