//! Insula bundle reader.
//!
//! An on-disk Insula bundle is the directory shipped
//! to a user / installed by Opifex. Per
//! `docs/spec/insula.md` §3.1:
//!
//! ```text
//! my-app/
//!   manifest.toml         # parsed by insula-manifest
//!   signature             # COSE_Sign1 (future; v0 unsigned)
//!   bin/
//!     my-app              # native ELF / Mach-O, or .wasm
//!   assets/
//!     …                   # opaque to the bundle reader
//! ```
//!
//! This crate is platform-neutral — bundle layout is
//! the same on macOS, Linux, Windows, and Atrium. Host
//! adapters consume it to derive their per-OS install
//! plan (App Sandbox profile, launchd plist, etc.).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use insula_manifest::Manifest;
use std::path::{Path, PathBuf};

mod error;
pub mod archive;
pub mod signing;

pub use error::Error;

/// A validated Insula bundle on disk.
///
/// Constructed via [`InsulaBundle::read`], which
/// performs the v0 validation:
///
/// - `manifest.toml` exists at the bundle root and
///   parses.
/// - The binary referenced by `bundle.entry` exists.
/// - For `form = "native"`, the binary file is at
///   `<root>/<entry>`. For `form = "wasm"`, same path
///   but the file is a WASM module.
#[derive(Debug, Clone)]
pub struct InsulaBundle {
    /// Absolute path to the bundle root directory.
    pub root: PathBuf,

    /// Parsed manifest.
    pub manifest: Manifest,
}

impl InsulaBundle {
    /// Read + validate a bundle from disk.
    pub fn read(root: impl AsRef<Path>) -> Result<Self, Error> {
        let root = root.as_ref();

        if !root.is_dir() {
            return Err(Error::BundleRootNotADir(root.to_path_buf()));
        }

        let manifest_path = root.join("manifest.toml");
        let manifest_src = std::fs::read_to_string(&manifest_path)
            .map_err(|e| Error::ManifestRead {
                path: manifest_path.clone(),
                source: e,
            })?;

        let manifest = Manifest::parse(&manifest_src)
            .map_err(Error::ManifestParse)?;

        // Validate the binary exists.
        let bin_path = root.join(&manifest.manifest_entry_path());
        if !bin_path.is_file() {
            return Err(Error::BinaryMissing {
                expected: bin_path,
                entry: manifest.manifest_entry_path().to_string(),
            });
        }

        Ok(InsulaBundle {
            root: root.to_path_buf(),
            manifest,
        })
    }

    /// Absolute path to the bundle's entry binary.
    pub fn binary_path(&self) -> PathBuf {
        self.root.join(self.manifest.manifest_entry_path())
    }

    /// The app's canonical identifier
    /// (`manifest.app.name`).
    pub fn app_id(&self) -> &str {
        &self.manifest.app.name
    }
}

/// Small extension trait providing the entry path on
/// `Manifest`. Lives here (not in `insula-manifest`)
/// because the manifest crate doesn't know about
/// bundle directory semantics — entry is a
/// bundle-relative path that only has meaning when a
/// bundle root is available.
trait ManifestEntryExt {
    fn manifest_entry_path(&self) -> &str;
}

impl ManifestEntryExt for Manifest {
    fn manifest_entry_path(&self) -> &str {
        &self.bundle.entry
    }
}
