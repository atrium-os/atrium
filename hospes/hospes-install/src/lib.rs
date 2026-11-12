//! Installing a foreign library for Hospes (docs/spec/hospes.md §12, §14 decision 5).
//!
//! Widevine comes one of two ways, both user-initiated:
//!
//! - **The user's own file** ([`check_library`] then [`install`]): any `libwidevinecdm.so`
//!   the user supplies, with its `manifest.json` if they have it.
//! - **Google's Chrome for Linux package** ([`verify_apt`], [`extract_cdm`]): the package
//!   repository's `InRelease` is checked against Google's Linux package signing key (shipped
//!   here, its primary fingerprint pinned), the `Packages` index against the hash the signed
//!   release lists, and the `.deb` against the hash the index lists. Only the CDM and its
//!   manifest are taken out of it. Atrium never runs anything from the package and never
//!   presents itself to Google's component updater.
//!
//! Either way the library must be the machine's ELF, export the CDM entry points and import
//! nothing the glibc shim does not provide ([`check_library`]): a CDM that needs more is
//! refused here, not at load. Fetching the three files is the caller's (through Nuntius in
//! the system; `curl` in tests).
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

pub mod apt;

/// Google's Linux package signing key (https://dl.google.com/linux/linux_signing_key.pub).
pub const GOOGLE_KEY: &str = include_str!("../keys/google-linux-packages.asc");
/// Its primary key's fingerprint, which the key file must carry.
pub const GOOGLE_FINGERPRINT: &str = "EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796";

#[derive(Debug)]
pub enum Error {
    Io(String),
    /// The repository's signature, an index or the package did not check out.
    Verify(String),
    /// The package or file is not what it should be.
    Package(String),
    /// The library is not one Hospes can host.
    Library(String),
    /// An older version than the one installed.
    Downgrade { have: String, offered: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(m) => write!(f, "{m}"),
            Error::Verify(m) => write!(f, "not verified: {m}"),
            Error::Package(m) => write!(f, "package: {m}"),
            Error::Library(m) => write!(f, "library: {m}"),
            Error::Downgrade { have, offered } => write!(f, "{offered} is older than the installed {have}"),
        }
    }
}

impl std::error::Error for Error {}

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// The Debian architecture and Chrome's platform directory for this machine.
pub fn platform() -> (&'static str, &'static str) {
    if cfg!(target_arch = "aarch64") {
        ("arm64", "linux_arm64")
    } else {
        ("amd64", "linux_x64")
    }
}

/// The CDM entry points (content_decryption_module.h) the library must export.
pub const ENTRY_POINTS: [&str; 4] = ["InitializeCdmModule_4", "CreateCdmInstance", "GetCdmVersion", "DeinitializeCdmModule"];

/// What [`check_library`] found.
#[derive(Debug, PartialEq)]
pub struct LibraryInfo {
    pub imports: usize,
    pub sha256: String,
}

/// The machine's ELF shared object, the CDM entry points exported, and every strong import
/// provided by the glibc shim by (name, version).
pub fn check_library(bytes: &[u8]) -> Result<LibraryInfo, Error> {
    let img = parse_library(bytes)?;
    for ep in ENTRY_POINTS {
        if img.export(ep, None).is_none() {
            return Err(Error::Library(format!("no {ep}: not a CDM")));
        }
    }
    imports_provided(&img, bytes)
}

/// Any library Hospes hosts (the CDM or OpenH264): the machine's ELF shared object, and every
/// strong import provided by the glibc shim. The version watch (spec §11.4) runs this.
pub fn check_imports(bytes: &[u8]) -> Result<LibraryInfo, Error> {
    imports_provided(&parse_library(bytes)?, bytes)
}

fn parse_library(bytes: &[u8]) -> Result<hospes_elf::Image, Error> {
    let machine = if cfg!(target_arch = "aarch64") { hospes_elf::Machine::Aarch64 } else { hospes_elf::Machine::X86_64 };
    hospes_elf::parse(bytes, Some(machine)).map_err(|e| Error::Library(format!("not this machine's ELF library: {e:?}")))
}

fn imports_provided(img: &hospes_elf::Image, bytes: &[u8]) -> Result<LibraryInfo, Error> {
    let have: std::collections::HashSet<(&str, Option<&str>)> = hospes_glibc::provided().collect();
    let mut missing = Vec::new();
    let mut n = 0;
    for (_, s) in img.imports() {
        n += 1;
        if s.bind == hospes_elf::Bind::Weak || have.contains(&(s.name.as_str(), s.version.as_deref())) {
            continue;
        }
        missing.push(match &s.version {
            Some(v) => format!("{}@{v}", s.name),
            None => s.name.clone(),
        });
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(Error::Library(format!("imports Hospes does not provide yet: {}", missing.join(", "))));
    }
    Ok(LibraryInfo { imports: n, sha256: sha256_hex(bytes) })
}

/// The CDM and its manifest, out of a Chrome `.deb` (its `data.tar.xz`).
pub struct Extracted {
    pub library: Vec<u8>,
    pub manifest: Vec<u8>,
}

/// Largest member we take out of the package (the CDM is about 20 MiB).
const MEMBER_MAX: u64 = 64 << 20;

/// Largest `data.tar` we unpack in memory (Chrome's is about 400 MiB).
const DATA_TAR_MAX: usize = 1 << 30;

/// A `Write` into memory that refuses to grow past its limit (a hostile `.xz` cannot make
/// the installer allocate without bound).
struct Capped(Vec<u8>, usize);

impl std::io::Write for Capped {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + b.len() > self.1 {
            return Err(std::io::Error::other("data.tar larger than the limit"));
        }
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The members of a `.deb` (a common `ar` archive): (name, contents). Only what dpkg
/// writes: short names, optionally ending in `/`; anything else is refused. Our own parser,
/// since the `ar` crate panics on a hostile GNU name-table index (found by the H5 fuzzer).
pub fn ar_members(deb: &[u8]) -> Result<Vec<(&str, &[u8])>, Error> {
    let bad = |m: &str| Error::Package(format!("not a .deb: {m}"));
    let mut rest = deb.strip_prefix(b"!<arch>\n").ok_or_else(|| bad("no ar magic"))?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        if rest.len() < 60 {
            return Err(bad("short member header"));
        }
        let (h, body) = rest.split_at(60);
        if &h[58..60] != b"`\n" {
            return Err(bad("bad member header"));
        }
        let name = std::str::from_utf8(&h[..16]).map_err(|_| bad("member name not text"))?.trim_end_matches(' ');
        let name = name.strip_suffix('/').unwrap_or(name);
        if name.is_empty() || name.contains('/') || name.starts_with('#') {
            return Err(bad("member name"));
        }
        let size: usize = std::str::from_utf8(&h[48..58]).ok().and_then(|t| t.trim_end_matches(' ').parse().ok()).ok_or_else(|| bad("member size"))?;
        if size > body.len() {
            return Err(bad("member runs past the end"));
        }
        out.push((name, &body[..size]));
        // Members start on even offsets.
        rest = &body[(size + (size & 1)).min(body.len())..];
    }
    Ok(out)
}

pub fn extract_cdm(deb: &[u8]) -> Result<Extracted, Error> {
    let data = ar_members(deb)?.into_iter().find(|(n, _)| *n == "data.tar.xz").map(|(_, d)| d);
    let data = data.ok_or_else(|| Error::Package("no data.tar.xz".into()))?;
    let mut tar_bytes = Capped(Vec::new(), DATA_TAR_MAX);
    lzma_rs::xz_decompress(&mut &data[..], &mut tar_bytes).map_err(|e| Error::Package(format!("data.tar.xz: {e:?}")))?;
    let tar_bytes = tar_bytes.0;
    let (_, plat) = platform();
    let want_lib = format!("opt/google/chrome/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so");
    let want_manifest = "opt/google/chrome/WidevineCdm/manifest.json";
    let (mut library, mut manifest) = (None, None);
    let mut t = tar::Archive::new(&tar_bytes[..]);
    for e in t.entries().map_err(|e| Error::Package(e.to_string()))? {
        let mut e = e.map_err(|e| Error::Package(e.to_string()))?;
        let path = e.path().map_err(|e| Error::Package(e.to_string()))?.to_string_lossy().trim_start_matches("./").to_string();
        let slot = if path == want_lib {
            &mut library
        } else if path == want_manifest {
            &mut manifest
        } else {
            continue;
        };
        if e.header().entry_type() != tar::EntryType::Regular || e.size() > MEMBER_MAX {
            return Err(Error::Package(format!("{path}: not a plain file of sensible size")));
        }
        let mut v = Vec::new();
        e.read_to_end(&mut v).map_err(|e| Error::Package(e.to_string()))?;
        *slot = Some(v);
    }
    Ok(Extracted {
        library: library.ok_or_else(|| Error::Package(format!("no {want_lib} in the package")))?,
        manifest: manifest.ok_or_else(|| Error::Package(format!("no {want_manifest} in the package")))?,
    })
}

/// The `"version"` of a CDM manifest (`manifest.json`), if it has a sane one.
pub fn manifest_version(manifest: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(manifest).ok()?;
    let at = s.find("\"version\"")? + 9;
    let rest = s[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let v = &rest[..rest.find('"')?];
    (!v.is_empty() && v.len() <= 32 && v.chars().all(|c| c.is_ascii_digit() || c == '.')).then(|| v.to_string())
}

/// Compare dotted versions numerically.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let p = |s: &str| s.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a).cmp(&p(b))
}

/// Install into `dest/<version>/` (`libwidevinecdm.so`, `manifest.json` if any, `SHA256`),
/// read-only, written beside and renamed into place, and point `dest/current` at it.
/// Refuses a version older than the current one unless `allow_downgrade`.
pub fn install(dest: &Path, library: &[u8], manifest: Option<&[u8]>, allow_downgrade: bool) -> Result<PathBuf, Error> {
    let info = check_library(library)?;
    let version = manifest.and_then(manifest_version).unwrap_or_else(|| format!("unversioned-{}", &info.sha256[..12]));
    let io = |e: std::io::Error| Error::Io(e.to_string());
    std::fs::create_dir_all(dest).map_err(io)?;
    let current = dest.join("current");
    if let Ok(have) = std::fs::read_link(&current) {
        let have = have.to_string_lossy().to_string();
        if !allow_downgrade && !have.starts_with("unversioned") && !version.starts_with("unversioned") && version_cmp(&version, &have).is_lt() {
            return Err(Error::Downgrade { have, offered: version });
        }
    }
    let dir = dest.join(&version);
    let tmp = dest.join(format!(".{version}.partial"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir(&tmp).map_err(io)?;
    let put = |name: &str, b: &[u8]| -> Result<(), Error> {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp.join(name);
        std::fs::write(&p, b).map_err(io)?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).map_err(io)
    };
    put("libwidevinecdm.so", library)?;
    if let Some(m) = manifest {
        put("manifest.json", m)?;
    }
    put("SHA256", format!("{}  libwidevinecdm.so\n", info.sha256).as_bytes())?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir).map_err(io)?;
    let link_tmp = dest.join(".current.partial");
    let _ = std::fs::remove_file(&link_tmp);
    std::os::unix::fs::symlink(&version, &link_tmp).map_err(io)?;
    std::fs::rename(&link_tmp, &current).map_err(io)?;
    Ok(dir)
}
