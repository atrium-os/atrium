//! Thin shell-out wrapper around `spirv-opt --ssa-rewrite
//! --eliminate-dead-code-aggressive` -- the Khronos SPIRV-Tools
//! mem2reg pass that promotes `OpVariable Function` +
//! `OpStore`/`OpLoad` into proper SSA with `OpPhi` nodes at
//! merge blocks.
//!
//! Why we need this: slangc emits function-local variables for
//! anything that can't be expressed in pure expression form --
//! loop counters, accumulators, conditionally-assigned values,
//! basically any non-trivial control flow.  atrium-spv-frontend
//! rejects `OpVariable` in its per-instruction dispatch (no
//! in-tree mem2reg yet), so without this pass every Slang
//! shader past the trivial expression level fails compile.
//!
//! `spirv-opt --ssa-rewrite` is the canonical, battle-tested
//! solution -- shipped with vulkan-tools and used by every
//! production SPIR-V toolchain.  We shell out instead of using
//! the spirv-tools Rust crate to avoid hard-depending on the C++
//! library at build time (the crate already exists here as an
//! optional dev-dep for the validator cross-check, behind the
//! `spirv-tools-cross-check` feature).
//!
//! Off by default: the daemon runs without this pass unless
//! `--spirv-opt-binary PATH` is provided.  Shaders that don't
//! need mem2reg (trivial Slang or pure-rspirv) are unaffected.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `spirv-opt --ssa-rewrite --eliminate-dead-code-aggressive`
/// on `bytes` and return the rewritten SPIR-V.
///
/// The two passes pair: `--ssa-rewrite` inserts OpPhi at merge
/// blocks and replaces OpLoad/OpStore uses with the phi result,
/// but leaves the now-dead OpVariable + initial OpStore in
/// place.  `--eliminate-dead-code-aggressive` removes those.
/// Without DCE the OpVariable still trips
/// atrium-spv-frontend's per-instruction reject.
///
/// On any failure (binary missing, non-zero exit, stderr noise)
/// returns `Err(diag)` so the caller can fall through to running
/// the validator against the original bytes -- the operator
/// still gets a clean error, just not the helpful one.
pub fn rewrite_to_ssa(
    bytes: &[u8],
    binary: &Path,
) -> Result<Vec<u8>, String> {
    let mut child = Command::new(binary)
        .arg("--ssa-rewrite")
        .arg("--eliminate-dead-code-aggressive")
        .arg("-o").arg("-")  // write SPIR-V to stdout
        .arg("-")            // read SPIR-V from stdin
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn spirv-opt {}: {e}", binary.display()))?;

    // Stream the input on a worker thread so we don't deadlock
    // against a child that writes stdout before fully draining
    // stdin (small shaders don't, but bounded threads avoid the
    // sharp edge unconditionally).
    let mut stdin = child.stdin.take()
        .ok_or_else(|| "spirv-opt stdin pipe closed".to_string())?;
    let bytes_clone = bytes.to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes_clone);
        drop(stdin); // close stdin so spirv-opt finishes
    });

    let output = child.wait_with_output()
        .map_err(|e| format!("wait spirv-opt: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "spirv-opt exit={:?}: {}",
            output.status.code(),
            stderr.trim(),
        ));
    }
    if output.stdout.len() < 20 {
        return Err(format!(
            "spirv-opt produced {} bytes of output (expected SPIR-V \
             >= 20 B for the header alone)",
            output.stdout.len(),
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Try `spirv-opt --version` to see whether the binary is
    /// reachable.  Skip the test if it isn't (CI without
    /// spirv-tools installed shouldn't fail).
    fn spirv_opt_available() -> Option<std::path::PathBuf> {
        let candidates = [
            "/opt/homebrew/bin/spirv-opt",
            "/usr/local/bin/spirv-opt",
            "/usr/bin/spirv-opt",
        ];
        for c in candidates {
            let p = std::path::PathBuf::from(c);
            if p.exists() { return Some(p); }
        }
        None
    }

    #[test]
    fn rewrite_eliminates_op_variable_function() {
        let Some(binary) = spirv_opt_available() else { return };
        // Trivial SPIR-V with an OpVariable Function used in
        // a single store + single load.  After --ssa-rewrite
        // + DCE the OpVariable should be gone.
        // We use rspirv to build it so the test stays robust.
        use rspirv::binary::Assemble;
        use rspirv::spirv::{
            AddressingModel, Capability, ExecutionMode, ExecutionModel,
            FunctionControl, MemoryModel, StorageClass,
        };
        let mut b = rspirv::dr::Builder::new();
        b.set_version(1, 3);
        b.capability(Capability::Shader);
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void   = b.type_void();
        let u32_ty = b.type_int(32, 0);
        let void_fn = b.type_function(void, vec![]);
        let ptr_u  = b.type_pointer(None, StorageClass::Function, u32_ty);
        let c_42   = b.constant_bit32(u32_ty, 42);
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let v = b.variable(ptr_u, None, StorageClass::Function, None);
        b.store(v, c_42, None, vec![]).unwrap();
        let _x = b.load(u32_ty, None, v, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        b.entry_point(ExecutionModel::GLCompute, main, "main", vec![]);
        b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
        let words: Vec<u32> = b.module().assemble();
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }

        // Original has at least one OpVariable Function.
        // Opcode for OpVariable = 59, storage-class operand for
        // Function = 7; we just smoke-check by walking the words.
        let rewritten = rewrite_to_ssa(&bytes, &binary).expect("rewrite");
        assert!(rewritten.len() >= 20);
        // The rewritten module's body OpVariable count should be
        // strictly less than the original (we had 1; should be 0).
        let count_op_variable = |buf: &[u8]| -> usize {
            let words: Vec<u32> = buf.chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]]))
                .collect();
            let mut n = 0usize;
            let mut i = 5;
            while i < words.len() {
                let w0 = words[i];
                let wc = (w0 >> 16) as usize;
                let op = (w0 & 0xFFFF) as u16;
                if wc == 0 { break; }
                if op == 59 { n += 1; }
                i += wc;
            }
            n
        };
        assert!(
            count_op_variable(&rewritten) < count_op_variable(&bytes),
            "rewrite must drop the trivially-promotable OpVariable Function",
        );
    }

    #[test]
    fn rewrite_errors_cleanly_on_missing_binary() {
        let r = rewrite_to_ssa(b"junk", std::path::Path::new("/no/such/binary"));
        assert!(r.is_err());
    }
}
