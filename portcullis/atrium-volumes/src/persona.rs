//! A person's folders, encrypted per persona (tessera-encryption.md §5.4,
//! §13 E4.6). portcullisd asks before it hands a persona's folders to an
//! app (at login, in practice). Each persona (Personal included) has one key
//! group; each of its folders is a quota domain in that group. The group key
//! is minted in, and loaded from, the person's Arca store, so the folders
//! open only while the person is logged in (arcad drops the key at logout).
//!
//! - First time: a key group is made and recorded as expected.
//! - A folder that doesn't exist is made in the group.
//! - A plain folder from before is moved in (Shankar 2026-11-19, "move them
//!   in at next login"): it is renamed aside to `.<name>.unencrypted`, the
//!   folder is made again in the group, everything is copied in (the kernel
//!   encrypts it as it goes; a rename across groups would be `EXDEV`), and
//!   the old copy is deleted. An interrupted move finishes next time.
//!   Before the old copy goes, a scrub of its volume is queued, and it
//!   runs once the folders are handed out (scrub.rs, E4.7): deleting
//!   leaves the old bytes on disk until something overwrites them.
//! - A folder that was in the group and isn't any more, or is in another
//!   group, or whose record doesn't verify, is refused (§5.4 expected
//!   groups): portcullisd doesn't hand it out.
//! - A removed persona: arcad retires its key (the kernel drops it) and the
//!   kernel crypto-erases the group (§10).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use log::{info, warn};

use crate::arca::{hex, unhex16, KeyError, Keys};
use crate::ffi::{self, GroupInfo};
use crate::groups::{GroupEntry, Groups};
use crate::protocol::{EraseFoldersRequest, PersonaFoldersRequest, Response};
use crate::scrub::ScrubQueue;

/// The kernel side, or a stand-in for tests.
pub trait Kernel {
    fn info(&self, dir: &Path) -> io::Result<GroupInfo>;
    /// Make `dir` the root of a quota domain of its own (no limit).
    fn make_domain(&self, dir: &Path) -> io::Result<()>;
    fn group_create(&self, dir: &Path, key_id: &[u8; 16], group: u64) -> io::Result<()>;
    fn join_group(&self, dir: &Path, group: u64) -> io::Result<()>;
    fn erase(&self, dir: &Path, group: u64) -> io::Result<()>;
}

pub struct Tessera;

impl Kernel for Tessera {
    fn info(&self, dir: &Path) -> io::Result<GroupInfo> { ffi::tessera_group_info(dir) }
    fn make_domain(&self, dir: &Path) -> io::Result<()> { ffi::tessera_set_quota(dir, 0) }
    fn group_create(&self, dir: &Path, key_id: &[u8; 16], group: u64) -> io::Result<()> {
        ffi::tessera_group_create(dir, key_id, group)
    }
    fn join_group(&self, dir: &Path, group: u64) -> io::Result<()> { ffi::tessera_quota_group(dir, group) }
    fn erase(&self, dir: &Path, group: u64) -> io::Result<()> { ffi::tessera_group_erase(dir, group) }
}

/// Where a plain folder waits while it moves in.
pub fn parked(folder: &Path) -> PathBuf {
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    folder.with_file_name(format!(".{name}.unencrypted"))
}

/// Largest key group id (format.h `TESSERA_KEY_GROUP_ID_MAX`).
const GROUP_ID_MAX: u64 = (1 << 56) - 1;

fn random_group() -> io::Result<u64> {
    use std::io::Read;
    let mut b = [0u8; 8];
    fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    // Random, so an id is never used twice on a volume (an erased group's
    // packs are freed by id, encryption §13 E4.5), even if this file is lost.
    Ok((u64::from_le_bytes(b) & GROUP_ID_MAX).max(1))
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
        && !s.starts_with('.')
}

fn folder_ok(f: &str) -> bool {
    let p = Path::new(f);
    p.is_absolute() && p.parent().is_some()
        && p.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn refused(folder: &Path, detail: impl Into<String>) -> Response {
    let detail = detail.into();
    warn!("persona folders: {} REFUSED: {detail}", folder.display());
    Response::FoldersRefused { folder: folder.display().to_string(), detail }
}

fn error(detail: impl Into<String>) -> Response {
    Response::Error { detail: detail.into() }
}

fn is_enotty(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOTTY) | Some(libc::EOPNOTSUPP))
        || e.kind() == io::ErrorKind::Unsupported
}

pub struct Folders<'a, K: Kernel, A: Keys> {
    pub kernel: &'a K,
    pub keys:   &'a A,
    pub scrub:  &'a dyn ScrubQueue,
    pub groups: &'a mut Groups,
    pub path:   &'a Path,
}

enum State {
    Missing,
    Open,
    Locked,
    Plain,
    Wrong(String),
}

impl<'a, K: Kernel, A: Keys> Folders<'a, K, A> {
    fn save(&mut self) -> Result<(), Response> {
        self.groups.save(self.path).map_err(|e| error(format!("expected groups not saved: {e}")))
    }

    fn state(&self, f: &Path, e: &GroupEntry) -> Result<State, Response> {
        match fs::symlink_metadata(f) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(State::Missing),
            Err(err) => return Err(error(format!("{}: {err}", f.display()))),
            Ok(m) if !m.is_dir() => return Ok(State::Wrong("it is not a folder".into())),
            Ok(_) => {}
        }
        let i = self.kernel.info(f).map_err(|err| error(format!("{}: {err}", f.display())))?;
        if hex(&i.volume_uuid) != e.volume_uuid {
            return Ok(State::Wrong("it is on another volume".into()));
        }
        if i.group_id == 0 {
            return Ok(State::Plain);
        }
        if i.group_id != e.group_id {
            return Ok(State::Wrong(format!("it is in key group {}, not {}", i.group_id, e.group_id)));
        }
        if !i.has(GroupInfo::ROOT) {
            return Ok(State::Wrong("it is inside another folder of the group".into()));
        }
        if i.has(GroupInfo::NO_RECORD) || i.has(GroupInfo::BAD) {
            return Ok(State::Wrong("its key group doesn't verify".into()));
        }
        if hex(&i.key_id) != e.key_id {
            return Ok(State::Wrong("its key group names another key".into()));
        }
        Ok(if i.has(GroupInfo::OPEN) { State::Open } else { State::Locked })
    }

    fn load(&self, req: &PersonaFoldersRequest, e: &GroupEntry, uuid: &[u8; 16]) -> Result<(), Response> {
        let key = unhex16(&e.key_id).ok_or_else(|| error("bad key id in expected groups"))?;
        match self.keys.load(&req.person, &req.persona, &key, uuid, e.group_id) {
            Ok(()) => Ok(()),
            Err(KeyError::Locked) => Err(error(format!("{}'s Arca is locked: {}'s folders stay closed", req.person, req.persona))),
            Err(err) => Err(error(format!("{}'s {} folder key: {err}", req.person, req.persona))),
        }
    }

    /// A folder made empty in the group, then given to the person.
    fn make(&self, f: &Path, group: u64, req: &PersonaFoldersRequest) -> Result<(), Response> {
        let e = |what: &str, err: io::Error| error(format!("{}: {what}: {err}", f.display()));
        fs::DirBuilder::new().create(f).map_err(|err| e("create", err))?;
        fs::set_permissions(f, fs::Permissions::from_mode(0o700)).map_err(|err| e("chmod", err))?;
        self.kernel.make_domain(f).map_err(|err| e("domain", err))?;
        self.kernel.join_group(f, group).map_err(|err| e("key group", err))?;
        ffi::chown(f, req.uid, req.gid).map_err(|err| e("chown", err))?;
        fs::set_permissions(f, fs::Permissions::from_mode(req.mode)).map_err(|err| e("chmod", err))?;
        Ok(())
    }

    pub fn prepare(&mut self, req: &PersonaFoldersRequest) -> Response {
        if !name_ok(&req.person) || !name_ok(&req.persona) {
            return error("bad person or persona name");
        }
        if req.folders.is_empty() || !req.folders.iter().all(|f| folder_ok(f)) {
            return error("folders must be absolute paths");
        }
        let folders: Vec<PathBuf> = req.folders.iter().map(PathBuf::from).collect();
        let probe = folders[0].parent().unwrap().to_path_buf();
        // A removed persona's erase that couldn't finish (its folders were
        // in use): try again now. Its key is gone already.
        let pending: Vec<GroupEntry> = self.groups.groups.iter()
            .filter(|g| g.retired && g.person == req.person && g.persona != req.persona)
            .cloned().collect();
        for g in pending {
            if let Err(Response::Error { detail }) = self.finish_erase(&g) {
                warn!("persona folders: {detail}");
            }
        }
        let entry = self.groups.find(&req.person, &req.persona).cloned();
        // A persona removed and added again under the same name: finish
        // the old group's erase first, then start afresh.
        if let Some(e) = entry.as_ref().filter(|e| e.retired) {
            if let Err(r) = self.finish_erase(e) {
                return r;
            }
            return self.prepare(req);
        }
        let base = match self.kernel.info(&probe) {
            Ok(i) => i,
            Err(err) if entry.is_none() && is_enotty(&err) => {
                info!("persona folders: {}'s {} stay plain: {} is not on Tessera", req.person, req.persona, probe.display());
                return Response::FoldersReady { encrypted: false, moved: Vec::new() };
            }
            Err(err) if entry.is_some() && is_enotty(&err) => {
                return refused(&probe, "its folders were encrypted and it is no longer on Tessera");
            }
            Err(err) => return error(format!("{}: {err}", probe.display())),
        };
        let uuid = base.volume_uuid;
        let mut loaded = false;
        let mut e = match entry {
            Some(e) => e,
            None => {
                let group = match random_group() {
                    Ok(g) => g,
                    Err(err) => return error(format!("random: {err}")),
                };
                let label = format!("{} folders", if req.persona == "personal" { "Personal" } else { &req.persona });
                let key = match self.keys.mint(&req.person, &req.persona, &uuid, group, &label) {
                    Ok(k) => k,
                    Err(KeyError::Unavailable(why)) => {
                        warn!("persona folders: {}'s {} stay plain for now: {why}", req.person, req.persona);
                        return Response::FoldersReady { encrypted: false, moved: Vec::new() };
                    }
                    Err(err) => return error(format!("{}'s {} folder key: {err}", req.person, req.persona)),
                };
                if let Err(err) = self.kernel.group_create(&probe, &key, group) {
                    let _ = self.keys.retire(&req.person, &req.persona, &key);
                    return error(format!("key group: {err}"));
                }
                let e = GroupEntry {
                    person: req.person.clone(), persona: req.persona.clone(),
                    volume_uuid: hex(&uuid), group_id: group, key_id: hex(&key),
                    folders: Vec::new(), retired: false,
                };
                self.groups.groups.push(e.clone());
                if let Err(r) = self.save() {
                    return r;
                }
                info!("persona folders: {}'s {} has key group {group}", req.person, req.persona);
                loaded = true;
                e
            }
        };
        if e.volume_uuid != hex(&uuid) {
            return refused(&probe, "its folders' group is on another volume");
        }
        let mut moved = Vec::new();
        for f in &folders {
            let fs_ = f.display().to_string();
            let expected = e.folders.contains(&fs_);
            let mut st = match self.state(f, &e) {
                Ok(s) => s,
                Err(r) => return r,
            };
            if matches!(st, State::Locked) || (!loaded && matches!(st, State::Missing | State::Plain)) {
                if let Err(r) = self.load(req, &e, &uuid) {
                    return r;
                }
                loaded = true;
                st = match self.state(f, &e) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
            }
            match st {
                State::Open => {}
                State::Locked => return refused(f, "its key group stays locked with its key loaded"),
                State::Wrong(why) => return refused(f, why),
                State::Plain if expected => return refused(f, "it was encrypted and no longer is"),
                State::Missing => {
                    if let Err(r) = self.make(f, e.group_id, req) {
                        return r;
                    }
                }
                State::Plain => {
                    let p = parked(f);
                    if fs::symlink_metadata(&p).is_ok() {
                        return refused(f, format!("{} is in the way", p.display()));
                    }
                    if let Err(err) = fs::rename(f, &p) {
                        return error(format!("{}: set aside: {err}", f.display()));
                    }
                    if let Err(r) = self.make(f, e.group_id, req) {
                        // Put it back as it was.
                        let _ = fs::remove_dir(f);
                        let _ = fs::rename(&p, f);
                        return r;
                    }
                }
            }
            if !expected {
                e.folders.push(fs_.clone());
                if let Some(x) = self.groups.find_mut(&req.person, &req.persona) {
                    x.folders = e.folders.clone();
                }
                if let Err(r) = self.save() {
                    return r;
                }
            }
            // A move in, new or interrupted.
            let p = parked(f);
            if let Ok(m) = fs::symlink_metadata(&p) {
                if !m.is_dir() {
                    return refused(f, format!("{} is not a folder", p.display()));
                }
                if let Err(err) = copy_tree(&p, f) {
                    return error(format!("moving {} into {}: {err}", p.display(), f.display()));
                }
                // Queued before the delete, so a crash in between still
                // gets its scrub.
                if let Err(err) = self.scrub.want(&probe) {
                    return error(format!("queueing a scrub of {}: {err}", probe.display()));
                }
                if let Err(err) = fs::remove_dir_all(&p) {
                    return error(format!("{}: {err}", p.display()));
                }
                info!("persona folders: {} moved into {}'s encrypted {}", p.display(), req.person, f.display());
                moved.push(fs_);
            }
        }
        if !moved.is_empty() {
            self.scrub.kick();
        }
        Response::FoldersReady { encrypted: true, moved }
    }

    /// A removed persona (personas.md §4.1): its key goes, then its group.
    pub fn erase(&mut self, req: &EraseFoldersRequest) -> Response {
        if !name_ok(&req.person) || !name_ok(&req.persona) {
            return error("bad person or persona name");
        }
        let Some(mut e) = self.groups.find(&req.person, &req.persona).cloned() else {
            return Response::FoldersErased { erased: false };
        };
        if !e.retired {
            let Some(key) = unhex16(&e.key_id) else { return error("bad key id in expected groups") };
            match self.keys.retire(&req.person, &req.persona, &key) {
                // Denied: arcad no longer has it (an earlier attempt).
                Ok(()) | Err(KeyError::Denied) => {}
                Err(err) => return error(format!("{}'s {} folder key: {err}", req.person, req.persona)),
            }
            e.retired = true;
            if let Some(x) = self.groups.find_mut(&req.person, &req.persona) {
                x.retired = true;
            }
            if let Err(r) = self.save() {
                return r;
            }
        }
        match self.finish_erase(&e) {
            Ok(()) => Response::FoldersErased { erased: true },
            Err(r) => r,
        }
    }

    /// The kernel's crypto-erase of a retired group, then the empty roots.
    fn finish_erase(&mut self, e: &GroupEntry) -> Result<(), Response> {
        let probe = e.folders.iter().map(PathBuf::from)
            .find_map(|f| f.parent().filter(|p| p.exists()).map(Path::to_path_buf));
        if let Some(dir) = probe {
            match self.kernel.erase(&dir, e.group_id) {
                Ok(()) => info!("persona folders: {}'s {} key group {} erased", e.person, e.persona, e.group_id),
                // ENOENT: no such group any more (erased before).
                Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {}
                Err(err) => {
                    return Err(error(format!("{}'s {} folders: erase: {err} (again next time)", e.person, e.persona)));
                }
            }
        }
        for f in &e.folders {
            let f = Path::new(f);
            let _ = fs::remove_dir(f);
            let _ = fs::remove_dir_all(parked(f));
        }
        self.groups.remove(&e.person, &e.persona);
        self.save()
    }
}

/// Copy `from` into `to` (an existing folder) without following links:
/// folders, files (contents, mode, owner, times, where a download came
/// from) and links. Anything else (a socket, a fifo) is left out. Files
/// already in `to` are overwritten: this is how an interrupted move resumes.
pub fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for ent in fs::read_dir(from)? {
        let ent = ent?;
        let src = ent.path();
        let dst = to.join(ent.file_name());
        let m = fs::symlink_metadata(&src)?;
        let ft = m.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(&src)?;
            if fs::symlink_metadata(&dst).is_ok() {
                remove_any(&dst)?;
            }
            std::os::unix::fs::symlink(&target, &dst)?;
            std::os::unix::fs::lchown(&dst, Some(m.uid()), Some(m.gid()))?;
        } else if ft.is_dir() {
            match fs::symlink_metadata(&dst) {
                Ok(d) if d.is_dir() => {}
                Ok(_) => {
                    remove_any(&dst)?;
                    fs::create_dir(&dst)?;
                }
                Err(_) => fs::create_dir(&dst)?,
            }
            copy_tree(&src, &dst)?;
            std::os::unix::fs::lchown(&dst, Some(m.uid()), Some(m.gid()))?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
            set_times(&dst, &m)?;
        } else if ft.is_file() {
            if let Ok(d) = fs::symlink_metadata(&dst) {
                if !d.is_file() {
                    remove_any(&dst)?;
                }
            }
            copy_file(&src, &dst, &m)?;
        } else {
            warn!("persona folders: {} left out (not a file, folder or link)", src.display());
        }
    }
    Ok(())
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) => Err(e),
    }
}

fn copy_file(src: &Path, dst: &Path, m: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(src)?;
    let mut to = fs::OpenOptions::new().write(true).create(true).truncate(true)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(dst)?;
    io::copy(&mut from, &mut to)?;
    to.sync_all()?;
    std::os::unix::fs::fchown(&to, Some(m.uid()), Some(m.gid()))?;
    to.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777))?;
    if let Err(e) = ffi::copy_origin(&from, &to) {
        warn!("persona folders: {}: origin not copied ({e})", dst.display());
    }
    to.set_times(fs::FileTimes::new().set_accessed(m.accessed()?).set_modified(m.modified()?))
}

fn set_times(dir: &Path, m: &fs::Metadata) -> io::Result<()> {
    let f = fs::File::open(dir)?;
    f.set_times(fs::FileTimes::new().set_accessed(m.accessed()?).set_modified(m.modified()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    const UUID: [u8; 16] = [0x42; 16];

    #[derive(Default)]
    struct World {
        /// Domain roots → their group (0: none).
        domains: HashMap<PathBuf, u64>,
        /// Group → its key.
        groups:  HashMap<u64, [u8; 16]>,
        /// Keys in the kernel's keyring.
        loaded:  HashSet<[u8; 16]>,
        /// Keys in Arca.
        arca:    HashMap<[u8; 16], u64>,
        minted:  u32,
        loads:   u32,
        tessera: bool,
        keys_up: bool,
        scrubs:  Vec<PathBuf>,
        kicks:   u32,
    }

    #[derive(Clone)]
    struct Fake(Rc<RefCell<World>>);

    impl Kernel for Fake {
        fn info(&self, dir: &Path) -> io::Result<GroupInfo> {
            let w = self.0.borrow();
            if !w.tessera {
                return Err(io::Error::from_raw_os_error(libc::ENOTTY));
            }
            let mut i = GroupInfo { volume_uuid: UUID, ..Default::default() };
            let root = dir.ancestors().find(|a| w.domains.contains_key(*a));
            if let Some(r) = root {
                i.domain_id = 1;
                if r == dir {
                    i.flags |= GroupInfo::ROOT;
                }
                i.group_id = w.domains[r];
                if i.group_id != 0 {
                    match w.groups.get(&i.group_id) {
                        None => i.flags |= GroupInfo::NO_RECORD | GroupInfo::BAD,
                        Some(k) => {
                            i.key_id = *k;
                            i.flags |= if w.loaded.contains(k) { GroupInfo::OPEN } else { GroupInfo::LOCKED };
                        }
                    }
                }
            }
            Ok(i)
        }
        fn make_domain(&self, dir: &Path) -> io::Result<()> {
            self.0.borrow_mut().domains.entry(dir.to_path_buf()).or_insert(0);
            Ok(())
        }
        fn group_create(&self, _dir: &Path, key_id: &[u8; 16], group: u64) -> io::Result<()> {
            let mut w = self.0.borrow_mut();
            if !w.loaded.contains(key_id) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            w.groups.insert(group, *key_id);
            Ok(())
        }
        fn join_group(&self, dir: &Path, group: u64) -> io::Result<()> {
            let mut w = self.0.borrow_mut();
            if fs::read_dir(dir)?.next().is_some() {
                return Err(io::Error::from_raw_os_error(libc::ENOTEMPTY));
            }
            let k = *w.groups.get(&group).ok_or(io::Error::from_raw_os_error(libc::ENOENT))?;
            if !w.loaded.contains(&k) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            w.domains.insert(dir.to_path_buf(), group);
            Ok(())
        }
        fn erase(&self, _dir: &Path, group: u64) -> io::Result<()> {
            let mut w = self.0.borrow_mut();
            let k = *w.groups.get(&group).ok_or(io::Error::from_raw_os_error(libc::ENOENT))?;
            if w.loaded.contains(&k) {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            let roots: Vec<PathBuf> = w.domains.iter().filter(|(_, g)| **g == group).map(|(p, _)| p.clone()).collect();
            for r in roots {
                for e in fs::read_dir(&r)? {
                    remove_any(&e?.path())?;
                }
                w.domains.remove(&r);
            }
            w.groups.remove(&group);
            Ok(())
        }
    }

    impl ScrubQueue for Fake {
        fn want(&self, dir: &Path) -> io::Result<()> {
            let mut w = self.0.borrow_mut();
            // Queued while the old copy is still there.
            assert!(fs::read_dir(dir).unwrap().any(|e| {
                e.unwrap().file_name().to_string_lossy().ends_with(".unencrypted")
            }));
            if !w.scrubs.iter().any(|d| d == dir) {
                w.scrubs.push(dir.to_path_buf());
            }
            Ok(())
        }
        fn kick(&self) {
            self.0.borrow_mut().kicks += 1;
        }
    }

    impl Keys for Fake {
        fn mint(&self, _u: &str, _p: &str, uuid: &[u8; 16], group: u64, _l: &str) -> Result<[u8; 16], KeyError> {
            let mut w = self.0.borrow_mut();
            if !w.keys_up {
                return Err(KeyError::Unavailable("no arcad".into()));
            }
            assert_eq!(uuid, &UUID);
            w.minted += 1;
            let k = [w.minted as u8; 16];
            w.arca.insert(k, group);
            w.loaded.insert(k);
            Ok(k)
        }
        fn load(&self, _u: &str, _p: &str, key_id: &[u8; 16], _uuid: &[u8; 16], group: u64) -> Result<(), KeyError> {
            let mut w = self.0.borrow_mut();
            w.loads += 1;
            if w.arca.get(key_id) != Some(&group) {
                return Err(KeyError::Denied);
            }
            w.loaded.insert(*key_id);
            Ok(())
        }
        fn retire(&self, _u: &str, _p: &str, key_id: &[u8; 16]) -> Result<(), KeyError> {
            let mut w = self.0.borrow_mut();
            w.arca.remove(key_id).ok_or(KeyError::Denied)?;
            w.loaded.remove(key_id);
            Ok(())
        }
    }

    struct T {
        _d: tempfile::TempDir,
        home: PathBuf,
        db: PathBuf,
        fake: Fake,
        groups: Groups,
    }

    fn setup() -> T {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home/alice");
        fs::create_dir_all(&home).unwrap();
        let db = d.path().join("db/groups.toml");
        let fake = Fake(Rc::new(RefCell::new(World { tessera: true, keys_up: true, ..Default::default() })));
        T { groups: Groups::load(&db).unwrap(), _d: d, home, db, fake }
    }

    fn req(t: &T, persona: &str) -> PersonaFoldersRequest {
        let me = fs::metadata(&t.home).unwrap();
        PersonaFoldersRequest {
            person: "alice".into(),
            persona: persona.into(),
            folders: ["Documents", "Downloads"].iter().map(|f| t.home.join(f).display().to_string()).collect(),
            uid: me.uid(),
            gid: me.gid(),
            mode: 0o2770 & 0o0770,
        }
    }

    fn run(t: &mut T, r: &PersonaFoldersRequest) -> Response {
        let fake = t.fake.clone();
        Folders { kernel: &fake, keys: &fake, scrub: &fake, groups: &mut t.groups, path: &t.db }.prepare(r)
    }

    fn erase(t: &mut T, persona: &str) -> Response {
        let fake = t.fake.clone();
        Folders { kernel: &fake, keys: &fake, scrub: &fake, groups: &mut t.groups, path: &t.db }
            .erase(&EraseFoldersRequest { person: "alice".into(), persona: persona.into() })
    }

    fn logout(t: &T) {
        t.fake.0.borrow_mut().loaded.clear();
    }

    fn ready(r: &Response) -> (bool, Vec<String>) {
        match r {
            Response::FoldersReady { encrypted, moved } => (*encrypted, moved.clone()),
            other => panic!("not ready: {other:?}"),
        }
    }

    fn group_of(t: &T, f: &str) -> u64 {
        t.fake.0.borrow().domains.get(&t.home.join(f)).copied().unwrap_or(0)
    }

    #[test]
    fn new_folders_are_made_in_the_personas_group_and_expected() {
        let mut t = setup();
        let r = req(&t, "personal");
        assert_eq!(ready(&run(&mut t, &r)), (true, vec![]));
        let g = t.groups.find("alice", "personal").unwrap().clone();
        assert_eq!(group_of(&t, "Documents"), g.group_id);
        assert_eq!(group_of(&t, "Downloads"), g.group_id);
        assert_eq!(g.folders.len(), 2);
        assert_eq!(fs::metadata(t.home.join("Documents")).unwrap().mode() & 0o777, 0o770);
        // Saved: a restart finds the same expectation.
        assert_eq!(Groups::load(&t.db).unwrap().find("alice", "personal"), Some(&g));

        // Next login: the key loads again, nothing new is minted.
        logout(&t);
        assert_eq!(ready(&run(&mut t, &r)), (true, vec![]));
        assert_eq!(t.fake.0.borrow().minted, 1);
        assert_eq!(t.fake.0.borrow().loads, 1);
        // Already open: no call to arcad at all.
        run(&mut t, &r);
        assert_eq!(t.fake.0.borrow().loads, 1);
    }

    #[test]
    fn a_persona_gets_a_group_of_its_own() {
        let mut t = setup();
        let mut a = req(&t, "personal");
        a.folders.truncate(1);
        ready(&run(&mut t, &a));
        let mut b = req(&t, "work");
        b.folders = vec![t.home.join("Work").display().to_string()];
        ready(&run(&mut t, &b));
        assert_ne!(group_of(&t, "Documents"), group_of(&t, "Work"));
        // Work's folder in Personal's list: it's in another group.
        let mut c = req(&t, "personal");
        c.folders = vec![t.home.join("Work").display().to_string()];
        assert!(matches!(run(&mut t, &c), Response::FoldersRefused { .. }));
    }

    #[test]
    fn old_files_move_in_and_keep_what_they_were() {
        let mut t = setup();
        let docs = t.home.join("Documents");
        fs::create_dir_all(docs.join("sub/deeper")).unwrap();
        fs::write(docs.join("a.txt"), b"alpha").unwrap();
        fs::write(docs.join("sub/deeper/b.bin"), vec![7u8; 100_000]).unwrap();
        fs::set_permissions(docs.join("a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink("a.txt", docs.join("link")).unwrap();
        let old = fs::metadata(docs.join("a.txt")).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let r = req(&t, "personal");
        let (enc, moved) = ready(&run(&mut t, &r));
        assert!(enc);
        assert_eq!(moved, vec![docs.display().to_string()]);
        let g = t.groups.find("alice", "personal").unwrap().group_id;
        assert_eq!(group_of(&t, "Documents"), g);
        assert_eq!(fs::read(docs.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(docs.join("sub/deeper/b.bin")).unwrap(), vec![7u8; 100_000]);
        assert_eq!(fs::metadata(docs.join("a.txt")).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::metadata(docs.join("a.txt")).unwrap().modified().unwrap(), old);
        assert_eq!(fs::read_link(docs.join("link")).unwrap(), PathBuf::from("a.txt"));
        assert!(!parked(&docs).exists(), "the old copy stays");
        // Its volume is queued for a scrub (before the delete: see the
        // fake) and the scrub started.
        assert_eq!(t.fake.0.borrow().scrubs, vec![t.home.clone()]);
        assert_eq!(t.fake.0.borrow().kicks, 1);
        // Nothing moves twice, and nothing more is scrubbed.
        assert_eq!(ready(&run(&mut t, &r)), (true, vec![]));
        assert_eq!(t.fake.0.borrow().kicks, 1);
    }

    #[test]
    fn an_interrupted_move_finishes_next_time() {
        let mut t = setup();
        let r = req(&t, "personal");
        ready(&run(&mut t, &r));
        // As if the move stopped after the folder was made again.
        let p = parked(&t.home.join("Downloads"));
        fs::create_dir(&p).unwrap();
        fs::write(p.join("late.pdf"), b"pdf").unwrap();
        logout(&t);
        let (_, moved) = ready(&run(&mut t, &r));
        assert_eq!(moved, vec![t.home.join("Downloads").display().to_string()]);
        assert_eq!(fs::read(t.home.join("Downloads/late.pdf")).unwrap(), b"pdf");
        assert!(!p.exists());
        assert_eq!(t.fake.0.borrow().kicks, 1);
    }

    #[test]
    fn a_folder_stripped_of_its_group_or_moved_to_another_is_refused() {
        let mut t = setup();
        let r = req(&t, "personal");
        ready(&run(&mut t, &r));
        logout(&t);
        // An offline edit: Documents' domain no longer names the group.
        t.fake.0.borrow_mut().domains.insert(t.home.join("Documents"), 0);
        match run(&mut t, &r) {
            Response::FoldersRefused { folder, detail } => {
                assert!(folder.ends_with("Documents"));
                assert!(detail.contains("no longer"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        // Into another group.
        t.fake.0.borrow_mut().domains.insert(t.home.join("Documents"), 999);
        assert!(matches!(run(&mut t, &r), Response::FoldersRefused { .. }));
        // Its group with another key: refused too.
        let g = t.groups.find("alice", "personal").unwrap().group_id;
        t.fake.0.borrow_mut().domains.insert(t.home.join("Documents"), g);
        t.fake.0.borrow_mut().groups.insert(g, [0xEE; 16]);
        assert!(matches!(run(&mut t, &r), Response::FoldersRefused { .. }));
    }

    #[test]
    fn without_tessera_or_keys_folders_stay_plain_until_they_are_encrypted() {
        let mut t = setup();
        t.fake.0.borrow_mut().keys_up = false;
        let r = req(&t, "personal");
        assert_eq!(ready(&run(&mut t, &r)), (false, vec![]));
        assert!(t.groups.find("alice", "personal").is_none());
        t.fake.0.borrow_mut().keys_up = true;
        t.fake.0.borrow_mut().tessera = false;
        assert_eq!(ready(&run(&mut t, &r)), (false, vec![]));
        // Once encrypted, a missing Tessera is refused, not plain.
        t.fake.0.borrow_mut().tessera = true;
        ready(&run(&mut t, &r));
        t.fake.0.borrow_mut().tessera = false;
        assert!(matches!(run(&mut t, &r), Response::FoldersRefused { .. }));
    }

    #[test]
    fn a_removed_persona_is_crypto_erased() {
        let mut t = setup();
        let r = req(&t, "work");
        ready(&run(&mut t, &r));
        fs::write(t.home.join("Documents/secret"), b"s").unwrap();
        let g = t.groups.find("alice", "work").unwrap().group_id;
        assert!(matches!(erase(&mut t, "work"), Response::FoldersErased { erased: true }));
        assert!(!t.home.join("Documents").exists());
        assert!(t.groups.find("alice", "work").is_none());
        assert!(!t.fake.0.borrow().groups.contains_key(&g));
        assert!(t.fake.0.borrow().arca.is_empty());
        assert!(matches!(erase(&mut t, "work"), Response::FoldersErased { erased: false }));
        // Added again under the same name: a new group and key.
        ready(&run(&mut t, &r));
        assert_ne!(t.groups.find("alice", "work").unwrap().group_id, g);
    }

    #[test]
    fn an_erase_that_cant_finish_now_finishes_later() {
        let mut t = setup();
        let r = req(&t, "work");
        ready(&run(&mut t, &r));
        let g = t.groups.find("alice", "work").unwrap().clone();
        // The key is retired, but the kernel says busy this time.
        t.fake.0.borrow_mut().arca.clear();
        let k = unhex16(&g.key_id).unwrap();
        t.fake.0.borrow_mut().loaded.insert([0; 16]);
        t.fake.0.borrow_mut().groups.insert(g.group_id, k);
        {
            let mut w = t.fake.0.borrow_mut();
            w.loaded.insert(k);
        }
        assert!(matches!(erase(&mut t, "work"), Response::Error { .. }));
        assert!(t.groups.find("alice", "work").unwrap().retired);
        // The next login (any persona) finishes it.
        t.fake.0.borrow_mut().loaded.remove(&k);
        let mut p = req(&t, "personal");
        p.folders = vec![t.home.join("Pictures").display().to_string()];
        ready(&run(&mut t, &p));
        assert!(t.groups.find("alice", "work").is_none());
        assert!(!t.home.join("Documents").exists());
    }

    #[test]
    fn names_and_paths_are_checked() {
        let mut t = setup();
        let mut r = req(&t, "../x");
        assert!(matches!(run(&mut t, &r), Response::Error { .. }));
        r.persona = "work".into();
        r.folders = vec!["relative".into()];
        assert!(matches!(run(&mut t, &r), Response::Error { .. }));
        r.folders = vec![format!("{}/../bob/Documents", t.home.display())];
        assert!(matches!(run(&mut t, &r), Response::Error { .. }));
    }
}
