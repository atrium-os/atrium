//! The `mmap`-based loader for flat shader blobs — the
//! JIT-emit path's counterpart to [`crate::dlopen`].
//!
//! A backend that emits an `atrium-spv-blob` produces a
//! flat, position-independent code blob with no ELF/Mach-O
//! framing and no external relocations. Loading it doesn't
//! need `dlopen`: `mmap` an anonymous region, copy the
//! code in, synchronise the instruction cache, flip the
//! pages to read+execute, and the per-stage entry points
//! are just `base + offset`.
//!
//! Isolated in its own module so the `libc` `mmap` calls
//! sit behind a local `allow(unsafe_code)` island, exactly
//! like `dlopen.rs`.
//!
//! # Why the icache flush matters
//!
//! On ARM64 the instruction and data caches are not
//! coherent. Freshly-written code sits in the D-cache; the
//! I-cache may hold stale bytes for those addresses.
//! Executing without a clean+invalidate sequence is a
//! classic nondeterministic-crash bug. `dlopen` does this
//! for us today; the JIT path must do it explicitly —
//! `__clear_cache` on FreeBSD/Linux, `sys_icache_invalidate`
//! on macOS.
//!
//! # macOS hardened-runtime note
//!
//! The production loader runs in the daemon on FreeBSD,
//! where `mmap` RW → write → `mprotect` RX is all that's
//! needed. macOS aarch64 enforces W^X for JIT pages, so
//! the host build (used only for `cargo test` / iteration)
//! takes the `MAP_JIT` + `pthread_jit_write_protect_np`
//! path instead. Both produce an executable mapping of the
//! same bytes.

#![allow(unsafe_code)]

use std::ptr;

use atrium_spv_blob::ShaderBlob;

use crate::dlopen::{CsMain, FsMain, ShaderEntryPoints, VsMain};
use crate::LoadError;

/// An executable mapping of a shader blob's code.
///
/// Owns the `mmap` region; `Drop` `munmap`s it. The
/// function pointers handed back in [`ShaderEntryPoints`]
/// point *into* this mapping, so a `JitMapping` must
/// outlive every call through them — the `LoadedShader`
/// that holds both enforces that by field drop order.
#[derive(Debug)]
pub(crate) struct JitMapping {
    base: *mut u8,
    /// Page-rounded mapping length passed to `munmap`.
    len: usize,
}

// SAFETY: the mapping is immutable + executable after
// construction; the raw pointer is just an address. Same
// reasoning as `LoadedShader`'s Send/Sync.
unsafe impl Send for JitMapping {}
unsafe impl Sync for JitMapping {}

impl Drop for JitMapping {
    fn drop(&mut self) {
        if !self.base.is_null() {
            // SAFETY: `base`/`len` are exactly what `mmap`
            // returned in `map_blob`; unmapped once.
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.len);
            }
        }
    }
}

/// `mmap` a shader blob's code into an executable region
/// and resolve its per-stage entry-point function
/// pointers.
///
/// The returned [`ShaderEntryPoints`] borrow their
/// addresses from the returned [`JitMapping`] — keep them
/// together.
pub(crate) fn map_blob(blob: &ShaderBlob)
    -> Result<(JitMapping, ShaderEntryPoints), LoadError>
{
    let code = &blob.code;
    if code.is_empty() {
        return Err(LoadError::Internal(
            "shader blob has empty code section".into()));
    }
    // Round the mapping up to a whole number of pages.
    // SAFETY: `sysconf` with a valid name; returns -1 only
    // on a bogus name, which `_SC_PAGESIZE` is not.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(LoadError::Internal(
            "sysconf(_SC_PAGESIZE) failed".into()));
    }
    let page = page as usize;
    let len = (code.len() + page - 1) & !(page - 1);

    // SAFETY: `map_exec` does the platform-specific
    // mmap/copy/icache-flush/protect dance; it returns a
    // valid executable mapping of `code` of length `len`,
    // or an error.
    let base = unsafe { map_exec(code, len)? };

    // Entry points: just `base + offset`, transmuted to the
    // stage's ABI function-pointer type. SAFETY: the blob
    // parser already validated each offset is 4-aligned and
    // `< code.len()`, and the bytes there are the backend's
    // emitted machine code for that stage's entry function.
    let entry_points = unsafe {
        ShaderEntryPoints {
            vs_main: blob.entries.vs.map(|o| {
                std::mem::transmute::<*mut u8, VsMain>(base.add(o as usize))
            }),
            fs_main: blob.entries.fs.map(|o| {
                std::mem::transmute::<*mut u8, FsMain>(base.add(o as usize))
            }),
            cs_main: blob.entries.cs.map(|o| {
                std::mem::transmute::<*mut u8, CsMain>(base.add(o as usize))
            }),
        }
    };

    if entry_points.vs_main.is_none()
        && entry_points.fs_main.is_none()
        && entry_points.cs_main.is_none()
    {
        // Drop the mapping we just made before erroring.
        drop(JitMapping { base, len });
        return Err(LoadError::Internal(
            "shader blob exports no vs/fs/cs entry point".into()));
    }

    Ok((JitMapping { base, len }, entry_points))
}

/// FreeBSD / Linux: `mmap` RW, copy, flush the icache,
/// then `mprotect` to RX (W^X — never simultaneously
/// writable + executable).
///
/// # Safety
/// `len` must be a whole number of pages and `>= code.len()`.
#[cfg(not(target_os = "macos"))]
unsafe fn map_exec(code: &[u8], len: usize) -> Result<*mut u8, LoadError> {
    extern "C" {
        // Provided by the compiler runtime (compiler-rt /
        // libgcc) that every Rust binary links. The
        // canonical ARM64 dc-cvau / ic-ivau / dsb / isb
        // instruction-cache sync sequence.
        fn __clear_cache(start: *mut libc::c_void, end: *mut libc::c_void);
    }

    let raw = libc::mmap(
        ptr::null_mut(), len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0,
    );
    if raw == libc::MAP_FAILED {
        return Err(LoadError::Internal(format!(
            "mmap({len} bytes) failed: {}",
            std::io::Error::last_os_error())));
    }
    let base = raw as *mut u8;
    ptr::copy_nonoverlapping(code.as_ptr(), base, code.len());
    // Sync I-cache with the freshly-written D-cache *before*
    // the pages can be executed.
    __clear_cache(base as *mut libc::c_void,
                  base.add(code.len()) as *mut libc::c_void);
    if libc::mprotect(raw, len, libc::PROT_READ | libc::PROT_EXEC) != 0 {
        let err = std::io::Error::last_os_error();
        libc::munmap(raw, len);
        return Err(LoadError::Internal(format!(
            "mprotect(RX) failed: {err}")));
    }
    Ok(base)
}

/// macOS aarch64: hardened-runtime JIT path. The region is
/// mapped `MAP_JIT` (RWX-capable); `pthread_jit_write_protect_np`
/// toggles the calling thread between write-enabled and
/// execute-enabled, and `sys_icache_invalidate` does the
/// cache sync.
///
/// # Safety
/// `len` must be a whole number of pages and `>= code.len()`.
#[cfg(target_os = "macos")]
unsafe fn map_exec(code: &[u8], len: usize) -> Result<*mut u8, LoadError> {
    extern "C" {
        fn pthread_jit_write_protect_np(enabled: libc::c_int);
        fn sys_icache_invalidate(start: *mut libc::c_void,
                                 len: libc::size_t);
    }

    let raw = libc::mmap(
        ptr::null_mut(), len,
        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT, -1, 0,
    );
    if raw == libc::MAP_FAILED {
        return Err(LoadError::Internal(format!(
            "mmap(MAP_JIT, {len} bytes) failed: {}",
            std::io::Error::last_os_error())));
    }
    let base = raw as *mut u8;
    // Thread enters write-enabled mode, writes the code,
    // then returns to execute-enabled mode.
    pthread_jit_write_protect_np(0);
    ptr::copy_nonoverlapping(code.as_ptr(), base, code.len());
    pthread_jit_write_protect_np(1);
    sys_icache_invalidate(raw, code.len());
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a constant-colour fragment shader's SPIR-V.
    fn const_color_spirv(rgba: [f32; 4]) -> Vec<u8> {
        use rspirv::binary::Assemble;
        use rspirv::spirv::{
            AddressingModel, Capability, ExecutionMode, ExecutionModel,
            FunctionControl, MemoryModel, StorageClass,
        };
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 0);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void = b.type_void();
        let f32_ty = b.type_float(32, None);
        let vec4 = b.type_vector(f32_ty, 4);
        let void_fn = b.type_function(void, vec![]);
        let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
        let cs: Vec<_> = rgba.iter()
            .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        let color = b.constant_composite(vec4, cs);
        let out = b.variable(ptr_out, None, StorageClass::Output, None);
        b.decorate(out, rspirv::spirv::Decoration::Location,
                   vec![rspirv::dr::Operand::LiteralBit32(0)]);
        let main = b.begin_function(void, None,
            FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        b.store(out, color, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
        b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
        let words: Vec<u32> = b.module().assemble();
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
        bytes
    }

    /// Compile a real shader straight to a blob, `mmap` it
    /// executable, call `atrium_fs_main`, and check the
    /// pixels — the end-to-end proof that the mmap-load
    /// path produces *callable* code (incl. the icache
    /// flush; a stale I-cache would crash or misbehave).
    #[test]
    fn maps_and_runs_a_const_shader() {
        let rgba = [0.25f32, 0.5, 0.75, 1.0];
        let spirv = const_color_spirv(rgba);
        let module = atrium_spv_frontend::translate(&spirv)
            .expect("frontend translate");
        let out = atrium_spv_backend_bespoke::compile_blob(
            &module, atrium_spv_backend_bespoke::Target::host())
            .expect("compile_blob");
        let blob = ShaderBlob::from_bytes(&out.blob).expect("blob parses");

        let (_mapping, entry_points) = map_blob(&blob)
            .expect("map_blob");
        let fs_main = entry_points.fs_main
            .expect("fragment blob exports fs_main");

        let mut color = [0f32; 4];
        let mut depth = 0f32;
        // SAFETY: fs_main is the AAPCS64 fragment entry of a
        // shader the bespoke backend just produced; the
        // argument shapes match docs/spec §4.1.
        unsafe {
            fs_main(
                ptr::null(), ptr::null(), ptr::null(),
                0.0, 0.0, 0.0, 0.0, 0,
                color.as_mut_ptr(), &mut depth,
            );
        }
        assert_eq!(color, rgba,
            "mmap'd shader produced {color:?}, expected {rgba:?}");
    }

    #[test]
    fn rejects_empty_code() {
        let blob = ShaderBlob {
            arch: atrium_spv_blob::ARCH_AARCH64,
            code: Vec::new(),
            entries: atrium_spv_blob::EntryOffsets::default(),
        };
        assert!(map_blob(&blob).is_err());
    }
}
