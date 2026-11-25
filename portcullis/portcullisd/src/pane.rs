//! An app's settings-pane data (curia.md §15.1): the one folder of the app's
//! that its pane in Settings works on, kept outside the app's overlay.
//!
//! The overlay's upper layer is the app uid's from its root, and the next
//! launch unions it over the signed bundle, so a pane that could write
//! there could replace or hide the app's own code. The folder the manifest
//! names in `[roles.app-settings] data` lives instead in the persona's key
//! group, `~/.atrium/apps/.pane-data/<app>` (or `persona-apps/<p>/…`),
//! under root-owned parents, and is mounted at that path in the app's jail
//! and at `/app-data` in the pane's. jaild mounts it only into a jail that
//! runs as the folder's own uid (`rw_owned_patterns`).

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

const PANE_DATA: &str = ".pane-data";

fn directory(path: &Path, uid: u32, mode: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => std::os::unix::fs::chown(path, Some(uid), Some(uid))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o777 != mode {
        return Err(io::Error::other(format!("unexpected owner or mode on {}", path.display())));
    }
    Ok(())
}

/// The folder for `app_id`'s pane data in `apps` (the person's apps root,
/// or a named persona's): made if missing, `apps` and `.pane-data` root's
/// and closed to others, the folder `app_uid`'s 0700.
pub fn folder_in(apps: &Path, app_id: &str, app_uid: u32) -> io::Result<PathBuf> {
    if !portcullis_overlay::plain_segment(app_id) || app_id.starts_with('.') || app_uid == 0 {
        return Err(io::Error::other("not an app's folder"));
    }
    let m = fs::symlink_metadata(apps)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!("{} is not root's alone", apps.display())));
    }
    let parent = apps.join(PANE_DATA);
    directory(&parent, 0, 0o700)?;
    let dir = parent.join(app_id);
    directory(&dir, app_uid, 0o700)?;
    Ok(dir)
}

/// [`folder_in`] for `person`'s instance of `app_id` in `persona`.
pub fn folder(person: &str, persona: Option<&str>, app_id: &str, app_uid: u32) -> io::Result<PathBuf> {
    let home = portcullis_overlay::home_of(person).ok_or_else(|| io::Error::other(format!("{person} has no home")))?;
    let home = fs::canonicalize(home)?;
    let apps = match persona {
        None => portcullis_overlay::apps_root(&home).map_err(io::Error::other)?,
        Some(p) if portcullis_peer::valid_person_persona(p) => portcullis_overlay::persona_apps_dir(&home, p),
        Some(_) => return Err(io::Error::other("not a persona")),
    };
    folder_in(&apps, app_id, app_uid)
}

/// The bundle has nothing at `data` (or a link on the way to it): the
/// folder mounted there hides none of the app's signed files.
pub fn bundle_has_none(tree: &Path, data: &str) -> io::Result<()> {
    let mut at = tree.to_path_buf();
    let parts: Vec<&str> = data.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
            Ok(m) if i + 1 < parts.len() && m.is_dir() => {}
            Ok(_) => return Err(io::Error::other(format!("the bundle has {}", parts[..=i].join("/")))),
        }
    }
    Err(io::Error::other(format!("the bundle has {data}")))
}

/// Make `data`'s folders in a jail's root, refusing a link on the way: the
/// app's overlay is its own to fill.
pub fn mountpoint(root: &Path, data: &str) -> io::Result<PathBuf> {
    let mut path = root.to_owned();
    for part in data.split('/') {
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(io::Error::other(format!("{} is not a folder", path.display())));
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pane-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_bundle_may_have_nothing_where_the_data_goes() {
        let t = tmp("bundle");
        fs::create_dir_all(t.join("var/db")).unwrap();
        fs::create_dir_all(t.join("bin")).unwrap();
        fs::write(t.join("bin/app"), b"x").unwrap();
        assert!(bundle_has_none(&t, "var/db/app").is_ok(), "folders on the way are fine");
        assert!(bundle_has_none(&t, "srv/app").is_ok());
        assert!(bundle_has_none(&t, "var/db").is_err(), "the bundle's own folder");
        assert!(bundle_has_none(&t, "bin/app/x").is_err(), "a file on the way");
        std::os::unix::fs::symlink("/", t.join("lnk")).unwrap();
        assert!(bundle_has_none(&t, "lnk/x").is_err(), "a link on the way");
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn a_mountpoint_refuses_a_link_the_app_made() {
        let t = tmp("mp");
        assert_eq!(mountpoint(&t, "var/db/app").unwrap(), t.join("var/db/app"));
        fs::create_dir_all(t.join("x")).unwrap();
        std::os::unix::fs::symlink(t.join("x"), t.join("lnk")).unwrap();
        assert!(mountpoint(&t, "lnk/app").is_err());
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn the_folder_needs_a_root_owned_parent_and_an_app() {
        let t = tmp("folder");
        // Not root's (the test runs unprivileged, or this is root's own temp).
        if fs::symlink_metadata(&t).unwrap().uid() != 0 {
            assert!(folder_in(&t, "org.example.app", 5000).is_err());
        }
        assert!(folder_in(&t, "..", 5000).is_err());
        assert!(folder_in(&t, ".pane-data", 5000).is_err());
        assert!(folder_in(&t, "org.example.app", 0).is_err(), "never root's");
        let _ = fs::remove_dir_all(&t);
    }
}
