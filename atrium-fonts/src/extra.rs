//! Installed font packages (docs/spec/opifex.md §8.1, fonts.md §6).
//!
//! `opifexd` installs a `kind = "font"` package into
//! `/usr/share/fonts/atrium-extra/<id>/` and rewrites the root-owned pin
//! list `/usr/share/fonts/atrium-extra/pins` atomically. The library reads
//! that list, never the fonts' own name tables, and maps an installed face
//! only when its file's BLAKE3 matches its line, as with the faces compiled
//! into `catalog.rs`.
//!
//! The pin list, UTF-8, one face a line, fields separated by one tab:
//!
//! ```text
//! # atrium-fonts pins 1
//! <package> <family> <file> <weight> <italic> <ranges> <address> <size>
//! ```
//!
//! - `package`: the package id, `[a-z0-9.-]`, at most 64 bytes.
//! - `family`: at most 64 bytes, no control characters.
//! - `file`: `<package>/<name>.ttf`, relative to the folder; `name` is
//!   `[A-Za-z0-9._-]`, at most 128 bytes, and not `.` or `..`.
//! - `weight`: 1 to 1000. `italic`: `0` or `1`.
//! - `ranges`: the code points the face is for, `4E00-9FFF,3040-309F`
//!   (hex, at most 64 ranges); what fallback looks it up by.
//! - `address`: `blake3:<64 hex>`. `size`: the file's length in bytes.
//!
//! The first line must be the header. A line that breaks a rule is skipped,
//! the rest still count; at most [`MAX_FACES`] lines are read. Faces keep
//! their ids for the life of the process: a newer list only adds faces
//! (an uninstalled face's file is gone, so it fails its check and is not
//! drawn).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use crate::FaceInfo;

/// The installed packages' folder.
pub const SYSTEM_EXTRA: &str = "/usr/share/fonts/atrium-extra";
pub const HEADER: &str = "# atrium-fonts pins 1";
pub const MAX_FACES: usize = 4096;
const MAX_PINS_BYTES: u64 = 1 << 20;
const MAX_RANGES: usize = 64;

/// An installed face's extra facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub package: &'static str,
    pub ranges: &'static [(u32, u32)],
}

struct Faces {
    all: Vec<FaceInfo>,
    /// For `all[crate::FACES.len()..]`.
    installed: Vec<Installed>,
}

static CURRENT: AtomicPtr<Faces> = AtomicPtr::new(std::ptr::null_mut());

struct Seen {
    stamp: Option<(SystemTime, u64)>,
    at: Option<Instant>,
}
static SEEN: Mutex<Seen> = Mutex::new(Seen { stamp: None, at: None });

/// The installed packages' folder: `ATRIUM_FONTS_EXTRA_DIR` when set, else
/// the system's.
pub fn dir() -> PathBuf {
    std::env::var_os("ATRIUM_FONTS_EXTRA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(SYSTEM_EXTRA))
}

fn current() -> &'static Faces {
    let p = CURRENT.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: set only by `publish`, to a leaked box never freed.
        return unsafe { &*p };
    }
    reload(true);
    // SAFETY: as above; `reload(true)` always publishes.
    unsafe { &*CURRENT.load(Ordering::Acquire) }
}

/// Every face: the shipped ones (`FACES`, same ids), then the installed.
pub fn faces() -> &'static [FaceInfo] {
    &current().all
}

/// The installed face's package and code points; `None` for a shipped face.
pub fn installed(id: usize) -> Option<&'static Installed> {
    current().installed.get(id.checked_sub(crate::FACES.len())?)
}

/// Read the pin list again if it changed, at most once a second; true when
/// faces were added. For a long-running process (frescod) on a character
/// no face has: an install shows up without a restart.
pub fn refresh() -> bool {
    reload(false)
}

fn reload(first: bool) -> bool {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !first {
        if seen.at.is_some_and(|t| t.elapsed().as_secs() < 1) {
            return false;
        }
        seen.at = Some(Instant::now());
    } else if !CURRENT.load(Ordering::Acquire).is_null() {
        return false; // another thread's first load won
    }
    let path = dir().join("pins");
    let stamp = std::fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
    if !first && stamp == seen.stamp {
        return false;
    }
    seen.stamp = stamp;
    let text = match stamp {
        Some((_, len)) if len <= MAX_PINS_BYTES => std::fs::read_to_string(&path).unwrap_or_default(),
        _ => String::new(),
    };
    let old = CURRENT.load(Ordering::Acquire);
    // SAFETY: as in `current`.
    let base: Option<&Faces> = if old.is_null() { None } else { Some(unsafe { &*old }) };
    let mut next = Faces {
        all: base.map_or_else(|| crate::FACES.to_vec(), |b| b.all.clone()),
        installed: base.map_or_else(Vec::new, |b| b.installed.clone()),
    };
    let mut added = false;
    for (face, inst) in parse(&text) {
        if next.all.iter().any(|f| f.address == face.address && f.file == face.file) || next.installed.len() >= MAX_FACES {
            continue;
        }
        next.all.push(face);
        next.installed.push(inst);
        added = true;
    }
    if first || added {
        // The old list stays alive (leaked): callers may still hold it.
        CURRENT.store(Box::into_raw(Box::new(next)), Ordering::Release);
    }
    added
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

fn package_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

fn file_ok(file: &str, package: &str) -> bool {
    let Some((pkg, name)) = file.split_once('/') else { return false };
    pkg == package
        && !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name.ends_with(".ttf")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn ranges(s: &str) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for r in s.split(',') {
        let (lo, hi) = r.split_once('-').unwrap_or((r, r));
        let (lo, hi) = (u32::from_str_radix(lo, 16).ok()?, u32::from_str_radix(hi, 16).ok()?);
        if lo > hi || hi > 0x10FFFF || out.len() >= MAX_RANGES {
            return None;
        }
        out.push((lo, hi));
    }
    (!out.is_empty()).then_some(out)
}

/// The faces a pin list names, the lines that break a rule left out.
pub fn parse(text: &str) -> Vec<(FaceInfo, Installed)> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Vec::new();
    }
    lines
        .take(MAX_FACES)
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [package, family, file, weight, italic, rs, address, size] = f[..] else { return None };
            if !package_ok(package) || family.is_empty() || family.len() > 64 || family.chars().any(char::is_control) || !file_ok(file, package) {
                return None;
            }
            let weight: u16 = weight.parse().ok().filter(|w| (1..=1000).contains(w))?;
            let italic = match italic {
                "0" => false,
                "1" => true,
                _ => return None,
            };
            let hex = address.strip_prefix("blake3:")?;
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                return None;
            }
            let size: u64 = size.parse().ok()?;
            let rs = ranges(rs)?;
            Some((
                FaceInfo { family: leak(family), file: leak(file), weight, italic, address: leak(address), size },
                Installed { package: leak(package), ranges: Box::leak(rs.into_boxed_slice()) },
            ))
        })
        .collect()
}

/// Families of installed faces for `ch`, in install order.
pub fn families_for(ch: char) -> impl Iterator<Item = &'static str> {
    let c = ch as u32;
    let base = crate::FACES.len();
    let all = faces();
    let mut seen: Vec<&'static str> = Vec::new();
    current().installed.iter().enumerate().filter_map(move |(i, inst)| {
        let fam = all[base + i].family;
        (inst.ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c)) && !seen.contains(&fam)).then(|| {
            seen.push(fam);
            fam
        })
    })
}

/// Where an installed face's file is.
pub fn path(file: &str) -> PathBuf {
    dir().join(Path::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn pin_lines_are_checked() {
        let ok = format!("noto-cjk-jp\tNoto Sans JP\tnoto-cjk-jp/NotoSansJP-Regular.ttf\t400\t0\t3040-30FF,4E00-9FFF\t{A}\t123");
        let text = [
            HEADER.to_string(),
            ok.clone(),
            ok.replace("noto-cjk-jp/", "../"),
            ok.replace("noto-cjk-jp/NotoSansJP-Regular.ttf", "other/NotoSansJP-Regular.ttf"),
            ok.replace(".ttf", ".otf"),
            ok.replace("\t400\t", "\t0\t"),
            ok.replace("\t0\t3040", "\tyes\t3040"),
            ok.replace("3040-30FF", "30FF-3040"),
            ok.replace("blake3:", "sha256:"),
            ok.replace("noto-cjk-jp\t", "Noto CJK\t"),
            format!("{ok}\textra"),
        ]
        .join("\n");
        let got = parse(&text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.family, "Noto Sans JP");
        assert_eq!((got[0].0.weight, got[0].0.italic, got[0].0.size), (400, false, 123));
        assert_eq!(got[0].1.ranges, &[(0x3040, 0x30FF), (0x4E00, 0x9FFF)]);
        assert!(parse(&ok).is_empty(), "no header");
    }
}
