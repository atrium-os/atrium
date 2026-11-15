//! An app's own icon, shipped in its bundle (forum.md §6.1).
//!
//! `[app] icon` is either a name from the system icon set (`"compass"`) or a
//! path inside the bundle to an SVG (`"icons/app.svg"`). This module tells the
//! two apart and reads the file, refusing anything that could leave the bundle:
//! the reader (portcullisd) runs as root and hands the bytes to a jailed
//! launcher, so a symlink to `/etc/master.passwd` must never be followed.

use std::io::Read;
use std::path::{Component, Path};

/// Largest bundle icon read. An icon is a few shapes; this is generous.
pub const MAX_ICON_BYTES: u64 = 32 * 1024;

/// `icon` names a file in the bundle (rather than the system set): it ends in
/// `.svg` and is a plain relative path of ordinary names.
pub fn is_bundle_path(icon: &str) -> bool {
    icon.len() <= 256
        && icon.to_ascii_lowercase().ends_with(".svg")
        && !icon.contains('\0')
        && Path::new(icon).components().all(|c| match c {
            Component::Normal(s) => s.to_str().is_some_and(|s| !s.starts_with('.')),
            _ => false,
        })
}

/// The SVG text of the bundle icon `icon` in `bundle_dir`, or `None` when
/// `icon` is not a bundle path, the file is missing, is not a regular file,
/// resolves outside the bundle (symlinks), is larger than `MAX_ICON_BYTES`,
/// or is not UTF-8.
pub fn read_bundle_svg(bundle_dir: &Path, icon: &str) -> Option<String> {
    if !is_bundle_path(icon) {
        return None;
    }
    let root = bundle_dir.canonicalize().ok()?;
    let file = root.join(icon).canonicalize().ok()?;
    if !file.starts_with(&root) {
        return None;
    }
    let f = std::fs::File::open(&file).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_ICON_BYTES {
        return None;
    }
    let mut text = String::new();
    f.take(MAX_ICON_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_paths_and_names() {
        assert!(is_bundle_path("icons/app.svg"));
        assert!(is_bundle_path("App.SVG"));
        assert!(!is_bundle_path("compass"));
        assert!(!is_bundle_path("/etc/icon.svg"));
        assert!(!is_bundle_path("../other/icon.svg"));
        assert!(!is_bundle_path("icons/../../x.svg"));
        assert!(!is_bundle_path("./icon.svg"));
        assert!(!is_bundle_path(".hidden/icon.svg"));
        assert!(!is_bundle_path("icon.png"));
    }

    #[test]
    fn reads_inside_the_bundle_only() {
        let d = std::env::temp_dir().join(format!("pt-icon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("b/icons")).unwrap();
        std::fs::write(d.join("b/icons/app.svg"), "<svg/>").unwrap();
        std::fs::write(d.join("secret.svg"), "secret").unwrap();
        std::os::unix::fs::symlink(d.join("secret.svg"), d.join("b/icons/out.svg")).unwrap();
        std::fs::write(d.join("b/big.svg"), vec![b' '; MAX_ICON_BYTES as usize + 1]).unwrap();
        std::fs::create_dir_all(d.join("b/dir.svg")).unwrap();
        let b = d.join("b");
        assert_eq!(read_bundle_svg(&b, "icons/app.svg").as_deref(), Some("<svg/>"));
        assert_eq!(read_bundle_svg(&b, "icons/out.svg"), None, "symlink out of the bundle");
        assert_eq!(read_bundle_svg(&b, "big.svg"), None, "too large");
        assert_eq!(read_bundle_svg(&b, "dir.svg"), None, "not a file");
        assert_eq!(read_bundle_svg(&b, "missing.svg"), None);
        assert_eq!(read_bundle_svg(&b, "compass"), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
