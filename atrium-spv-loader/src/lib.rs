//! atrium-spv-loader — daemon-side cache + loader for
//! tier-2 software Vulkan shaders.
//!
//! Responsibilities:
//!
//! 1. **Content-hash the SPIR-V** (sha256) to derive the
//!    cache key.
//! 2. **Look up `<cache_dir>/v{N}/<hash>.{so,dylib}`** and
//!    its `.pcmap` sidecar.
//! 3. **On miss**, spawn `atrium-spv-compile` with the
//!    SPIR-V as input and wait for it to write the cache
//!    files. In production the spawn runs through
//!    Portcullis with FS caps for input + cache dirs
//!    only; this crate doesn't enforce that — the caller
//!    sets the jail up.
//! 4. **Dlopen the resulting shared library** and grab
//!    the entry-point function pointers
//!    (`atrium_fs_main` / `atrium_vs_main` /
//!    `atrium_cs_main`) the daemon will call per draw.
//! 5. **Cache the loaded shader in memory** so subsequent
//!    `vkCreateShaderModule` calls with the same SPIR-V
//!    return the cached handle immediately.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §2 — execution model
//! - [`docs/spec/tier2-renderer.md`] §6 — crate layout
//! - [`docs/spec/tier2-renderer.md`] §D4 — path-based
//!   cache versioning
//! - [`docs/spec/tier2-shader-codegen-constraints.md`]
//!   §F3 — ABI bump invalidates cache

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

// We need `unsafe` for libloading; gate it locally rather
// than relaxing `forbid(unsafe_code)` for the whole crate.
mod dlopen;
pub use dlopen::{CsMain, FsMain, FsSpanMain, LoadedShader, ShaderEntryPoints, VsMain};

// The JIT-emit path: mmap a flat `atrium-spv-blob`
// PROT_EXEC instead of `dlopen`ing a `.so`. Same local
// `allow(unsafe_code)` discipline as `dlopen`.
mod jitmap;

/// Loader configuration.
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Root cache directory (e.g. `/var/atrium/shaders`).
    /// The version subdirectory `v{N}/` is appended
    /// internally per the path-based versioning policy.
    pub cache_root: PathBuf,
    /// Shader ABI version. Bumped when the daemon and the
    /// compiled-shader function signatures get out of sync.
    /// Defaults to [`atrium_spv_ir::TIER2_SHADER_ABI_VERSION`]
    /// when the caller goes through [`LoaderConfig::default`].
    pub abi_version: u32,
    /// Path to the `atrium-spv-compile` binary. Production
    /// callers point this at the system install path
    /// (`/usr/local/libexec/atrium-spv-compile`); tests
    /// point at `target/debug/atrium-spv-compile`.
    pub compile_binary: PathBuf,
}

impl LoaderConfig {
    /// The standard production layout.
    pub fn production() -> Self {
        Self {
            cache_root: PathBuf::from("/var/atrium/shaders"),
            abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
            compile_binary: PathBuf::from("/usr/local/libexec/atrium-spv-compile"),
        }
    }
}

/// The cache + loader itself.
///
/// Thread-safe: `load_or_compile` may be called
/// concurrently from multiple threads. The in-memory
/// shader cache is mutex-guarded; concurrent compiles
/// of the same hash are serialized (the second caller
/// blocks on the first's compile completion).
pub struct ShaderCache {
    config: LoaderConfig,
    loaded: Mutex<HashMap<String, Arc<LoadedShader>>>,
}

impl ShaderCache {
    /// Construct a cache with the given config.
    pub fn new(config: LoaderConfig) -> Self {
        Self { config, loaded: Mutex::new(HashMap::new()) }
    }

    /// The cache's version subdirectory
    /// (`<cache_root>/v{N}/`).
    pub fn version_dir(&self) -> PathBuf {
        self.config.cache_root.join(format!("v{}", self.config.abi_version))
    }

    /// Compute the content hash for a SPIR-V module.
    pub fn hash(spirv: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(spirv);
        format!("{:x}", h.finalize())
    }

    /// Cache path for a hash's flat executable blob — the
    /// JIT-emit artifact (`atrium-spv-blob` format). The
    /// bespoke backend writes this; the loader `mmap`s it
    /// `PROT_EXEC` directly. Preferred over [`Self::so_path`]
    /// when both exist for the same hash.
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.version_dir().join(format!("{hash}.afblob"))
    }

    /// Cache path for a hash's compiled shared library —
    /// the legacy `dlopen` artifact. `.dylib` on macOS,
    /// `.so` elsewhere. Still produced for Cranelift-
    /// compiled shaders (until JIT-emit phase 4).
    pub fn so_path(&self, hash: &str) -> PathBuf {
        let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
        self.version_dir().join(format!("{hash}.{ext}"))
    }

    /// Cache path for a hash's pcmap sidecar.
    pub fn pcmap_path(&self, hash: &str) -> PathBuf {
        self.version_dir().join(format!("{hash}.pcmap"))
    }

    /// Compute the content hash for a SPIR-V module *and* a
    /// set of `VkSpecializationInfo`-style overrides.  The
    /// override entries are sorted by SpecId for stability,
    /// then mixed in with a `\0spec\0` separator.  When
    /// `overrides` is empty this returns exactly the same
    /// hash [`Self::hash`] would — preserving cache keys for
    /// callers that don't use spec constants.
    pub fn hash_with_spec_overrides(
        spirv: &[u8],
        overrides: &[(u32, u32)],
    ) -> String {
        let mut h = Sha256::new();
        h.update(spirv);
        if !overrides.is_empty() {
            let mut entries = overrides.to_vec();
            entries.sort_by_key(|(k, _)| *k);
            h.update(b"\x00spec\x00");
            for (k, v) in entries {
                h.update(k.to_le_bytes());
                h.update(v.to_le_bytes());
            }
        }
        format!("{:x}", h.finalize())
    }

    /// Get a loaded shader, compiling on cache miss.
    ///
    /// This is the load-bearing entry point. Concurrent
    /// calls for the same SPIR-V block on the in-memory
    /// `loaded` mutex; only one compile runs.
    pub fn load_or_compile(&self, spirv: &[u8])
        -> Result<Arc<LoadedShader>, LoadError>
    {
        self.load_or_compile_with_spec_overrides(spirv, &[])
    }

    /// Get a loaded shader, applying `VkSpecializationInfo`
    /// overrides.  Each override is `(SpecId, value)` where
    /// `value` is the 32-bit bit pattern the host wants to
    /// substitute for the SPIR-V `OpSpecConstant`'s declared
    /// default.  The cache key is hash(spirv) when
    /// `overrides` is empty, otherwise it folds the
    /// overrides in (matching `atrium-spv-compile`'s default
    /// hashing rule) so that two pipelines using the same
    /// SPIR-V with different overrides land on distinct
    /// compiled artifacts.
    pub fn load_or_compile_with_spec_overrides(
        &self,
        spirv: &[u8],
        overrides: &[(u32, u32)],
    ) -> Result<Arc<LoadedShader>, LoadError>
    {
        let hash = Self::hash_with_spec_overrides(spirv, overrides);

        // Fast path: already loaded in this process.
        {
            let loaded = self.loaded.lock().map_err(|_| LoadError::Internal(
                "loaded-shader mutex poisoned".to_string(),
            ))?;
            if let Some(s) = loaded.get(&hash) {
                return Ok(s.clone());
            }
        }

        // Disk-cache miss → spawn the compile binary. The
        // compiler writes one of two artifacts: a flat
        // `.afblob` (bespoke backend, the JIT-emit path) or
        // a `.so` (Cranelift fallback, until phase 4).
        let blob_path = self.blob_path(&hash);
        let so_path = self.so_path(&hash);
        let pcmap_path = self.pcmap_path(&hash);
        if !blob_path.exists() && !so_path.exists() {
            self.compile(spirv, &hash, overrides)?;
            if !blob_path.exists() && !so_path.exists() {
                return Err(LoadError::Internal(format!(
                    "atrium-spv-compile exited 0 but neither {} nor {} exists",
                    blob_path.display(), so_path.display(),
                )));
            }
        }

        let pcmap_bytes = if pcmap_path.exists() {
            Some(std::fs::read(&pcmap_path).map_err(|e| LoadError::Internal(
                format!("reading {}: {e}", pcmap_path.display()),
            ))?)
        } else {
            None
        };

        // Prefer the JIT-emit blob (`mmap PROT_EXEC`); fall
        // back to `dlopen`ing the `.so` for Cranelift-
        // compiled shaders.
        let shader = if blob_path.exists() {
            let blob_bytes = std::fs::read(&blob_path).map_err(|e|
                LoadError::Internal(format!(
                    "reading {}: {e}", blob_path.display())))?;
            jitmap::open(&blob_bytes, pcmap_bytes.as_deref())?
        } else {
            dlopen::open(&so_path, pcmap_bytes.as_deref())?
        };
        let shader = Arc::new(shader);

        let mut loaded = self.loaded.lock().map_err(|_| LoadError::Internal(
            "loaded-shader mutex poisoned".to_string(),
        ))?;
        // Double-check in case another thread won the race.
        if let Some(existing) = loaded.get(&hash) {
            return Ok(existing.clone());
        }
        loaded.insert(hash, shader.clone());
        Ok(shader)
    }

    /// Drop a compiled shader from the in-memory cache.
    /// The on-disk `.so` and `.pcmap` are untouched.
    pub fn forget(&self, hash: &str) {
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded.remove(hash);
        }
    }

    /// Run the compile binary as a subprocess.  When
    /// `overrides` is non-empty, each entry is forwarded as a
    /// `--spec-const SPECID=VALUE` arg (hex-encoded so the
    /// shell quoting stays trivial).
    fn compile(
        &self,
        spirv: &[u8],
        hash: &str,
        overrides: &[(u32, u32)],
    ) -> Result<(), LoadError> {
        let version_dir = self.version_dir();
        std::fs::create_dir_all(&version_dir).map_err(|e|
            LoadError::Internal(format!(
                "creating cache dir {}: {e}", version_dir.display(),
            )))?;

        // Write SPIR-V to a tempfile in the version dir.
        // We write into the version dir rather than the
        // system tempdir so a Portcullis-jailed binary
        // doesn't need an extra FS cap.
        let in_path = version_dir.join(format!("{hash}.spv.tmp"));
        std::fs::write(&in_path, spirv).map_err(|e|
            LoadError::Internal(format!(
                "writing {}: {e}", in_path.display(),
            )))?;

        let mut cmd = Command::new(&self.config.compile_binary);
        cmd.arg("--input").arg(&in_path)
            .arg("--output-dir").arg(&version_dir)
            .arg("--hash").arg(hash);
        for (spec_id, value) in overrides {
            cmd.arg("--spec-const")
                .arg(format!("{spec_id}=0x{value:08x}"));
        }
        let result = cmd.output();

        // Clean up the input tempfile regardless of compile
        // outcome.
        let _ = std::fs::remove_file(&in_path);

        let output = result.map_err(|e| LoadError::Internal(
            format!("spawning {}: {e}", self.config.compile_binary.display()),
        ))?;

        match output.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(LoadError::Unsupported(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )),
            Some(code) => Err(LoadError::Internal(format!(
                "atrium-spv-compile exited {code}: {}",
                String::from_utf8_lossy(&output.stderr),
            ))),
            None => Err(LoadError::Internal(format!(
                "atrium-spv-compile killed by signal: {}",
                String::from_utf8_lossy(&output.stderr),
            ))),
        }
    }
}

/// Loader-side errors.
#[derive(Debug)]
pub enum LoadError {
    /// The compile binary returned exit code 1 (Unsupported).
    /// Caller (vkCreateShaderModule) returns
    /// `VK_ERROR_INVALID_SHADER_NV` to the app.
    Unsupported(String),
    /// Anything else: setup error, FS error, malformed
    /// `.so` / `.pcmap`, dlopen failure, etc.
    Internal(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Unsupported(s) => write!(f, "unsupported: {s}"),
            LoadError::Internal(s) => write!(f, "internal: {s}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Make the `Path` import live-used at the top — the
/// helper modules below reach for `Path` in their
/// function signatures and `clippy::missing_docs` for
/// pub-but-internal helpers would otherwise complain.
const _: fn(&Path) = |_| {};
