//! `cargo atrium icon` (dev-tools.md §3.5): check an SVG against the dock's
//! limits, and import one drawn elsewhere into `assets/icons/<name>.svg`.
//! The work is atrium-icon's; this reads the file (bounded), picks the
//! destination and writes it.

use std::path::{Path, PathBuf};

/// The largest source read: an exported SVG carries editor data the import
/// drops, so this is well past the dock's 32 KB.
pub const MAX_SOURCE: u64 = 4 << 20;

/// Where imported icons go in a project.
pub const ICON_DIR: &str = "assets/icons";

/// Read an SVG of at most [`MAX_SOURCE`] bytes.
pub fn read_source(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    f.take(MAX_SOURCE + 1).read_to_end(&mut bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_SOURCE {
        return Err(format!("{} is larger than {} MB", path.display(), MAX_SOURCE >> 20));
    }
    Ok(bytes)
}

/// An icon's name: 1 to 48 of `a-z 0-9 -`, not starting with `-`.
pub fn valid_name(n: &str) -> bool {
    (1..=48).contains(&n.len()) && !n.starts_with('-')
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A name from the source's file name: lowercased, other characters as `-`.
pub fn name_from(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("icon");
    let mut n: String = stem.chars().map(|c| {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }
    }).collect();
    while n.contains("--") { n = n.replace("--", "-") }
    let n = n.trim_matches('-').chars().take(48).collect::<String>();
    let n = n.trim_end_matches('-').to_string();
    if n.is_empty() { "icon".into() } else { n }
}

/// What `icon import` would write.
pub struct Plan {
    pub import: atrium_icon::Import,
    /// Project-relative, e.g. `assets/icons/app.svg`.
    pub path: String,
    pub replaces: bool,
}

/// Plan importing `src` as `name` (from the file name when None) in `proj`.
pub fn plan(proj: &Path, src: &Path, name: Option<&str>) -> Result<Plan, String> {
    let name = name.map(str::to_string).unwrap_or_else(|| name_from(src));
    if !valid_name(&name) {
        return Err(format!("{name:?} isn't an icon name: 1 to 48 of a-z, 0-9 and -"));
    }
    let import = atrium_icon::import(&read_source(src)?)?;
    let path = format!("{ICON_DIR}/{name}.svg");
    let replaces = proj.join(&path).exists();
    Ok(Plan { import, path, replaces })
}

/// Write the plan's icon (only one the dock accepts).
pub fn apply(proj: &Path, p: &Plan) -> Result<PathBuf, String> {
    if let Some(why) = &p.import.report.problem {
        return Err(format!("the result still isn't an icon the dock draws: {why}"));
    }
    let dest = proj.join(&p.path);
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    // Through a fresh file beside it, so a reader never sees half an icon.
    let tmp = dest.with_extension("svg.new");
    std::fs::write(&tmp, &p.import.svg).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(dest)
}

/// A report as JSON, for Artifex.
pub fn report_json(r: &atrium_icon::Report) -> serde_json::Value {
    serde_json::json!({
        "ok": r.ok(), "shapes": r.shapes, "points": r.points, "bytes": r.bytes, "problem": r.problem,
        "limits": {
            "shapes": portcullis_toml::icon::MAX_ICON_SHAPES,
            "points": portcullis_toml::icon::MAX_ICON_POINTS,
            "bytes": portcullis_toml::icon::MAX_ICON_BYTES,
        },
    })
}

/// The files under `assets/` a bundle carries: regular files (no links)
/// whose every path part is a name atrium-assets can open, at most
/// `MAX_ASSETS` of them, sorted. A project without `assets/` has none.
pub fn asset_files(proj: &Path) -> Result<Vec<String>, String> {
    let root = proj.join("assets");
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![(root, String::from("assets"))];
    while let Some((dir, rel)) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for ent in rd {
            let ent = ent.map_err(|e| format!("{}: {e}", dir.display()))?;
            let Some(name) = ent.file_name().to_str().map(str::to_string) else {
                return Err(format!("{}: a file name that isn't UTF-8", dir.display()));
            };
            if name.starts_with('.') { continue }
            let t = ent.file_type().map_err(|e| format!("{name}: {e}"))?;
            let r = format!("{rel}/{name}");
            if !atrium_part(&name) {
                return Err(format!("{r}: only A-Z a-z 0-9 . _ - in asset names"));
            }
            if t.is_dir() {
                stack.push((ent.path(), r));
            } else if t.is_file() {
                out.push(r);
            } else {
                return Err(format!("{r}: not a file (links aren't bundled)"));
            }
            if out.len() > MAX_ASSETS {
                return Err(format!("more than {MAX_ASSETS} files in assets/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The most files a bundle's `assets/` may hold.
pub const MAX_ASSETS: usize = 4096;

fn atrium_part(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && n.len() <= 255
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cargo-atrium-icon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names() {
        assert_eq!(name_from(Path::new("/x/My Logo (final).svg")), "my-logo-final");
        assert_eq!(name_from(Path::new("___.svg")), "icon");
        assert!(valid_name("app-2") && !valid_name("-a") && !valid_name("A") && !valid_name("a/b") && !valid_name(""));
    }

    #[test]
    fn imports_into_assets_icons() {
        let d = dir("import");
        let src = d.join("Logo.svg");
        std::fs::write(&src, r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><metadata>x</metadata><rect x="2" y="2" width="20" height="20" fill="#c06030"/></svg>"##).unwrap();
        let p = plan(&d, &src, None).unwrap();
        assert_eq!(p.path, "assets/icons/logo.svg");
        assert!(!p.replaces && p.import.report.ok(), "{:?}", p.import.report.problem);
        let out = apply(&d, &p).unwrap();
        assert!(atrium_icon::check(&std::fs::read(&out).unwrap()).ok());
        assert!(plan(&d, &src, None).unwrap().replaces);
        assert!(plan(&d, &src, Some("../x")).is_err());
        assert_eq!(asset_files(&d).unwrap(), ["assets/icons/logo.svg"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_large_source_is_refused_unread() {
        let d = dir("large");
        let src = d.join("big.svg");
        std::fs::write(&src, vec![b' '; MAX_SOURCE as usize + 1]).unwrap();
        assert!(read_source(&src).unwrap_err().contains("larger than"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn assets_are_files_with_plain_names() {
        let d = dir("assets");
        std::fs::create_dir_all(d.join("assets/pics")).unwrap();
        std::fs::write(d.join("assets/pics/a.png"), b"x").unwrap();
        std::fs::write(d.join("assets/.DS_Store"), b"x").unwrap();
        assert_eq!(asset_files(&d).unwrap(), ["assets/pics/a.png"]);
        std::fs::write(d.join("assets/bad name.png"), b"x").unwrap();
        assert!(asset_files(&d).is_err());
        std::fs::remove_file(d.join("assets/bad name.png")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", d.join("assets/p")).unwrap();
            assert!(asset_files(&d).unwrap_err().contains("links"));
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
