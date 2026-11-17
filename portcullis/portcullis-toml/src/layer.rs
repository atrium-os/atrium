//! Toolchain layers (dev-tools.md §6.2, dev-tools-plan.md M3).
//!
//! A layer is a read-only tree of tools (the Rust toolchain, `c-dev`)
//! that portcullisd mounts into a workspace at `/atrium/toolchains/<name>`
//! and puts on the workspace's `PATH`. Its `atrium.toml` says what it is:
//!
//! ```toml
//! kind = "layer"
//!
//! [layer]
//! name    = "rust"
//! version = "1.100.0"
//! arch    = "aarch64"                    # amd64 | aarch64
//! mount   = "/atrium/toolchains/rust"    # always /atrium/toolchains/<name>
//! path    = ["bin"]                      # put first on PATH, in order
//! summary = "Rust 1.100.0: rustc, cargo, clippy, rustfmt, rust-analyzer"
//!
//! [layer.env]                            # ${LAYER} is the mount
//! RUST_SRC_PATH = "${LAYER}/lib/rustlib/src/rust/library"
//! ```
//!
//! This module is the manifest and what a workspace gets from a set of
//! layers (`compose`). Packing, signing, the content index and install
//! belong to Opifex (the package catalog); the installed tree is at
//! `<root>/layers/<name>@<version>/` (`installed_dir`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where every layer is mounted, in a workspace.
pub const MOUNT_BASE: &str = "/atrium/toolchains";
/// Where installed layers are, under the Atrium state root.
pub const INSTALLED: &str = "layers";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LayerManifest {
    /// Always "layer": what tells this manifest from an app's.
    pub kind:  String,
    pub layer: LayerSection,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LayerSection {
    pub name:    String,
    pub version: String,
    pub arch:    String,
    pub mount:   String,
    /// Folders in the layer put on `PATH`, first to last, before the base.
    #[serde(default)]
    pub path:    Vec<String>,
    #[serde(default)]
    pub summary: String,
    /// `sha256:<hex>` of the layer's `content.index` (path, mode, size and
    /// SHA-256 of every file), so the manifest's one signature covers every
    /// byte. Filled in by `opifex layer pack`.
    #[serde(default)]
    pub content: String,
    /// More environment, `${LAYER}` standing for the mount.
    #[serde(default)]
    pub env:     BTreeMap<String, String>,
}

/// Variables whose values are colon lists: two layers may both add to them.
const LISTS: &[&str] = &["LD_LIBRARY_PATH", "PKG_CONFIG_PATH", "MANPATH", "PYTHONPATH", "INFOPATH",
                         "ACLOCAL_PATH", "CMAKE_PREFIX_PATH"];
/// Never set by a layer: the loader's hooks, or what the workspace itself
/// owns.
const REFUSED: &[&str] = &["LD_PRELOAD", "LD_AUDIT", "LD_LIBMAP", "LD_LIBMAP_DISABLE", "LD_LIBRARY_PATH_FDS",
                           "LD_BIND_NOW", "LD_DYNAMIC_WEAKEN", "LD_UTRACE", "LD_DEBUG",
                           "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "TMPDIR", "PWD", "ENV",
                           "ATRIUM_WORKSPACE"];

pub const MAX_ENV: usize = 32;
pub const MAX_VALUE: usize = 1024;

impl LayerManifest {
    pub fn from_str(s: &str) -> Result<Self, String> {
        let m: LayerManifest = toml::from_str(s).map_err(|e| e.to_string())?;
        m.validate()?;
        Ok(m)
    }

    /// `name@version`, how a workspace record names it.
    pub fn reference(&self) -> String { format!("{}@{}", self.layer.name, self.layer.version) }

    pub fn validate(&self) -> Result<(), String> {
        let l = &self.layer;
        if self.kind != "layer" {
            return Err(format!("kind {:?}: a layer's is \"layer\"", self.kind));
        }
        if !valid_name(&l.name) {
            return Err(format!("name {:?}: up to 32 lowercase letters, digits and dashes", l.name));
        }
        if !valid_version(&l.version) {
            return Err(format!("version {:?}: up to 32 letters, digits, dots, dashes, pluses", l.version));
        }
        if !matches!(l.arch.as_str(), "amd64" | "aarch64") {
            return Err(format!("arch {:?}: amd64 or aarch64", l.arch));
        }
        if l.mount != format!("{MOUNT_BASE}/{}", l.name) {
            return Err(format!("mount {:?}: a layer named {} mounts at {MOUNT_BASE}/{}", l.mount, l.name, l.name));
        }
        if !l.content.is_empty() && content_digest(&l.content).is_none() {
            return Err(format!("content {:?}: sha256:<64 hex digits>", l.content));
        }
        if l.path.len() > 8 {
            return Err("path: at most 8 folders".into());
        }
        for p in &l.path {
            if !inside(p) {
                return Err(format!("path {p:?}: a folder inside the layer"));
            }
        }
        if l.env.len() > MAX_ENV {
            return Err(format!("env: at most {MAX_ENV} variables"));
        }
        for (k, v) in &l.env {
            let kb = k.as_bytes();
            if kb.is_empty() || kb.len() > 64 || !(kb[0].is_ascii_uppercase() || kb[0] == b'_')
                || !kb.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_') {
                return Err(format!("env {k:?}: an upper-case name"));
            }
            if REFUSED.contains(&k.as_str()) || k.starts_with("LD_32_") {
                return Err(format!("env {k}: a layer doesn't set it"));
            }
            if v.len() > MAX_VALUE || v.bytes().any(|c| c < 0x20 || c == 0x7f) {
                return Err(format!("env {k}: too long, or a control character"));
            }
            // A list of paths may name only the layer's own folders.
            if k == "LD_LIBRARY_PATH" || k.ends_with("PATH") {
                for part in v.split(':') {
                    let Some(rest) = part.strip_prefix("${LAYER}") else {
                        return Err(format!("env {k}: {part:?} is not inside the layer (${{LAYER}}/...)"));
                    };
                    if !rest.is_empty() && !(rest.starts_with('/') && inside(&rest[1..])) {
                        return Err(format!("env {k}: {part:?} is not inside the layer"));
                    }
                }
            }
        }
        Ok(())
    }

    /// A packed layer: one whose content index is named.
    pub fn require_content(&self) -> Result<[u8; 32], String> {
        content_digest(&self.layer.content).ok_or_else(|| format!("{} has no content index (not packed)", self.reference()))
    }

    /// Its environment with `${LAYER}` replaced.
    pub fn env(&self) -> Vec<(String, String)> {
        self.layer.env.iter().map(|(k, v)| (k.clone(), v.replace("${LAYER}", &self.layer.mount))).collect()
    }

    /// The `PATH` folders it adds, in order, as absolute paths in the workspace.
    pub fn path_dirs(&self) -> Vec<String> {
        self.layer.path.iter().map(|p| format!("{}/{}", self.layer.mount, p.trim_end_matches('/'))).collect()
    }
}

/// The file that lists a layer's content, at its top.
pub const CONTENT_INDEX: &str = "content.index";

/// `sha256:<hex>` → the 32 bytes.
pub fn content_digest(s: &str) -> Option<[u8; 32]> {
    let h = s.strip_prefix("sha256:")?;
    if h.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)).then_some(out)
}

/// A relative path that stays inside: no `..`, no leading `/`, no empties.
fn inside(p: &str) -> bool {
    !p.is_empty() && p.len() <= 256 && !p.starts_with('/')
        && p.trim_end_matches('/').split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && p.bytes().all(|c| c > 0x20 && c != 0x7f && c != b':')
}

pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn valid_version(v: &str) -> bool {
    let b = v.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
}

/// `name@version` → (name, version).
pub fn parse_ref(r: &str) -> Option<(&str, &str)> {
    let (n, v) = r.split_once('@')?;
    (valid_name(n) && valid_version(v)).then_some((n, v))
}

/// Its folder under the state root: `<root>/layers/<name>@<version>`.
pub fn installed_dir(root: &std::path::Path, reference: &str) -> Option<std::path::PathBuf> {
    parse_ref(reference).map(|_| root.join(INSTALLED).join(reference))
}

/// The host's arch as layers name it.
pub fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "aarch64" } else { "amd64" }
}

/// What a workspace gets from its layers, in the record's order: one
/// `PATH` (layers' folders, then `base`), the other variables (lists
/// joined, anything else set twice refused), and the mounts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Composed {
    pub path:   String,
    pub env:    Vec<String>,
    /// (layer reference, mount in the workspace).
    pub mounts: Vec<(String, String)>,
}

pub fn compose(layers: &[LayerManifest], base_path: &str) -> Result<Composed, String> {
    let mut path: Vec<String> = Vec::new();
    let mut vars: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut mounts: Vec<(String, String)> = Vec::new();
    for m in layers {
        if mounts.iter().any(|(_, at)| *at == m.layer.mount) {
            return Err(format!("two layers named {}: a workspace has one of each", m.layer.name));
        }
        mounts.push((m.reference(), m.layer.mount.clone()));
        path.extend(m.path_dirs());
        for (k, v) in m.env() {
            match vars.get_mut(&k) {
                Some((by, old)) if LISTS.contains(&k.as_str()) => {
                    old.push(':');
                    old.push_str(&v);
                    let _ = by;
                }
                Some((by, _)) => return Err(format!("{k} is set by both {by} and {}", m.reference())),
                None => {
                    vars.insert(k, (m.reference(), v));
                }
            }
        }
    }
    if !base_path.is_empty() {
        path.push(base_path.to_string());
    }
    Ok(Composed {
        path: path.join(":"),
        env: vars.into_iter().map(|(k, (_, v))| format!("{k}={v}")).collect(),
        mounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust() -> &'static str {
        "kind = \"layer\"\n[layer]\nname = \"rust\"\nversion = \"1.100.0\"\narch = \"aarch64\"\n\
         mount = \"/atrium/toolchains/rust\"\npath = [\"bin\"]\n\
         [layer.env]\nRUST_SRC_PATH = \"${LAYER}/lib/rustlib/src/rust/library\"\n"
    }

    fn cdev() -> String {
        "kind = \"layer\"\n[layer]\nname = \"c-dev\"\nversion = \"1\"\narch = \"aarch64\"\n\
         mount = \"/atrium/toolchains/c-dev\"\npath = [\"bin\", \"sbin\"]\n\
         [layer.env]\nLD_LIBRARY_PATH = \"${LAYER}/lib\"\nPKG_CONFIG_PATH = \"${LAYER}/libdata/pkgconfig\"\n".into()
    }

    #[test]
    fn a_layer_parses_and_composes() {
        let r = LayerManifest::from_str(rust()).unwrap();
        let c = LayerManifest::from_str(&cdev()).unwrap();
        assert_eq!(r.reference(), "rust@1.100.0");
        let w = compose(&[r, c], "/usr/bin:/bin").unwrap();
        assert_eq!(w.path, "/atrium/toolchains/rust/bin:/atrium/toolchains/c-dev/bin:/atrium/toolchains/c-dev/sbin:/usr/bin:/bin");
        assert!(w.env.contains(&"RUST_SRC_PATH=/atrium/toolchains/rust/lib/rustlib/src/rust/library".to_string()));
        assert!(w.env.contains(&"LD_LIBRARY_PATH=/atrium/toolchains/c-dev/lib".to_string()));
        assert_eq!(w.mounts.len(), 2);
    }

    #[test]
    fn what_a_layer_may_not_say() {
        let bad = |from: &str, to: &str| {
            let t = rust().replace(from, to);
            assert!(LayerManifest::from_str(&t).is_err(), "{to}");
        };
        bad("kind = \"layer\"", "kind = \"app\"");
        bad("/atrium/toolchains/rust\"\n", "/usr/local\"\n");
        bad("/atrium/toolchains/rust\"\n", "/atrium/toolchains/other\"\n");
        bad("[\"bin\"]", "[\"../bin\"]");
        bad("[\"bin\"]", "[\"/bin\"]");
        bad("arch = \"aarch64\"", "arch = \"riscv64\"");
        bad("RUST_SRC_PATH", "LD_PRELOAD");
        bad("RUST_SRC_PATH", "PATH");
        bad("RUST_SRC_PATH = \"${LAYER}/lib/rustlib/src/rust/library\"", "LD_LIBRARY_PATH = \"/usr/lib\"");
        bad("RUST_SRC_PATH = \"${LAYER}/lib/rustlib/src/rust/library\"", "MANPATH = \"${LAYER}/../x\"");
        bad("RUST_SRC_PATH = \"${LAYER}/lib/rustlib/src/rust/library\"", "X = \"a\\nb\"");
        bad("version = \"1.100.0\"", "version = \"1 0\"");
        bad("name = \"rust\"", "name = \"Rust\"");
        bad("path = [\"bin\"]", "path = [\"bin\"]\nsetuid = true");
        bad("path = [\"bin\"]", "path = [\"bin\"]\ncontent = \"sha256:12\"");
        let ok = rust().replace("path = [\"bin\"]", &format!("path = [\"bin\"]\ncontent = \"sha256:{}\"", "ab".repeat(32)));
        assert_eq!(LayerManifest::from_str(&ok).unwrap().require_content().unwrap(), [0xab; 32]);
        assert!(LayerManifest::from_str(rust()).unwrap().require_content().is_err());
    }

    #[test]
    fn the_same_variable_from_two_layers() {
        let a = LayerManifest::from_str(&cdev()).unwrap();
        let mut b = a.clone();
        b.layer.name = "python".into();
        b.layer.mount = "/atrium/toolchains/python".into();
        let w = compose(&[a.clone(), b.clone()], "").unwrap();
        assert!(w.env.contains(&"PKG_CONFIG_PATH=/atrium/toolchains/c-dev/libdata/pkgconfig:/atrium/toolchains/python/libdata/pkgconfig".to_string()));
        b.layer.env.insert("CC".into(), "${LAYER}/bin/cc".into());
        let mut a2 = a.clone();
        a2.layer.env.insert("CC".into(), "${LAYER}/bin/clang".into());
        assert!(compose(&[a2, b], "").is_err());
        assert!(compose(&[a.clone(), a], "").is_err());
    }

    #[test]
    fn references() {
        assert_eq!(parse_ref("rust@1.100.0"), Some(("rust", "1.100.0")));
        assert_eq!(parse_ref("c-dev@1"), Some(("c-dev", "1")));
        for bad in ["rust", "@1", "rust@", "../x@1", "rust@../1", "rust@1/2"] {
            assert!(parse_ref(bad).is_none(), "{bad}");
        }
    }
}
