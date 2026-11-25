//! Where Stoa keeps a session's layout and scrollback (docs/spec/stoa.md
//! §5.4a): in the key group of the person and persona whose shell it runs,
//! never in stoad's own state.
//!
//! stoad asks (`SessionStateDir { uid, persona }`) the first time a person
//! mints a session in a persona; a named persona must be one of theirs.
//! portcullisd makes nothing but `<persona apps dir>/.stoa/` (root's, 0700)
//! and in it `sessions/` (stoad's uid, 0700), in an apps folder an app
//! launch already prepared; it asks the kernel that the folder is in an
//! open key group, and hands stoad the open directory. stoad names no path of a
//! person's and opens nothing it wasn't given. Without an open, encrypted
//! group there is no directory: the session is kept in memory only.
//!
//! The persona's crypto-erase takes the folder with it; before that, and at
//! logout, stoad is told to drop the person's sessions (`DROP`).

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Under the persona's apps folder.
const DIR: &str = ".stoa";
const SESSIONS: &str = "sessions";

fn cstr(name: &str) -> Result<std::ffi::CString, String> {
    std::ffi::CString::new(name).map_err(|_| format!("a NUL in {name:?}"))
}

/// `name` under the folder open on `dir`, opened without following a link.
fn open_at(dir: &OwnedFd, name: &str) -> Result<OwnedFd, String> {
    let c = cstr(name)?;
    // SAFETY: a live directory descriptor and a NUL-terminated name; the
    // new descriptor is ours alone on success.
    let raw = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(format!("{name}: {}", io::Error::last_os_error()));
    }
    // SAFETY: just opened, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// (owner, mode) of what is open on `fd`.
fn stat_fd(fd: &OwnedFd) -> Result<(u32, u32), String> {
    let m = fs::File::from(fd.try_clone().map_err(|e| e.to_string())?).metadata().map_err(|e| e.to_string())?;
    Ok((m.uid(), m.mode() & 0o7777))
}

/// The persona's apps folder, walked one folder at a time from the home
/// down, never through a link: Personal's `.atrium/apps`, a named
/// persona's `.atrium/persona-apps/<persona>`. Only looked at: the
/// ordinary app launch prepares it (and its key group), as for Fasti.
/// Each folder below the home must be `owner`'s (root's) and not writable
/// by group or others.
fn open_parent(home: &Path, persona: &str, owner: u32) -> Result<OwnedFd, String> {
    let c = std::ffi::CString::new(home.as_os_str().as_encoded_bytes()).map_err(|_| "a NUL in the home".to_string())?;
    // SAFETY: a NUL-terminated path; the descriptor is ours alone on success.
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(format!("{}: {}", home.display(), io::Error::last_os_error()));
    }
    // SAFETY: just opened, owned here alone.
    let mut fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let comps: &[&str] = if persona == "personal" { &[".atrium", "apps"] } else { &[".atrium", portcullis_overlay::PERSONA_APPS, persona] };
    for c in comps {
        fd = open_at(&fd, c).map_err(|e| format!("{e}: no app has run in {persona} yet"))?;
        let (uid, mode) = stat_fd(&fd)?;
        if uid != owner || mode & 0o022 != 0 {
            return Err(format!("{}'s {c} is not root's own folder", home.display()));
        }
    }
    Ok(fd)
}

/// The folder `name` in the one open on `dir`, made if missing (owned by
/// `uid`, `mode`), opened without following a link and checked on what was
/// opened: `uid`'s with exactly `mode`.
fn dir_at(dir: &OwnedFd, name: &str, uid: u32, mode: u32) -> Result<OwnedFd, String> {
    let c = cstr(name)?;
    // SAFETY: a live directory descriptor and a NUL-terminated name.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), mode as libc::mode_t) } == 0 {
        // SAFETY: as above; AT_SYMLINK_NOFOLLOW so a link swapped in is not followed.
        if unsafe { libc::fchownat(dir.as_raw_fd(), c.as_ptr(), uid, uid, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(format!("chown {name}: {}", io::Error::last_os_error()));
        }
    } else {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(format!("create {name}: {e}"));
        }
    }
    let fd = open_at(dir, name)?;
    if stat_fd(&fd)? != (uid, mode) {
        return Err(format!("{name} has an unexpected owner or mode"));
    }
    Ok(fd)
}

/// Is the folder open on `fd` in a key group that is open now?
#[cfg(target_os = "freebsd")]
fn sealed(fd: &OwnedFd) -> Result<(), String> {
    use atrium_volumes::ffi::{GroupInfo, TESSERA_IOC_GROUP_INFO};
    let mut gi = GroupInfo::default();
    // SAFETY: an open directory descriptor; `gi` is the #[repr(C)] struct of
    // the size the command encodes, `inode` 0 asks about this directory.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), TESSERA_IOC_GROUP_INFO as libc::c_ulong, &mut gi as *mut GroupInfo) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::ENOTTY) {
            "its folder is not on an encrypted volume".into()
        } else {
            format!("the kernel didn't say where its folder is ({e})")
        });
    }
    if gi.group_id == 0 {
        return Err("its folder is in no key group".into());
    }
    if !gi.has(GroupInfo::OPEN) || gi.has(GroupInfo::LOCKED) || gi.has(GroupInfo::BAD) {
        return Err(format!("its key group {} is not open", gi.group_id));
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn sealed(_fd: &OwnedFd) -> Result<(), String> {
    Err("no encrypted volumes here".into())
}

/// The open `sessions` folder of `uid`'s `persona`, for the stoad running
/// as `stoad_uid`. `Err` says why the session is kept in memory only.
pub fn dir_for(stoad_uid: u32, uid: u32, person: &str, persona: &str) -> Result<OwnedFd, String> {
    if uid < 1000 || !portcullis_overlay::plain_segment(person) {
        return Err(format!("uid {uid} is not a person"));
    }
    if persona != "personal" && !portcullis_peer::valid_person_persona(persona) {
        return Err(format!("{persona:?} is not a persona"));
    }
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    let home = fs::canonicalize(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    let parent = open_parent(&home, persona, 0)?;
    let top = dir_at(&parent, DIR, 0, 0o700)?;
    let fd = dir_at(&top, SESSIONS, stoad_uid, 0o700)?;
    sealed(&fd)?;
    eprintln!("portcullisd: Stoa sessions of {person} ({persona}) are kept in their key group");
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fn base(tag: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("stoa-state-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&b);
        fs::create_dir_all(&b).unwrap();
        b
    }

    fn mkdirs(p: &Path) {
        fs::DirBuilder::new().recursive(true).mode(0o755).create(p).unwrap();
    }

    #[test]
    fn a_folder_with_the_wrong_mode_is_refused() {
        let b = base("mode");
        let me = unsafe { libc::getuid() };
        let d = open_at(&fs::File::open(&b).unwrap().into(), ".").unwrap();
        dir_at(&d, "x", me, 0o700).unwrap();
        dir_at(&d, "x", me, 0o700).unwrap();
        assert!(dir_at(&d, "x", me, 0o755).is_err(), "an existing folder with another mode");
        std::os::unix::fs::symlink(b.join("x"), b.join("l")).unwrap();
        assert!(dir_at(&d, "l", me, 0o700).is_err(), "a link");
        let _ = fs::remove_dir_all(&b);
    }

    /// No folder from the home down is taken through a link: a person who
    /// moves `.atrium` aside and links `persona-apps` to another person's
    /// gets nothing.
    #[test]
    fn a_link_anywhere_below_the_home_is_refused() {
        let b = base("walk");
        let me = unsafe { libc::getuid() };
        let (home, other) = (b.join("home"), b.join("other"));
        mkdirs(&home.join(".atrium/persona-apps/work"));
        mkdirs(&other.join(".atrium/persona-apps/work"));
        assert!(open_parent(&home, "work", me).is_ok());
        assert!(open_parent(&home, "play", me).is_err(), "a persona folder no launch made");
        fs::rename(home.join(".atrium/persona-apps"), home.join(".atrium/held")).unwrap();
        std::os::unix::fs::symlink(other.join(".atrium/persona-apps"), home.join(".atrium/persona-apps")).unwrap();
        assert!(open_parent(&home, "work", me).is_err(), "persona-apps is a link");
        fs::rename(home.join(".atrium"), home.join("atrium-held")).unwrap();
        std::os::unix::fs::symlink(other.join(".atrium"), home.join(".atrium")).unwrap();
        assert!(open_parent(&home, "work", me).is_err(), ".atrium is a link");
        fs::remove_file(home.join(".atrium")).unwrap();
        mkdirs(&home.join(".atrium/persona-apps/work"));
        fs::set_permissions(home.join(".atrium/persona-apps"), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(open_parent(&home, "work", me).is_err(), "a group-writable folder on the way");
        let _ = fs::remove_dir_all(&b);
    }
}
