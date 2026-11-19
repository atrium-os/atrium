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

/// atrium-volumes' socket (persona folders' encryption, E4.6).
pub const VOLUMES_SOCKET: &str = "/var/run/atrium/atrium-volumes.sock";

fn volumes() -> Option<crate::volumes_client::Client> {
    let path = std::env::var_os("ATRIUM_VOLUMES_SOCKET").map(PathBuf::from).unwrap_or_else(|| VOLUMES_SOCKET.into());
    match crate::volumes_client::Client::connect(&path) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("portcullisd: atrium-volumes at {} not reached ({e}): folders are not encrypted now", path.display());
            None
        }
    }
}

/// A persona's folders, encrypted in its key group with its key loaded
/// from the person's Arca (tessera-encryption.md §5.4, E4.6): atrium-volumes
/// makes missing ones in the group and moves a plain one's files in. A
/// folder that should be encrypted and isn't (an offline edit) is refused,
/// and so is a locked Arca: the folders aren't handed out. Without Tessera
/// or Arca keys on this machine the folders stay as they were.
fn encrypt_folders(person: &str, persona: &str, dirs: &[PathBuf], uid: u32, gid: u32) -> Result<(), String> {
    use atrium_volumes::protocol::{PersonaFoldersRequest, Request, Response};
    let Some(mut c) = volumes() else { return Ok(()) };
    let req = Request::PersonaFolders(PersonaFoldersRequest {
        person: person.into(),
        persona: persona.into(),
        folders: dirs.iter().map(|d| d.display().to_string()).collect(),
        uid,
        gid,
        mode: 0o2770,
    });
    match c.send(&req) {
        Ok(Response::FoldersReady { encrypted, moved }) => {
            for m in moved {
                eprintln!("portcullisd: {person}'s {m}: old files moved into the encrypted folder");
            }
            if !encrypted {
                eprintln!("portcullisd: {person}'s {persona} folders are not encrypted (no Tessera keys here)");
            }
            Ok(())
        }
        Ok(Response::FoldersRefused { folder, detail }) => Err(format!("{folder} refused: {detail}")),
        Ok(Response::Error { detail }) => Err(detail),
        Ok(other) => Err(format!("atrium-volumes: unexpected {other:?}")),
        Err(e) => {
            eprintln!("portcullisd: atrium-volumes: {e}: {person}'s {persona} folders are not encrypted now");
            Ok(())
        }
    }
}

/// A removed persona's folders, crypto-erased: `Ok(false)` if they were
/// never encrypted.
fn erase_folders(person: &str, persona: &str) -> Result<bool, String> {
    use atrium_volumes::protocol::{EraseFoldersRequest, Request, Response};
    let Some(mut c) = volumes() else { return Ok(false) };
    // The home, so atrium-volumes scrubs its volume: the persona's app
    // data went in the clear just before (E5).
    let home = portcullis_overlay::home_of(person).map(|h| h.display().to_string());
    match c.send(&Request::EraseFolders(EraseFoldersRequest { person: person.into(), persona: persona.into(), home })) {
        Ok(Response::FoldersErased { erased }) => Ok(erased),
        Ok(Response::Error { detail }) => Err(detail),
        Ok(other) => Err(format!("atrium-volumes: unexpected {other:?}")),
        Err(e) => Err(format!("atrium-volumes: {e}")),
    }
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
    encrypt_folders(person, "personal", &FOLDERS.iter().map(|f| home.join(f)).collect::<Vec<_>>(), uid, gid)?;
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

/// Where the named personas' folders are under the person's home
/// (personas.md §4.1): `~/.atrium/personas/<persona>/Documents`, and so on.
pub const PERSONAS_DIR: &str = ".atrium/personas";
/// Where they appear in a `home-files` jail: `/atrium/personas/<persona>/`.
pub const PERSONAS_IN_JAIL: &str = "atrium/personas";

/// A persona's files group: `files-<person>-<persona>`, or, when that is
/// longer than a group name may be, the person's group with a hash of the
/// persona's name (stable, so it is found again).
pub fn persona_group(person: &str, persona: &str) -> String {
    let g = format!("files-{person}-{persona}");
    if g.len() <= 32 {
        return g;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in persona.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    let base: String = format!("files-{person}").chars().take(22).collect();
    format!("{base}-{:08x}", h as u32)
}

fn plain(persona: &str) -> bool {
    !persona.is_empty() && persona.len() <= 32 && persona != "personal"
        && persona.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && persona.as_bytes()[0].is_ascii_lowercase()
}

fn ensure_group(group: &str) -> Result<u32, String> {
    match gid_of(group) {
        Some(g) => Ok(g),
        None => {
            pw(&["groupadd", group])?;
            gid_of(group).ok_or_else(|| format!("{group} was not made"))
        }
    }
}

/// A folder of the person's, made if missing, `uid:gid` 2770; a link or
/// someone else's is refused.
fn ensure_folder(d: &Path, uid: u32, gid: u32, person: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(d) {
        Ok(m) if !m.is_dir() => return Err(format!("{} is not a folder", d.display())),
        Ok(m) if m.uid() != uid => return Err(format!("{} is not {person}'s", d.display())),
        Ok(_) => {}
        Err(_) => std::fs::DirBuilder::new().mode(0o770).create(d).map_err(|e| format!("create {}: {e}", d.display()))?,
    }
    std::os::unix::fs::chown(d, Some(uid), Some(gid)).map_err(|e| format!("chown {}: {e}", d.display()))?;
    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o2770)).map_err(|e| format!("chmod {}: {e}", d.display()))
}

/// A root-owned directory 0755 (no link): the levels above a persona's
/// folders, so nothing but root renames or replaces them.
fn ensure_root_dir(d: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(d) {
        Ok(m) if !m.is_dir() || m.uid() != 0 => return Err(format!("{} is not root's folder", d.display())),
        Ok(_) => {}
        Err(_) => std::fs::DirBuilder::new().mode(0o755).create(d).map_err(|e| format!("create {}: {e}", d.display()))?,
    }
    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("chmod {}: {e}", d.display()))
}

/// Make a named persona's folders (personas.md §4.1) and its files group if
/// missing: `~/.atrium/personas/<persona>/` root's, each folder the
/// person's with the persona's group, 2770. Returns the persona's folder
/// and the group's id.
pub fn ensure_persona(person: &str, persona: &str) -> Result<(PathBuf, u32), String> {
    if !plain(persona) {
        return Err(format!("{persona:?} is not a named persona"));
    }
    let (home, _) = ensure_person(person)?;
    let uid = uid_of(person).ok_or_else(|| format!("no such person {person}"))?;
    let gid = ensure_group(&persona_group(person, persona))?;
    let all = home.join(PERSONAS_DIR);
    ensure_root_dir(&all)?;
    let dir = all.join(persona);
    ensure_root_dir(&dir)?;
    encrypt_folders(person, persona, &FOLDERS.iter().map(|f| dir.join(f)).collect::<Vec<_>>(), uid, gid)?;
    for f in FOLDERS {
        ensure_folder(&dir.join(f), uid, gid, person)?;
    }
    Ok((dir, gid))
}

/// Where a download from an instance in `persona` goes: the person's
/// Downloads for Personal, the persona's own otherwise (personas.md §4.1).
pub fn downloads_for(person: &str, persona: Option<&str>) -> Result<(PathBuf, u32), String> {
    match persona.filter(|p| *p != "personal") {
        None => ensure_person(person).map(|(h, g)| (h.join("Downloads"), g)),
        Some(p) => ensure_persona(person, p).map(|(d, g)| (d.join("Downloads"), g)),
    }
}

/// Give `run_as` a persona's files group too (the file manager spans the
/// personas, §3; no app instance holds one).
pub fn grant_persona_group(person: &str, persona: &str, run_as: &str) -> Result<(), String> {
    let group = persona_group(person, persona);
    let (_, members) = group_entry(&group).ok_or_else(|| format!("no group {group}"))?;
    if members.iter().any(|m| m == run_as) {
        return Ok(());
    }
    pw(&["groupmod", &group, "-m", run_as])
}

/// A removed persona's folders and group go (personas.md §4.1 "Removing a
/// persona"). Encrypted, it is the key that goes: arcad deletes it, the
/// kernel drops it and crypto-erases the group (E4.6), and the empty
/// folders are deleted after. Plain, they are deleted.
pub fn remove_persona(person: &str, persona: &str) -> Result<bool, String> {
    if !plain(persona) {
        return Err(format!("{persona:?} is not a named persona"));
    }
    // An erase that can't finish now (a folder in use) finishes at the
    // person's next login; its key is gone already, so nothing is readable.
    if let Err(e) = erase_folders(person, persona) {
        return Err(format!("its key is gone; the erase finishes later ({e})"));
    }
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    let dir = home.join(PERSONAS_DIR).join(persona);
    let had = match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() && m.uid() == 0 => {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("remove {}: {e}", dir.display()))?;
            true
        }
        Ok(_) => return Err(format!("{} is not root's folder; left", dir.display())),
        Err(_) => false,
    };
    let group = persona_group(person, persona);
    if gid_of(&group).is_some() {
        pw(&["groupdel", &group])?;
    }
    Ok(had)
}

/// Where dispensatord serves removable media (atrium-media).
const MEDIA_SOCKET: &str = "/atrium/sockets/removable-media/removable-media.sock";

/// The persona's sealed sticks that are mounted now, by name (E5). Removing
/// the persona takes their keys, so they show Locked when next plugged in,
/// but a mounted stick stays open until it is ejected: Remove waits for
/// that. No dispensatord: no sticks.
pub fn mounted_sticks(persona: &str) -> Vec<String> {
    match atrium_media::client::list(Path::new(MEDIA_SOCKET)) {
        Ok(v) => v
            .into_iter()
            .filter(|v| v.persona == persona && !v.locked && !v.mountpoint.is_empty())
            .map(|v| if v.label.is_empty() { format!("the stick {}", v.part) } else { format!("the stick “{}”", v.label) })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The person's uid.
pub fn person_uid(person: &str) -> Option<u32> { uid_of(person) }

/// A persona's name in words: "work" → "Work", "side-project" → "Side project".
pub fn persona_words(p: &str) -> String {
    let s = p.replace('-', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The user extended attribute that says where a saved file came from.
pub const ORIGIN_ATTR: &str = "atrium.origin";

/// Write `from` (at most `max` bytes) into `dir` as `name`, or `name (2)`
/// and so on beside one already there: never over a file, never through a
/// link. The file is `uid:gid` 0660 with `origin` as [`ORIGIN_ATTR`].
/// Returns the name it got.
pub fn save_into(dir: &Path, name: &str, mut from: std::fs::File, max: u64, uid: u32, gid: u32, origin: &str) -> Result<String, String> {
    use std::io::{Read, Seek, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let _ = from.seek(std::io::SeekFrom::Start(0));
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..1000 {
        let file = if n == 1 { name.to_string() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(&file);
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let done = (|| -> Result<(), String> {
            let n = std::io::copy(&mut (&mut from).take(max + 1), &mut f).map_err(|e| e.to_string())?;
            if n > max {
                return Err(format!("the file is over {} MB", max >> 20));
            }
            f.flush().and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
            std::os::unix::fs::fchown(&f, Some(uid), Some(gid)).map_err(|e| format!("chown: {e}"))?;
            f.set_permissions(std::fs::Permissions::from_mode(0o660)).map_err(|e| format!("chmod: {e}"))
        })();
        if let Err(e) = done {
            let _ = std::fs::remove_file(&path);
            return Err(format!("{}: {e}", path.display()));
        }
        if let Err(e) = mark_origin(&f, origin) {
            eprintln!("portcullisd: {}: origin not recorded ({e})", path.display());
        }
        return Ok(file);
    }
    Err("too many files of that name".into())
}

#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)]
fn mark_origin(f: &std::fs::File, url: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(ORIGIN_ATTR).unwrap();
    // SAFETY: a descriptor we hold, a NUL-terminated name, a buffer and its length.
    let r = unsafe { libc::extattr_set_fd(f.as_raw_fd(), libc::EXTATTR_NAMESPACE_USER, name.as_ptr(), url.as_ptr().cast(), url.len()) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(target_os = "freebsd"))]
fn mark_origin(_: &std::fs::File, _: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("no extended attributes here"))
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

    #[test]
    fn persona_groups_fit_a_group_name() {
        assert_eq!(persona_group("alice", "work"), "files-alice-work");
        let long = persona_group("alice", "a-very-long-persona-name-indeed");
        assert!(long.len() <= 32 && long.starts_with("files-alice-"), "{long}");
        assert_eq!(long, persona_group("alice", "a-very-long-persona-name-indeed"), "stable");
        assert_ne!(long, persona_group("alice", "a-very-long-persona-name-indeeds"));
        assert!(!plain("personal") && !plain("../x") && !plain("Work") && plain("side-project"));
    }
}
