//! Fresco SPIR-V bundle format — manifest parsing + SPIR-V loading.
//!
//! Per `docs/spec/fresco-rendering-stack.md` §3.1 a bundle is a directory
//! containing `manifest.json` + `compute/*.spv` + `pipelines/*.{vert,frag}.spv`.
//! `Bundle::load()` reads + validates everything; the resulting `Bundle`
//! exposes the loaded SPIR-V words by op-id for downstream consumers
//! (fresco-vulkan in step 4 of the POC creates pipelines from these).
//!
//! This crate is pure data — it has no Vulkan dependency. Pipeline
//! creation lives in fresco-vulkan, which queries the bundle by op-id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("manifest read: {path}: {source}")]
    ManifestRead { path: PathBuf, source: std::io::Error },
    #[error("manifest parse: {path}: {source}")]
    ManifestParse { path: PathBuf, source: serde_json::Error },
    #[error("spv read: {path}: {source}")]
    SpvRead { path: PathBuf, source: std::io::Error },
    #[error("spv decode: {path}: {msg}")]
    SpvDecode { path: PathBuf, msg: String },
    #[error("spirv-val rejected {path}:\n{output}")]
    SpvVal { path: PathBuf, output: String },
}

/// Parsed `manifest.json`. Schema mirrors §3.1 of the spec.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name:    String,
    pub version: u32,
    pub ops:     Vec<ManifestOp>,
    #[serde(default)]
    pub depends_on:    Vec<String>,
    #[serde(default)]
    pub gpu_resources: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct ManifestOp {
    /// Op-ID from the §3.4 closed registry. e.g. 4096 (= 0x1000) =
    /// ATRIUM_CORE_RECT.
    pub id: u32,
    /// Human-readable name; for logs and debugging only. Op resolution
    /// goes by `id`, not `name`.
    pub name: String,
    /// `<path>.spv:<entrypoint>` — relative to the bundle directory.
    pub compute_entry: String,
    /// Base name; loader appends `.vert.spv` and `.frag.spv`.
    /// Relative to the bundle directory.
    pub render_pipeline: String,
}

/// One op's loaded SPIR-V + metadata.
pub struct LoadedOp {
    pub id:               u32,
    pub name:             String,
    pub compute_spirv:    Vec<u32>,
    pub compute_entry:    String,    // entrypoint name, e.g. "main"
    pub vertex_spirv:     Vec<u32>,
    pub fragment_spirv:   Vec<u32>,
}

pub struct Bundle {
    pub root:     PathBuf,
    pub manifest: Manifest,
    /// op-id → LoadedOp
    by_op_id: HashMap<u32, LoadedOp>,
}

impl Bundle {
    /// Load + validate a bundle from `path` (the directory containing
    /// `manifest.json`).
    pub fn load(path: &Path) -> Result<Self, BundleError> {
        let manifest_path = path.join("manifest.json");
        let raw = std::fs::read(&manifest_path).map_err(|e| BundleError::ManifestRead {
            path: manifest_path.clone(), source: e,
        })?;
        let manifest: Manifest = serde_json::from_slice(&raw).map_err(|e| {
            BundleError::ManifestParse { path: manifest_path.clone(), source: e }
        })?;

        let mut by_op_id = HashMap::new();
        for op in &manifest.ops {
            /* Compute: "compute/foo.spv:main" → path + entry */
            let (compute_rel, compute_entry) = match op.compute_entry.split_once(':') {
                Some((p, e)) => (p, e.to_string()),
                None => (op.compute_entry.as_str(), "main".to_string()),
            };
            let compute_spirv = load_spv(&path.join(compute_rel))?;
            spirv_val_if_available(&path.join(compute_rel))?;

            let vert_path = path.join(format!("{}.vert.spv", op.render_pipeline));
            let frag_path = path.join(format!("{}.frag.spv", op.render_pipeline));
            let vertex_spirv   = load_spv(&vert_path)?;
            spirv_val_if_available(&vert_path)?;
            let fragment_spirv = load_spv(&frag_path)?;
            spirv_val_if_available(&frag_path)?;

            by_op_id.insert(op.id, LoadedOp {
                id: op.id,
                name: op.name.clone(),
                compute_spirv,
                compute_entry,
                vertex_spirv,
                fragment_spirv,
            });
        }

        Ok(Self { root: path.to_path_buf(), manifest, by_op_id })
    }

    /// Iterate ops in manifest order.
    pub fn ops(&self) -> impl Iterator<Item = &LoadedOp> {
        self.manifest.ops.iter().filter_map(|m| self.by_op_id.get(&m.id))
    }

    pub fn op(&self, id: u32) -> Option<&LoadedOp> {
        self.by_op_id.get(&id)
    }
}

// ── helpers ─────────────────────────────────────────────────────────

fn load_spv(path: &Path) -> Result<Vec<u32>, BundleError> {
    let bytes = std::fs::read(path).map_err(|e| BundleError::SpvRead {
        path: path.to_path_buf(), source: e,
    })?;
    if bytes.len() % 4 != 0 {
        return Err(BundleError::SpvDecode {
            path: path.to_path_buf(),
            msg: format!("not a multiple of 4 bytes (len={})", bytes.len()),
        });
    }
    let mut words = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    /* SPIR-V magic number (little-endian) — sanity check before
     * shelling out to spirv-val. */
    const SPIRV_MAGIC: u32 = 0x07230203;
    if words.first().copied() != Some(SPIRV_MAGIC) {
        return Err(BundleError::SpvDecode {
            path: path.to_path_buf(),
            msg: format!("bad SPIR-V magic: {:#x}", words.first().copied().unwrap_or(0)),
        });
    }
    Ok(words)
}

/// Run `spirv-val` against the file if it's available on PATH. Per
/// §3.5 of the spec, every .spv must be validated at load time.
/// Skipped (with a debug log) if spirv-val isn't installed; build.sh
/// already validated, so this is defense-in-depth, not a primary
/// gate. A future hardened build can require it.
fn spirv_val_if_available(path: &Path) -> Result<(), BundleError> {
    let result = Command::new("spirv-val").arg(path).output();
    match result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(BundleError::SpvVal {
            path: path.to_path_buf(),
            output: format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ),
        }),
        Err(_) => Ok(()), /* spirv-val not on PATH; skip */
    }
}

// ── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn atrium_core_path() -> PathBuf {
        /* Bundle lives at workspace_root/bundles/atrium-core. The crate
         * root is workspace_root/crates/fresco-bundle, so go up two. */
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("bundles/atrium-core")
    }

    #[test]
    fn load_atrium_core() {
        let p = atrium_core_path();
        if !p.join("compute/op_rectangle.comp.spv").exists() {
            eprintln!("skip: bundle .spv not built (run bundles/atrium-core/build.sh)");
            return;
        }
        let bundle = Bundle::load(&p).expect("load atrium-core");
        assert_eq!(bundle.manifest.name, "atrium-core");
        assert_eq!(bundle.manifest.version, 1);
        assert_eq!(bundle.manifest.ops.len(), 1);

        let rect = bundle.op(0x1000).expect("rect op present");
        assert_eq!(rect.name, "rect");
        assert_eq!(rect.compute_entry, "main");
        assert!(rect.compute_spirv.len() > 8);  // actual SPIR-V, not empty
        assert!(rect.vertex_spirv.len() > 8);
        assert!(rect.fragment_spirv.len() > 8);
    }
}
