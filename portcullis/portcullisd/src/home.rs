//! The person's home (docs/spec/homes.md §2, scrinium.md §9 H1): the
//! folders they see, the `files-<person>` group that opens them to the file
//! manager, and `~/.atrium`, root's, where app overlays and session state
//! live.
//!
//! Nothing runs as the person: the folders are theirs, group
//! `files-<person>`, mode 2770 (setgid, so what the file manager makes keeps
//! the group), and the one app given `home-files` (Scrinium) runs as its own
//! per-app uid with that group added at launch.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The folders a person sees, made at their first launch.
pub const FOLDERS: &[&str] = &["Documents", "Downloads", "Pictures", "Music", "Videos"];
/// Where the folders appear in a `home-files` jail.
pub const IN_JAIL: &str = "atrium/home";

/// The person's files group.
pub fn files_group(person: &str) -> String { format!("files-{person}") }

/// `name:*:gid:members` for a group (pw, so this module needs no unsafe).
fn group_entry(group: &str) -> Option<(u32, Vec<String>)> {
    let o = Command::new("pw").args(["groupshow", group]).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let f: Vec<&str> = line.split(':').collect();
    let gid = f.get(2)?.parse().ok()?;
    let members = f.get(3).map_or(vec![], |m| m.split(',').filter(|s| !s.is_empty()).map(String::from).collect());
    Some((gid, members))
}

fn gid_of(group: &str) -> Option<u32> { group_entry(group).map(|g| g.0) }

fn uid_of(person: &str) -> Option<u32> {
    let o = Command::new("id").args(["-u", person]).output().ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8_lossy(&o.stdout).trim().parse().ok()
}

fn pw(args: &[&str]) -> Result<(), String> {
    let o = Command::new("pw").args(args).output().map_err(|e| format!("pw {}: {e}", args.join(" ")))?;
    if o.status.success() { Ok(()) } else { Err(format!("pw {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim())) }
}

/// Make the person's folders, group and `~/.atrium` if missing, and put
/// right a folder whose group or mode drifted. Returns the home and the
/// group's id. A folder that is a link, or someone else's, is refused.
pub fn ensure_person(person: &str) -> Result<(PathBuf, u32), String> {
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    let uid = uid_of(person).ok_or_else(|| format!("no such person {person}"))?;
    let group = files_group(person);
    let gid = match gid_of(&group) {
        Some(g) => g,
        None => {
            pw(&["groupadd", &group])?;
            gid_of(&group).ok_or_else(|| format!("{group} was not made"))?
        }
    };
    portcullis_overlay::apps_root(&home)?;
    for f in FOLDERS {
        let d = home.join(f);
        match std::fs::symlink_metadata(&d) {
            Ok(m) if !m.is_dir() => return Err(format!("{} is not a folder", d.display())),
            Ok(m) if m.uid() != uid => return Err(format!("{} is not {person}'s", d.display())),
            Ok(_) => {}
            Err(_) => std::fs::DirBuilder::new().mode(0o770).create(&d).map_err(|e| format!("create {}: {e}", d.display()))?,
        }
        std::os::unix::fs::chown(&d, Some(uid), Some(gid)).map_err(|e| format!("chown {}: {e}", d.display()))?;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o2770)).map_err(|e| format!("chmod {}: {e}", d.display()))?;
    }
    Ok((home, gid))
}

/// Give `run_as` (an app's own account) the person's files group, so the
/// app's processes get it from initgroups when the jail starts them.
pub fn grant_group(person: &str, run_as: &str) -> Result<(), String> {
    let group = files_group(person);
    let (_, members) = group_entry(&group).ok_or_else(|| format!("no group {group}"))?;
    if members.iter().any(|m| m == run_as) {
        return Ok(());
    }
    pw(&["groupmod", &group, "-m", run_as])
}

/// The person's folders and where each goes in the jail at `root`.
pub fn mounts(home: &Path, root: &Path) -> Vec<(PathBuf, PathBuf)> {
    FOLDERS.iter().map(|f| (home.join(f), root.join(IN_JAIL).join(f))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_places() {
        assert_eq!(files_group("alice"), "files-alice");
        let m = mounts(Path::new("/home/alice"), Path::new("/var/lib/atrium/jails/x"));
        assert_eq!(m.len(), 5);
        assert_eq!(m[0], (PathBuf::from("/home/alice/Documents"), PathBuf::from("/var/lib/atrium/jails/x/atrium/home/Documents")));
    }
}
