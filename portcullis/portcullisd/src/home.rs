//! The person's home (docs/spec/homes.md §2, scrinium.md §9 H1): the
//! folders they see, the `files-<person>` group that opens them to the file
//! manager, and `~/.atrium`, root's, where app overlays and session state
//! live.
//!
//! Nothing runs as the person: the folders are theirs, group
//! `files-<person>`, mode 2770 (setgid, so what the file manager makes keeps
//! the group), and the one app given `home-files` (Scrinium) runs as its own
//! per-app uid with that group added at launch.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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

/// A group's id, from the group database.
pub fn group_gid(group: &str) -> Option<u32> { gid_of(group) }

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
/// and so is a locked Arca: the folders aren't handed out. Plain folders
/// are handed out only where `sealed_or_allowed` allows it.
fn encrypt_folders(person: &str, persona: &str, dirs: &[PathBuf], uid: u32, gid: u32) -> Result<(), String> {
    // The same rule as app data (mail.md §6.4a A1): plain only where it
    // is allowed.
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    let named = Some(persona).filter(|p| *p != "personal");
    let asked = encrypt_dirs(person, persona, dirs, uid, gid, 0o2770);
    sealed_or_allowed(person, named, "folders", &home.join(".atrium"), asked, dirs).map(|_| ())
}

/// What atrium-volumes said about the folders it was asked to encrypt.
struct Ready {
    encrypted: bool,
    /// Some folders are in use and move in at the next login.
    waiting: bool,
}

/// Why atrium-volumes gave no `FoldersReady`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fail {
    /// A folder should be encrypted and isn't, or is in the wrong group.
    Refused,
    /// atrium-volumes answered an error or something unexpected.
    Error,
    /// atrium-volumes was not reached, or the exchange failed.
    Unreachable,
}

/// Every trouble is an error: the caller decides whether plain data is
/// allowed.
fn encrypt_dirs(person: &str, persona: &str, dirs: &[PathBuf], uid: u32, gid: u32, mode: u32) -> Result<Ready, (Fail, String)> {
    use atrium_volumes::protocol::{PersonaFoldersRequest, Request, Response};
    let Some(mut c) = volumes() else { return Err((Fail::Unreachable, "atrium-volumes not reached".into())) };
    let req = Request::PersonaFolders(PersonaFoldersRequest {
        person: person.into(),
        persona: persona.into(),
        folders: dirs.iter().map(|d| d.display().to_string()).collect(),
        uid,
        gid,
        mode,
    });
    match c.send(&req) {
        Ok(Response::FoldersReady { encrypted, moved, waiting }) => {
            for m in moved {
                eprintln!("portcullisd: {person}'s {m}: old files moved into the encrypted folder");
            }
            for w in &waiting {
                eprintln!("portcullisd: {person}'s {w} is in use: it is encrypted once it is free (next login)");
            }
            if !encrypted {
                eprintln!("portcullisd: {person}'s {persona} folders are not encrypted (no Tessera keys here)");
            }
            Ok(Ready { encrypted, waiting: !waiting.is_empty() })
        }
        Ok(Response::FoldersRefused { folder, detail }) => Err((Fail::Refused, format!("{folder} refused: {detail}"))),
        Ok(Response::Error { detail }) => Err((Fail::Error, detail)),
        Ok(other) => Err((Fail::Error, format!("atrium-volumes: unexpected {other:?}"))),
        Err(e) => Err((Fail::Unreachable, format!("atrium-volumes: {e}"))),
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

/// The parts of `~/.atrium` that hold the person's app data (E6,
/// tessera-encryption.md §13.2), in their Personal key group: app overlays
/// (`apps`) and session state (`state`), root's 0755; workspaces and the
/// workspaces' object cache, root's with the files group, 0750.
pub const APP_DATA_ROOT: &[&str] = &["apps", "state"];
pub const APP_DATA_FILES: &[&str] = &["workspaces", "cache"];

/// What a launch's app data is (mail.md §6.4a A1, A2): the kernel's answer,
/// handed to the app as `ATRIUM_APP_DATA`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppData {
    /// In the expected key group, with the group open.
    Sealed,
    /// Plain, allowed: the root-owned `plain_app_data` opt-out on a machine
    /// whose kernel reports no key groups here, or Personal's one login
    /// while its folders wait to be moved in.
    Plain,
}

impl AppData {
    pub fn env_value(self) -> &'static str {
        match self { AppData::Sealed => "sealed", AppData::Plain => "plain" }
    }
}

/// The policy that may opt a machine out of encrypted app data.
const VOLUMES_POLICY: &str = "/etc/atrium/volumes.policy.toml";
/// Root-only record of each person's one plain login (migration): the
/// boot it was first used in, or `spent`.
const ALLOWANCE_DIR: &str = "/var/db/atrium/plain-app-data";
const SPENT: &str = "spent";

fn allowance_path(person: &str) -> PathBuf { Path::new(ALLOWANCE_DIR).join(person) }

/// The session of `person` ended: a plain login they used is spent.
pub fn app_data_session_ended(person: &str) {
    if std::fs::symlink_metadata(allowance_path(person)).is_ok() {
        if let Err(e) = write_allowance(person, SPENT) {
            eprintln!("portcullisd: {person}'s plain login could not be recorded as spent: {e}");
        }
    }
}

/// Replace the record (root-only directory, 0600, synced).
fn write_allowance(person: &str, value: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(ALLOWANCE_DIR)?;
    let tmp = Path::new(ALLOWANCE_DIR).join(format!(".{person}.new"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(value.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, allowance_path(person))
}

/// This boot, as `kern.boottime`: a record from an earlier boot is spent,
/// so a power-off or a crash that never sent SessionEnded still spends it.
fn boot_id() -> Result<String, String> {
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is NUL-terminated; `tv` and `len` are live stack
    // values whose size is passed; no new value is set.
    #[allow(unsafe_code)]
    let r = unsafe {
        libc::sysctlbyname(c"kern.boottime".as_ptr(), (&mut tv as *mut libc::timeval).cast(), &mut len, std::ptr::null_mut(), 0)
    };
    if r != 0 || len != std::mem::size_of::<libc::timeval>() {
        return Err(format!("kern.boottime: {}", std::io::Error::last_os_error()));
    }
    Ok(format!("boot {}.{:06}", tv.tv_sec, tv.tv_usec))
}

/// What the person's record says about the plain login for this boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Allowance {
    /// Never used: this is the one.
    Unused,
    /// Being used in this boot's session (portcullisd restarted in it).
    ThisBoot,
    /// Used and over, or anything unreadable.
    Spent,
}

fn allowance(record: Option<&str>, boot: &str) -> Allowance {
    match record {
        None => Allowance::Unused,
        Some(r) if r == boot => Allowance::ThisBoot,
        Some(_) => Allowance::Spent,
    }
}

/// The record, read: `Err` (unreadable) counts as spent.
fn allowance_now(person: &str, boot: &str) -> Allowance {
    match std::fs::read_to_string(allowance_path(person)) {
        Ok(r) => allowance(Some(&r), boot),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => allowance(None, boot),
        Err(e) => {
            eprintln!("portcullisd: {ALLOWANCE_DIR}/{person}: {e}: the plain login counts as spent");
            Allowance::Spent
        }
    }
}

/// The kernel's answer for one directory, asked on its own descriptor.
fn group_info(dir: &Path) -> Result<atrium_volumes::ffi::GroupInfo, String> {
    atrium_volumes::ffi::tessera_group_info_here(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// In a key group whose key is loaded, and intact.
fn sealed_open(info: &atrium_volumes::ffi::GroupInfo) -> bool {
    use atrium_volumes::ffi::GroupInfo;
    info.group_id != 0 && info.has(GroupInfo::OPEN) && !info.has(GroupInfo::BAD)
}

/// Whether this machine encrypts, whatever the config says: the volume
/// holding the person's data has key groups (any group, anyone's), or a
/// folder is in one. Only "not Tessera" (ENOTTY, EOPNOTSUPP) and a folder
/// not there yet count as no; any other error counts as yes.
fn kernel_has_groups(at: &Path, checked: &[PathBuf]) -> bool {
    use atrium_volumes::ffi::GroupInfo;
    let mut dirs: Vec<PathBuf> = vec![at.to_path_buf()];
    dirs.extend(checked.iter().cloned());
    dirs.extend(APP_DATA_ROOT.iter().chain(APP_DATA_FILES).map(|d| at.join(d)));
    if let Ok(rd) = std::fs::read_dir(at.join(portcullis_overlay::PERSONA_APPS)) {
        dirs.extend(rd.flatten().map(|e| e.path()));
    }
    dirs.iter().any(|d| match atrium_volumes::ffi::tessera_group_info(d) {
        Ok(g) => g.group_id != 0 || g.has(GroupInfo::VOLUME_GROUPS),
        // ENOENT: a folder not made yet. Every caller has atrium-volumes
        // (or ensure_*) make the folders first, so a missing one is not a
        // removed one; if that changes, ENOENT must count as yes.
        Err(e) => !matches!(e.raw_os_error(), Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::ENOENT)),
    })
}

/// `plain_app_data` in the root-owned volumes policy. A policy that
/// doesn't parse opts nothing out.
fn plain_opt_out() -> bool {
    match atrium_volumes::policy::Policy::load(Path::new(VOLUMES_POLICY)) {
        Ok(p) => p.plain_app_data,
        Err(e) => {
            eprintln!("portcullisd: {VOLUMES_POLICY}: {e}: no plain_app_data opt-out");
            false
        }
    }
}

/// The person's app data in their key groups before anything mounts or
/// uses it (E6): Personal's parts of `~/.atrium`, or a named persona's
/// overlays folder `~/.atrium/persona-apps/<persona>` (root's, 0700). A
/// persona overlay from before E6 (`apps/<id>@<persona>`) is moved to its
/// persona's folder first, so it never enters Personal's group.
///
/// It fails closed (mail.md §6.4a A1; `sealed_or_allowed`).
pub fn ensure_app_data(person: &str, persona: Option<&str>) -> Result<AppData, String> {
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    portcullis_overlay::apps_root(&home)?;
    let at = home.join(".atrium");
    let named = persona.filter(|p| *p != "personal");
    let (asked, checked) = match named {
        None => {
            portcullis_overlay::relocate_persona_overlays(person, None);
            let gid = ensure_group(&files_group(person))?;
            let root: Vec<PathBuf> = APP_DATA_ROOT.iter().map(|d| at.join(d)).collect();
            let files: Vec<PathBuf> = APP_DATA_FILES.iter().map(|d| at.join(d)).collect();
            let asked = match encrypt_dirs(person, "personal", &root, 0, 0, 0o755) {
                Ok(x) => encrypt_dirs(person, "personal", &files, 0, gid, 0o750)
                    .map(|y| Ready { encrypted: x.encrypted && y.encrypted, waiting: x.waiting || y.waiting }),
                Err(e) => Err(e),
            };
            (asked, root.into_iter().chain(files).collect::<Vec<_>>())
        }
        Some(p) => {
            if !plain(p) {
                return Err(format!("{p:?} is not a named persona"));
            }
            ensure_root_dir(&at.join(portcullis_overlay::PERSONA_APPS))?;
            portcullis_overlay::relocate_persona_overlays(person, Some(p));
            let dir = portcullis_overlay::persona_apps_dir(&home, p);
            (encrypt_dirs(person, p, std::slice::from_ref(&dir), 0, 0, 0o700), vec![dir])
        }
    };
    sealed_or_allowed(person, named, "app data", &at, asked, &checked)
}

/// The A1 rule for one set of folders (app data, or the visible folders):
/// sealed only when atrium-volumes answered `encrypted` (it alone knows
/// each folder's expected group, and a wrong one is `FoldersRefused`) AND
/// the kernel, asked on each folder's own descriptor, reports it in a key
/// group that is open. Plain is allowed in two cases only: the root-owned
/// `plain_app_data` opt-out on a machine whose kernel reports no key
/// groups, and Personal's one login while atrium-volumes reports its
/// folders `waiting` (in use, moved in at the next login).
fn sealed_or_allowed(person: &str, named: Option<&str>, what: &str, at: &Path, asked: Result<Ready, (Fail, String)>, checked: &[PathBuf]) -> Result<AppData, String> {
    let which = named.unwrap_or("personal");
    // A refusal (a folder in the wrong group) always stops the launch.
    let asked = match asked {
        Err((Fail::Refused, e)) => return Err(e),
        other => other,
    };
    let kernel: Result<(), String> = checked.iter().try_for_each(|d| match group_info(d) {
        Ok(g) if sealed_open(&g) => Ok(()),
        Ok(_) => Err(format!("{} is not in an open key group", d.display())),
        Err(e) => Err(e),
    });
    let volumes_sealed = matches!(asked, Ok(Ready { encrypted: true, .. }));
    let why = match (&asked, &kernel) {
        (Err((_, e)), _) => e.clone(),
        (Ok(_), Err(k)) => k.clone(),
        (Ok(_), Ok(())) if !volumes_sealed => "atrium-volumes reports them not encrypted".into(),
        (Ok(_), Ok(())) => String::new(),
    };
    let sealed = volumes_sealed && kernel.is_ok();
    let boot = if sealed { Err(String::new()) } else { boot_id() };
    let facts = Facts {
        sealed,
        kernel_has_groups: !sealed && kernel_has_groups(at, checked),
        opt_out: !sealed && plain_opt_out(),
        personal: named.is_none(),
        waiting: asked.as_ref().is_ok_and(|r| r.waiting),
        allowance: match &boot {
            Ok(b) => allowance_now(person, b),
            Err(_) => Allowance::Spent,
        },
    };
    match decide(&facts) {
        Some(Why::Sealed) => Ok(AppData::Sealed),
        Some(Why::OptOut) => {
            eprintln!("portcullisd: {person}'s {which} {what} is PLAIN under plain_app_data in {VOLUMES_POLICY} ({why})");
            Ok(AppData::Plain)
        }
        Some(Why::Migrating) => {
            // Recorded before it is used, so a restart or a power-off
            // can't hand it out again in a later boot.
            let boot = boot.as_deref().unwrap_or_default();
            if facts.allowance == Allowance::Unused {
                write_allowance(person, boot).map_err(|e| format!("{person}'s {what}: not encrypted, and the one plain login could not be recorded ({e}): {why}"))?;
            }
            eprintln!("portcullisd: {person}'s Personal {what} is PLAIN for this one login, while its folders wait to be moved in ({why})");
            Ok(AppData::Plain)
        }
        None => {
            if facts.opt_out && facts.kernel_has_groups {
                eprintln!("portcullisd: plain_app_data in {VOLUMES_POLICY} is IGNORED: this volume has key groups");
            }
            Err(format!("{person}'s {which} {what}: not encrypted, and plain is not allowed here: {why}"))
        }
    }
}

/// Where `person`'s VPN profiles for `persona` (None: Personal) live
/// (vpn.md §7.1a): `.crypta` in that persona's app data, root's, 0700. As
/// Mail's accounts (mail.md §6.4a): a named persona's only when the kernel
/// reports its app data sealed; Personal's also where its app data may be
/// plain (the machine's opt-out, or its one migrating login).
pub fn vpn_profiles_dir(person: &str, persona: Option<&str>) -> Result<PathBuf, String> {
    let named = persona.filter(|p| *p != "personal");
    match ensure_app_data(person, named)? {
        AppData::Sealed => {}
        AppData::Plain if named.is_none() => {}
        AppData::Plain => return Err(format!("{person}'s {} app data is plain: no VPN profiles there", named.unwrap_or("personal"))),
    }
    let home = portcullis_overlay::home_of(person).ok_or_else(|| format!("{person} has no home"))?;
    let parent = match named {
        None => portcullis_overlay::apps_root(&home)?,
        Some(p) => portcullis_overlay::persona_apps_dir(&home, p),
    };
    match std::fs::symlink_metadata(&parent) {
        Ok(m) if m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0 => {}
        _ => return Err(format!("{} is not root's own folder", parent.display())),
    }
    let dir = parent.join(".crypta");
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", dir.display())),
    }
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() && m.uid() == 0 && m.mode() & 0o777 == 0o700 => Ok(dir),
        _ => Err(format!("{} is not root's, 0700", dir.display())),
    }
}

/// What A1's decision rests on.
#[derive(Clone, Copy, Debug)]
struct Facts {
    sealed: bool,
    kernel_has_groups: bool,
    opt_out: bool,
    personal: bool,
    waiting: bool,
    allowance: Allowance,
}

impl Default for Facts {
    fn default() -> Self {
        Facts { sealed: false, kernel_has_groups: false, opt_out: false, personal: false, waiting: false, allowance: Allowance::Spent }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why { Sealed, OptOut, Migrating }

/// `None`: refused.
fn decide(f: &Facts) -> Option<Why> {
    if f.sealed {
        Some(Why::Sealed)
    } else if f.opt_out && !f.kernel_has_groups {
        Some(Why::OptOut)
    } else if f.personal && f.waiting && f.allowance != Allowance::Spent {
        Some(Why::Migrating)
    } else {
        None
    }
}

#[cfg(test)]
mod app_data_tests {
    use super::{allowance, decide, Allowance, Facts, Why};

    #[test]
    fn plain_app_data_is_refused_unless_allowed() {
        assert_eq!(decide(&Facts::default()), None);
        assert_eq!(decide(&Facts { personal: true, ..Facts::default() }), None);
        assert_eq!(decide(&Facts { sealed: true, ..Facts::default() }), Some(Why::Sealed));
    }

    #[test]
    fn the_opt_out_counts_only_where_the_kernel_reports_no_groups() {
        let opt = Facts { opt_out: true, ..Facts::default() };
        assert_eq!(decide(&opt), Some(Why::OptOut));
        assert_eq!(decide(&Facts { kernel_has_groups: true, ..opt }), None);
    }

    #[test]
    fn personal_gets_one_plain_login_while_its_folders_wait() {
        let first = Facts { personal: true, waiting: true, allowance: Allowance::Unused, ..Facts::default() };
        assert_eq!(decide(&first), Some(Why::Migrating));
        assert_eq!(decide(&Facts { allowance: Allowance::ThisBoot, ..first }), Some(Why::Migrating), "portcullisd restarted in the same login");
        assert_eq!(decide(&Facts { allowance: Allowance::Spent, ..first }), None, "a second login refuses");
        assert_eq!(decide(&Facts { personal: false, ..first }), None, "a named persona never gets it");
    }

    #[test]
    fn the_plain_login_is_spent_by_a_new_boot_or_a_logout() {
        let boot = "boot 1795425660.000123";
        assert_eq!(allowance(None, boot), Allowance::Unused);
        assert_eq!(allowance(Some(boot), boot), Allowance::ThisBoot);
        assert_eq!(allowance(Some("boot 1795425660.000124"), boot), Allowance::Spent, "a power-off or crash, never logged out");
        assert_eq!(allowance(Some(super::SPENT), boot), Allowance::Spent, "logged out");
        assert_eq!(allowance(Some(""), boot), Allowance::Spent);
    }
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
    // Its app overlays' folder, emptied by remove_persona_places (E6); the
    // erase above took it if it was encrypted.
    let apps = portcullis_overlay::persona_apps_dir(&home, persona);
    if let Ok(m) = std::fs::symlink_metadata(&apps) {
        if m.is_dir() && m.uid() == 0 {
            std::fs::remove_dir_all(&apps).map_err(|e| format!("remove {}: {e}", apps.display()))?;
        }
    }
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
