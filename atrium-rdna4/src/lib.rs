//! atrium-rdna4: SPIR-V compute kernels on RDNA4 without a Vulkan driver.
//!
//! Compile once with [`Kernel::from_spirv`], upload [`Kernel::code`] to a
//! 256-byte-aligned GPU address, then [`Kernel::record`] a dispatch into a
//! PM4 [`Blob`] for each launch. Submission (and buffer allocation) is the
//! caller's: frescod uses atrium-gpu-rs, the oracle uses amdgpu on Linux.
//!
//! Resource binding is by address. The compute ABI (see
//! [`atrium_rdna4_compiler::Abi`]) passes each storage buffer's 64-bit GPU
//! address and, if the kernel has push constants, the address of a buffer
//! holding the push-constant block. Workgroup memory comes from LDS.
//!
//! ```ignore
//! let k = Kernel::from_spirv(&spv, &Options::default())?;
//! upload(code_va, &k.code());
//! let mut ib = Blob::compute();
//! k.record(&mut ib, code_va, &[((0, 0), scene_va), ((0, 1), tiles_va)], Some(push_va), [n, 1, 1])?;
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use atrium_rdna4_cmd::{Blob, Dispatch};
pub use atrium_rdna4_compiler::{Abi, Binary};

/// Why a kernel could not be built or recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The SPIR-V frontend rejected the module.
    Frontend(String),
    /// Building or verifying GIR failed.
    Gir(String),
    /// The RDNA4 backend rejected the kernel.
    Compile(String),
    /// A dispatch was recorded without a buffer or push block the kernel uses.
    Binding(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

/// Build options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Entry point index in the module.
    pub entry: usize,
    /// Spec-constant overrides (SpecId → value bits).
    pub spec_overrides: std::collections::HashMap<u32, u32>,
}

/// A compiled compute kernel.
#[derive(Debug, Clone)]
pub struct Kernel {
    /// Entry-point name.
    pub name: String,
    /// The machine code and its ABI.
    pub bin: Binary,
}

/// Required alignment of the code's GPU address.
pub const CODE_ALIGN: u64 = 256;

impl Kernel {
    /// Compile entry point `opts.entry` of a SPIR-V module.
    pub fn from_spirv(spirv: &[u8], opts: &Options) -> Result<Kernel, Error> {
        let fo = atrium_spv_frontend::Options { spec_overrides: opts.spec_overrides.clone(), native_subgroups: true };
        let m = atrium_spv_frontend::translate_with_options(spirv, &fo).map_err(|e| Error::Frontend(format!("{e:?}")))?;
        let mut g = atrium_gir::build::build(&m, opts.entry).map_err(|e| Error::Gir(format!("{e:?}")))?;
        atrium_gir::passes::optimize(&mut g);
        atrium_gir::verify::verify(&g).map_err(|e| Error::Gir(format!("{e:?}")))?;
        let bin = atrium_rdna4_compiler::compile(&g).map_err(|e| Error::Compile(format!("{e:?}")))?;
        Ok(Kernel { name: g.name.clone(), bin })
    }

    /// The code as bytes, to upload at a [`CODE_ALIGN`]-aligned address.
    pub fn code(&self) -> Vec<u8> {
        self.bin.code.iter().flat_map(|d| d.to_le_bytes()).collect()
    }

    /// Storage buffers the kernel reads or writes, as (set, binding).
    pub fn buffers(&self) -> &[(u32, u32)] {
        &self.bin.abi.buffers
    }

    /// True if the kernel reads push constants (pass `push_va` to
    /// [`Kernel::record`]).
    pub fn uses_push_constants(&self) -> bool {
        self.bin.abi.push
    }

    /// Workgroup size.
    pub fn local_size(&self) -> [u32; 3] {
        self.bin.local_size
    }

    /// The dispatch for `groups` workgroups, with `buffers` mapping each
    /// (set, binding) the kernel uses to its GPU address and `push_va` the
    /// address of the push-constant block. Extra buffers are ignored.
    pub fn dispatch(&self, code_va: u64, buffers: &[((u32, u32), u64)], push_va: Option<u64>, groups: [u32; 3]) -> Result<Dispatch, Error> {
        if code_va % CODE_ALIGN != 0 {
            return Err(Error::Binding(format!("code address {code_va:#x} is not {CODE_ALIGN}-byte aligned")));
        }
        let mut user = vec![];
        for b in &self.bin.abi.buffers {
            let va = buffers.iter().find(|(k, _)| k == b).map(|(_, va)| *va).ok_or_else(|| Error::Binding(format!("no address for buffer (set {}, binding {})", b.0, b.1)))?;
            user.extend([va as u32, (va >> 32) as u32]);
        }
        if self.bin.abi.push {
            let va = push_va.ok_or_else(|| Error::Binding("kernel reads push constants but no push block address was given".into()))?;
            user.extend([va as u32, (va >> 32) as u32]);
        }
        Ok(Dispatch {
            shader_va: code_va,
            vgprs: self.bin.vgprs,
            user_sgprs: user,
            threads: self.bin.local_size,
            groups,
            wave32: true,
            lds_bytes: self.bin.lds_bytes,
        })
    }

    /// Append the state and launch for a dispatch (see [`Kernel::dispatch`])
    /// to `ib`.
    pub fn record(&self, ib: &mut Blob, code_va: u64, buffers: &[((u32, u32), u64)], push_va: Option<u64>, groups: [u32; 3]) -> Result<(), Error> {
        let d = self.dispatch(code_va, buffers, push_va, groups)?;
        atrium_rdna4_cmd::emit_dispatch(ib, &d);
        Ok(())
    }
}
