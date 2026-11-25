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
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Under the persona's apps folder.
const DIR: &str = ".stoa";
const SESSIONS: &str = "sessions";

/// A folder made under a root-owned parent, or the one there: never a link,
/// always `uid`'s with exactly `mode`.
fn directory(path: &Path, uid: u32, mode: u32) -> Result<(), String> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => std::os::unix::fs::chown(path, Some(uid), Some(uid)).map_err(|e| format!("chown {}: {e}", path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", path.display())),
    }
    let m = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o7777 != mode {
        return Err(format!("{} has an unexpected owner or mode", path.display()));
    }
    Ok(())
}

/// The persona's apps folder: Personal's `~/.atrium/apps`, a named
/// persona's `~/.atrium/persona-apps/<persona>`. Only looked at: the
/// ordinary app launch prepares it (and its key group), as for Fasti. It
/// must be root's and not writable by group or others.
fn parent(person: &str, persona: &str) -> Result<PathBuf, String> {
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    let home = fs::canonicalize(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    let p = home.join(".atrium").join(if persona == "personal" {
        PathBuf::from("apps")
    } else {
        Path::new(portcullis_overlay::PERSONA_APPS).join(persona)
    });
    let m = fs::symlink_metadata(&p).map_err(|e| format!("{} ({e}): no app has run in {persona} yet", p.display()))?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        return Err(format!("{} is not root's own folder", p.display()));
    }
    Ok(p)
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
    let p = parent(person, persona)?;
    let top = p.join(DIR);
    directory(&top, 0, 0o700)?;
    let d = top.join(SESSIONS);
    directory(&d, stoad_uid, 0o700)?;
    let c = std::ffi::CString::new(d.as_os_str().as_encoded_bytes()).map_err(|_| "a NUL in the path".to_string())?;
    // SAFETY: a NUL-terminated path; the descriptor is ours alone on success.
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(format!("open {}: {}", d.display(), io::Error::last_os_error()));
    }
    // SAFETY: just opened, owned here alone.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // Checked on what was opened, never by path again.
    let m = fs::File::from(fd.try_clone().map_err(|e| e.to_string())?).metadata().map_err(|e| e.to_string())?;
    if m.uid() != stoad_uid || m.mode() & 0o777 != 0o700 {
        return Err(format!("{} changed under us", d.display()));
    }
    sealed(&fd)?;
    eprintln!("portcullisd: Stoa sessions of {person} ({persona}) are kept in {}", d.display());
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_with_the_wrong_mode_is_refused() {
        let base = std::env::temp_dir().join(format!("stoa-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let me = unsafe { libc::getuid() };
        let d = base.join("x");
        directory(&d, me, 0o700).unwrap();
        directory(&d, me, 0o700).unwrap();
        assert!(directory(&d, me, 0o755).is_err(), "an existing folder with another mode");
        std::os::unix::fs::symlink(&d, base.join("l")).unwrap();
        assert!(directory(&base.join("l"), me, 0o700).is_err(), "a link");
        let _ = fs::remove_dir_all(&base);
    }
}
