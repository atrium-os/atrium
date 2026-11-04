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
pub mod emit;
pub mod isel;
pub mod mir;
pub mod regalloc;

use atrium_gir::Function;

/// Compilation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not handled by this backend yet.
    Unsupported(String),
    /// A bug in the compiler.
    Internal(String),
}

/// How resources reach the shader (the interim compute ABI).
///
/// User SGPRs `s[2i:2i+1]` hold the 64-bit address of `buffers[i]`; the
/// workgroup id x, y, z follow in the next three SGPRs; `v0` holds the
/// packed local invocation id.
#[derive(Debug, Clone)]
pub struct Abi {
    /// Storage buffers in user-SGPR order, as (set, binding).
    pub buffers: Vec<(u32, u32)>,
}

impl Abi {
    /// The ABI for `f`: its buffers in (set, binding) order.
    pub fn for_function(f: &Function) -> Abi {
        Abi { buffers: f.buffers.clone() }
    }
    /// Number of user SGPRs.
    pub fn user_sgprs(&self) -> u8 {
        2 * self.buffers.len() as u8
    }
    /// First SGPR of the workgroup id.
    pub fn tgid_sgpr(&self) -> u8 {
        self.user_sgprs()
    }
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
}

/// Compile an optimised GIR kernel.
pub fn compile(f: &Function) -> Result<Binary, Error> {
    if f.push_bytes > 0 {
        return Err(Error::Unsupported("push constants (G4)".into()));
    }
    if f.workgroup_bytes > 0 {
        return Err(Error::Unsupported("workgroup memory (G4)".into()));
    }
    if f.buffers.len() > 8 {
        return Err(Error::Unsupported("more than 8 storage buffers".into()));
    }
    let abi = Abi::for_function(f);
    let u = atrium_gir::passes::uniformity::analyse(f);
    let mut is = isel::Isel::new(f, &u, &abi);
    is.run()?;
    let asg = regalloc::allocate(&is.mi, &is.vregs, 1, abi.tgid_sgpr() + 3)?;
    let code = emit::emit(&is.mi, &is.vregs, &asg);
    Ok(Binary { code, vgprs: asg.vgprs, sgprs: asg.sgprs, abi, local_size: f.local_size })
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
                let lit = if has_lit && i + n < code.len() { n += 1; format!(" lit=0x{:08x}", code[i + n - 1]) } else { String::new() };
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
