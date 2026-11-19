//! Per-app overlay VOLUMES (`docs/spec/portcullis.md` §4.1, §4.2), one per
//! person and app (`docs/spec/homes.md` §3, H7).
//!
//! Each app's writable overlay is its own Tessera volume. For a person with a
//! home it lives in that home: mounted at `~/.atrium/apps/<app.id>/`, backed
//! by `~/.atrium/apps/<app.id>.img`, so everything that is the person's is one
//! tree. Without a person (or a home) it is the older machine-wide place:
//! mounted at `/var/lib/atrium/overlays/<app.id>`, backed by an image file
//! under `/var/lib/atrium/overlay-vols/<app.id>.img`; a person's first launch
//! moves an overlay found there into their home ([`adopt_legacy`]). Either
//! way through `md(4)`:
//!
//! ```text
//! mkfs-tessera --create -s <MiB> overlay-vols/<id>.img
//! mdconfig -a -t vnode -f overlay-vols/<id>.img        -> mdN
//! mount -t tessera -o tessera.quota_bytes=N /dev/mdN overlays/<id>/
//! ```
//!
//! WHY A VOLUME AND NOT A DIRECTORY. Quota-scoped `statfs` is per-MOUNT and
//! cannot be per-path — `VFS_STATFS` receives a mount, not a vnode
//! (tessera-quotas.md §3.6). A per-directory quota therefore does NOT stop a
//! jail's `df` from reporting the whole pool, which is the dedup existence
//! oracle's first channel (tessera-fs.md §20.1): write a candidate chunk,
//! watch free space, learn whether that content already exists somewhere else
//! on the system. Giving each overlay its own mount with a whole-FS quota
//! closes that structurally rather than behaviourally — the jail sees its own
//! quota, and the number it sees is derived from LOGICAL bytes, so a duplicate
//! consumes full quota whether or not it deduped physically.
//!
//! That is also why dedup inside an overlay volume can stay `global`: the
//! observable number is content-independent by construction, and the volume
//! holds only that app's own content, so there is nothing of anyone else's to
//! probe for. The `deferred` policy (one full write per duplicate) is the
//! fallback for when an overlay is still a plain directory on the shared
//! volume — see [`Backing::Directory`].
//!
//! This crate shells out to `mkfs-tessera`, `mdconfig(8)` and `mount(8)`, so
//! unlike `portcullis-jail` it is FreeBSD-only in effect. It is a separate
//! crate precisely to keep that out of `portcullis-jail`, which is deliberately
//! pure and host-testable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const OVERLAYS_DIR:     &str = "/var/lib/atrium/overlays";
pub const OVERLAY_VOLS_DIR: &str = "/var/lib/atrium/overlay-vols";

/// Largest overlay we will build.
///
/// ★ NOT A FILESYSTEM LIMIT ANY MORE. Until 2026-09-13 overlays were capped at
/// 384 MiB because the image is created by `ftruncate` and Tessera refused a
/// truncate-extend past `TESSERA_WRITE_MATERIALIZE_MAX` (512 MiB): it built the
/// whole new file in one contiguous kernel buffer. `tessera_fs_extend_sparse`
/// now appends hole windows instead, so an image can be as large as the store
/// holding it (the kmod refuses an extend past the volume's own capacity).
///
/// What bounds it now is the COST OF CREATING THE IMAGE, which is linear in its
/// nominal size even though the image is all holes: every hole chunk is a
/// manifest record, and the whole extend runs under the store's flush gate so a
/// crash cannot leave it half-extended. Measured on the dev VM:
///
/// | image   | truncate | metadata |
/// |---------|----------|----------|
/// | 1 GiB   | 0.25 s   | 1.5 MiB  |
/// | 2 GiB   | 0.52 s   | 1.6 MiB  |
/// | 4 GiB   | 0.99 s   | 3.1 MiB  |
/// | 7 GiB   | 1.76 s   | 4.6 MiB  |
///
/// An 8 GiB quota needs a 10 GiB image: about 2.5 s during which every other
/// publish on the shared volume waits, once, at the app's first launch. That is
/// the line. Large media belongs in a user volume, not in an app's overlay.
///
/// This is a QUOTA, not an allocation: an unused image costs its metadata and
/// nothing else (a fresh 2.5 GiB image grew the store by 15 MiB).
pub const MAX_QUOTA_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Overlay size when the manifest does not ask for one — an app's own writable
/// state (config, caches, pkg-installed files from its setup phase). Creating
/// its image takes about a third of a second.
pub const DEFAULT_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;

/// Smallest overlay we will build. Below this the filesystem's own fixed costs
/// (a 1 MiB journal plus a metadata reserve of `max(1024 sectors, total/16)`)
/// eat most of the volume.
pub const MIN_QUOTA_BYTES: u64 = 64 * 1024 * 1024;

/// How the app's overlay is actually backed, once [`ensure_mounted`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backing {
    /// Its own Tessera volume, mounted with a whole-FS quota. The oracle is
    /// closed structurally; dedup inside may stay `global`.
    Volume { device: String },
    /// A plain directory on the shared volume — the pre-2026-09-12 layout.
    /// The caller MUST still arm `deferred` dedup on it, because nothing else
    /// closes the oracle in this shape.
    Directory,
}

/// Where an overlay is mounted and the image that backs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub dir: PathBuf,
    pub img: PathBuf,
    /// The person whose home holds it (None: the machine-wide place).
    pub person: Option<String>,
}

/// The machine-wide place (before homes.md, and for launches with no person).
pub fn legacy_place(app_id: &str) -> Place {
    Place { dir: PathBuf::from(OVERLAYS_DIR).join(app_id), img: PathBuf::from(OVERLAY_VOLS_DIR).join(format!("{app_id}.img")), person: None }
}

/// A name that is one plain path segment (an app id, a login name).
pub fn plain_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.len() <= 255 && !s.contains('/') && !s.contains('\0')
}

/// The person's home from the password database, when it is a real
/// directory (not `/nonexistent`, not a link) and not `/`.
pub fn home_of(person: &str) -> Option<PathBuf> {
    if !plain_segment(person) {
        return None;
    }
    let c = std::ffi::CString::new(person).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwnam_r(c.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
    if r != 0 || res.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    let dir = PathBuf::from(unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }.to_string_lossy().into_owned());
    let m = fs::symlink_metadata(&dir).ok()?;
    (m.is_dir() && dir != Path::new("/") && dir.is_absolute()).then_some(dir)
}

/// `~/.atrium/apps` in `home`, made if missing: `.atrium` and `apps` are
/// root's, 0755, so nothing but root can swap an app's folder for a link
/// (homes.md §2). Refuses either when it is a link or not root's.
pub fn apps_root(home: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let mut at = home.to_path_buf();
    for part in [".atrium", "apps"] {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() && m.uid() == 0 => {}
            Ok(_) => return Err(format!("{} is not root's own folder", at.display())),
            Err(_) => {
                fs::DirBuilder::new().mode(0o755).create(&at).map_err(|e| format!("create {}: {e}", at.display()))?;
                std::os::unix::fs::chown(&at, Some(0), Some(0)).map_err(|e| format!("chown {}: {e}", at.display()))?;
            }
        }
    }
    Ok(at)
}

/// Where `person`'s overlay of `app_id` lives: in their home when they have
/// one, else the machine-wide place.
pub fn place(person: Option<&str>, app_id: &str) -> Place {
    place_instance(person, app_id, None)
}

/// Where a named persona's app overlays live, under the home: one folder
/// per persona, `~/.atrium/persona-apps/<persona>/`, so each can be a
/// domain of that persona's key group (tessera-encryption.md E6). Before
/// E6 they were `apps/<id>@<persona>`, in among Personal's.
pub const PERSONA_APPS: &str = "persona-apps";

/// `~/.atrium/persona-apps/<persona>` (not made here).
pub fn persona_apps_dir(home: &Path, persona: &str) -> PathBuf {
    home.join(".atrium").join(PERSONA_APPS).join(persona)
}

fn persona_ok(persona: &str) -> bool {
    plain_segment(persona) && !persona.contains('@') && !persona.starts_with('.')
}

/// The overlay of the instance of `app_id` in `persona` (personas.md §3):
/// `apps/<id>` for Personal, `persona-apps/<persona>/<id>` for a named
/// persona. A named persona needs the person's home (there is no
/// machine-wide place for it) and a name that is one plain segment without
/// `@`; otherwise `None`.
pub fn persona_place(person: &str, app_id: &str, persona: &str) -> Option<Place> {
    if !persona_ok(persona) || app_id.contains('@') || !plain_segment(app_id) {
        return None;
    }
    let home = home_of(person)?;
    let dir = persona_apps_dir(&home, persona);
    Some(Place { dir: dir.join(app_id), img: dir.join(format!("{app_id}.img")), person: Some(person.to_string()) })
}

/// The pre-E6 place of a persona instance, `apps/<id>@<persona>`.
fn old_persona_place(home: &Path, person: &str, app_id: &str, persona: &str) -> Place {
    let apps = home.join(".atrium").join("apps");
    let key = format!("{app_id}@{persona}");
    Place { dir: apps.join(&key), img: apps.join(format!("{key}.img")), person: Some(person.to_string()) }
}

/// `(app id, persona)` of each pre-E6 persona overlay left in `apps/`.
fn old_persona_overlays(home: &Path) -> Vec<(String, String)> {
    let mut v = std::collections::BTreeSet::new();
    if let Ok(rd) = fs::read_dir(home.join(".atrium").join("apps")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = name.strip_suffix(".img").unwrap_or(&name);
            if let Some((id, persona)) = key.split_once('@') {
                if plain_segment(id) && persona_ok(persona) {
                    v.insert((id.to_string(), persona.to_string()));
                }
            }
        }
    }
    v.into_iter().collect()
}

/// Move `person`'s pre-E6 persona overlays (`apps/<id>@<persona>`, all
/// personas or only `only`) to their persona's folder, unmounting each
/// first. Done before the folders are encrypted, so it is a rename on
/// the plain volume; a persona folder that is already encrypted gets a
/// copy (EXDEV). One that is in use, or whose new place is taken, stays
/// and is logged. Returns how many moved.
pub fn relocate_persona_overlays(person: &str, only: Option<&str>) -> usize {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let Some(home) = home_of(person) else { return 0 };
    let mut moved = 0;
    for (id, persona) in old_persona_overlays(&home) {
        if only.is_some_and(|o| o != persona) {
            continue;
        }
        let old = old_persona_place(&home, person, &id, &persona);
        let Some(new) = persona_place(person, &id, &persona) else { continue };
        let r = (|| -> Result<(), String> {
            if new.img.exists() || fs::read_dir(&new.dir).map(|mut r| r.next().is_some()).unwrap_or(false) {
                return Err(format!("{} is taken", new.dir.display()));
            }
            let parent = new.dir.parent().ok_or("no parent")?;
            for d in [parent.parent().ok_or("no parent")?, parent] {
                match fs::symlink_metadata(d) {
                    Ok(m) if m.is_dir() && m.uid() == 0 => {}
                    Ok(_) => return Err(format!("{} is not root's own folder", d.display())),
                    Err(_) => {
                        fs::DirBuilder::new().mode(0o700).create(d).map_err(|e| format!("create {}: {e}", d.display()))?;
                        std::os::unix::fs::chown(d, Some(0), Some(0)).map_err(|e| format!("chown {}: {e}", d.display()))?;
                    }
                }
            }
            teardown(&old)?;
            if old.img.exists() {
                move_path(&old.img, &new.img)?;
                let _ = fs::remove_dir(&old.dir);
            } else if old.dir.exists() {
                let _ = fs::remove_dir(&new.dir);
                move_path(&old.dir, &new.dir)?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => {
                moved += 1;
                eprintln!("portcullis: moved {person}'s {id}@{persona} overlay into {}", new.dir.display());
            }
            Err(e) => eprintln!("portcullis: {person}'s {id}@{persona} overlay stays in apps/ for now: {e}"),
        }
    }
    moved
}

/// Every overlay place in `person`'s home: Personal's in `apps/` and each
/// persona's in `persona-apps/<persona>/` (and any pre-E6 one).
pub fn person_places(person: &str) -> Vec<Place> {
    let Some(home) = home_of(person) else { return Vec::new() };
    let mut v = Vec::new();
    let mut scan = |dir: PathBuf| {
        let mut keys = std::collections::BTreeSet::new();
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let key = name.strip_suffix(".img").unwrap_or(&name).to_string();
                if plain_segment(&key) && !key.starts_with('.') {
                    keys.insert(key);
                }
            }
        }
        for k in keys {
            v.push(Place { dir: dir.join(&k), img: dir.join(format!("{k}.img")), person: Some(person.to_string()) });
        }
    };
    scan(home.join(".atrium").join("apps"));
    if let Ok(rd) = fs::read_dir(home.join(".atrium").join(PERSONA_APPS)) {
        for e in rd.flatten() {
            let p = e.file_name().to_string_lossy().into_owned();
            if persona_ok(&p) {
                scan(persona_apps_dir(&home, &p));
            }
        }
    }
    v
}

/// Unmount every overlay of `person` (at logout, before their keys go:
/// the images are in their key groups, E6). The ones that stay mounted
/// (busy), by place.
pub fn teardown_person(person: &str) -> Vec<String> {
    let mut busy = Vec::new();
    for pl in person_places(person) {
        if let Err(e) = teardown(&pl) {
            busy.push(format!("{}: {e}", pl.dir.display()));
        }
    }
    busy
}

/// [`place`] for an instance: Personal falls back as `place` does; a named
/// persona without a home place is `None` (never the Personal or legacy one).
pub fn place_instance(person: Option<&str>, app_id: &str, persona: Option<&str>) -> Place {
    if let (Some(person), Some(persona)) = (person, persona) {
        if let Some(p) = persona_place(person, app_id, persona) {
            return p;
        }
    }
    if let Some(p) = person {
        if let (Some(home), true) = (home_of(p), plain_segment(app_id)) {
            let apps = home.join(".atrium").join("apps");
            return Place { dir: apps.join(app_id), img: apps.join(format!("{app_id}.img")), person: Some(p.to_string()) };
        }
    }
    legacy_place(app_id)
}

/// Every place an overlay of `app_id` may be: the machine-wide one and each
/// person's under `/home` (for `portcullis remove`).
pub fn all_places(app_id: &str) -> Vec<Place> {
    let mut v = vec![legacy_place(app_id)];
    if let Ok(rd) = fs::read_dir("/home") {
        for e in rd.flatten() {
            let person = e.file_name().to_string_lossy().into_owned();
            let p = place(Some(&person), app_id);
            if p.person.is_some() && (p.img.exists() || p.dir.exists()) {
                v.push(p);
            }
            // And each persona's instance (personas.md §3), where it is
            // now and where it was before E6 (`apps/<id>@<persona>`).
            let home = Path::new("/home").join(&person);
            for (id, persona) in old_persona_overlays(&home) {
                if id == app_id {
                    v.push(old_persona_place(&home, &person, &id, &persona));
                }
            }
            if let Ok(rd) = fs::read_dir(home.join(".atrium").join(PERSONA_APPS)) {
                for e in rd.flatten() {
                    let persona = e.file_name().to_string_lossy().into_owned();
                    if let Some(p) = persona_place(&person, app_id, &persona) {
                        if p.img.exists() || p.dir.exists() {
                            v.push(p);
                        }
                    }
                }
            }
        }
    }
    v
}

/// Image size for a given quota: the quota plus the filesystem's own overhead.
///
/// A Tessera volume spends a flat 1 MiB on the journal and reserves
/// `max(1024 sectors, total/16)` — about 6.25% — for metadata, and a volume
/// whose data zone is exactly the quota cannot actually hold a quota's worth
/// of data. 25% with a 64 MiB floor is deliberately generous: the headroom is
/// nominal (the image is allocate-on-write), whereas an overlay that hits
/// ENOSPC below its own advertised quota is a real and confusing failure.
pub fn image_mib_for_quota(quota_bytes: u64) -> u64 {
    let quota_mib = quota_bytes / (1024 * 1024);
    quota_mib + std::cmp::max(64, quota_mib / 4)
}

/// The largest quota whose image fits in `bytes` — the inverse of
/// [`image_mib_for_quota`], rounded down to a whole MiB.
///
/// Used two ways. Against the STORE, so a manifest asking for more than the
/// store can hold gets clamped with a message instead of an `EFBIG` from
/// `mkfs-tessera` and a silent fall back to a directory. And against an
/// EXISTING IMAGE, whose inner filesystem was sized at creation and cannot
/// grow: mounting it with a larger quota would make `df` advertise space the
/// volume does not have, and the app would hit ENOSPC below its own quota.
pub fn max_quota_fitting(bytes: u64) -> u64 {
    let mib = bytes / (1024 * 1024);
    // q + max(64, q/4) <= mib. Above 320 MiB the 25% term dominates, so
    // q <= mib * 4/5; below it the 64 MiB floor does.
    let q = if mib >= 320 { mib * 4 / 5 } else { mib.saturating_sub(64) };
    q * 1024 * 1024
}

/// Total size of the filesystem holding `dir`, in bytes.
fn store_bytes(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut sfs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c outlives the call; sfs is a zeroed stack local of the right type.
    if unsafe { libc::statvfs(c.as_ptr(), &mut sfs) } != 0 {
        return None;
    }
    Some(sfs.f_blocks as u64 * sfs.f_frsize as u64)
}

/// Is `dir` itself a mount point, and if so of what filesystem type and from
/// what device? `None` when `dir` is merely inside some other mount.
fn mount_at(dir: &Path) -> Option<(String, String)> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c outlives the call; sfs is a zeroed stack local of the right type.
    if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 {
        return None;
    }
    let cstr = |buf: &[libc::c_char]| -> String {
        buf.iter().take_while(|&&ch| ch != 0).map(|&ch| ch as u8 as char).collect()
    };
    let on = cstr(&sfs.f_mntonname);
    if Path::new(&on) != dir {
        return None;   // inside a mount, not the mount point itself
    }
    Some((cstr(&sfs.f_fstypename), cstr(&sfs.f_mntfromname)))
}

/// Is the overlay at `pl` currently mounted as its own Tessera volume?
pub fn mounted_volume(pl: &Place) -> Option<String> {
    match mount_at(&pl.dir) {
        Some((fstype, from)) if fstype == "tessera" => Some(from),
        _ => None,
    }
}

fn have(tool: &str) -> bool {
    Command::new("which").arg(tool).output()
        .map(|o| o.status.success()).unwrap_or(false)
}

/// The `md(4)` device already serving `img`, if any.
///
/// Attaching the same image twice gives two devices over one file, which is
/// corruption waiting to happen, so every attach has to look first. `mdconfig
/// -lv` prints `md0<TAB>vnode<TAB>512M<TAB>/path/to.img`.
fn existing_md(img: &Path) -> Option<String> {
    let out = Command::new("mdconfig").arg("-lv").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let want = img.to_string_lossy();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let unit = f.next()?;
        if line.split_whitespace().any(|w| w == want) {
            return Some(unit.to_string());
        }
    }
    None
}

fn attach(img: &Path) -> Result<String, String> {
    if let Some(u) = existing_md(img) {
        return Ok(u);
    }
    let out = Command::new("mdconfig")
        .args(["-a", "-t", "vnode", "-f"])
        .arg(img)
        .output()
        .map_err(|e| format!("mdconfig -a {}: {e}", img.display()))?;
    if !out.status.success() {
        return Err(format!("mdconfig -a {}: {}", img.display(),
            String::from_utf8_lossy(&out.stderr).trim()));
    }
    let unit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if unit.is_empty() {
        return Err(format!("mdconfig -a {} printed no device", img.display()));
    }
    Ok(unit)
}

fn detach(unit: &str) {
    let _ = Command::new("mdconfig").args(["-d", "-u", unit.trim_start_matches("md")])
        .status();
}

/// Step 0 of launch (`portcullis.md` §4.2): make sure the app's overlay volume
/// exists and is mounted at `overlays/<app.id>`, and say how it ended up
/// backed.
///
/// Idempotent — a second call on a mounted overlay is a `statfs` and nothing
/// else. The volume deliberately stays mounted across launches; it holds the
/// app's persistent state, and only [`teardown`] takes it down.
///
/// FALLBACK POLICY, which is not symmetric:
///
///   - The image does not exist yet and something goes wrong (no
///     `mkfs-tessera`, no `mdconfig`, mount refuses): fall back to a plain
///     directory and say so loudly. There is no state to lose on a first run,
///     and refusing to launch at all would be a worse answer than launching
///     with the older, behavioural mitigation.
///   - The image DOES exist and cannot be mounted: hard error. The app's
///     persistent state lives inside that image, and quietly substituting an
///     empty directory would present as data loss.
pub fn ensure_mounted(pl: &Place, app_id: &str, quota_bytes: u64) -> Result<Backing, String> {
    let (dir, img) = (pl.dir.clone(), pl.img.clone());

    if let Some(device) = mounted_volume(pl) {
        return Ok(Backing::Volume { device });
    }
    if pl.person.is_some() {
        let home_apps = dir.parent().ok_or("an overlay with no folder above it")?;
        apps_root(home_apps.parent().and_then(Path::parent).ok_or("no home above .atrium")?)?;
        adopt_legacy(pl, app_id)?;
        if let Some(device) = mounted_volume(pl) {
            return Ok(Backing::Volume { device });
        }
    }

    /* Clamp, but never silently: an app that declared 20 GiB and quietly got
     * 8 GiB would look like a Tessera bug the first time it filled up. */
    let mut quota = quota_bytes.clamp(MIN_QUOTA_BYTES, MAX_QUOTA_BYTES);
    if quota != quota_bytes {
        eprintln!("portcullis: {app_id} asked for a {} MiB overlay; using {} MiB \
                   (overlays are {}-{} MiB; see MAX_QUOTA_BYTES for why)",
            quota_bytes / (1024 * 1024), quota / (1024 * 1024),
            MIN_QUOTA_BYTES / (1024 * 1024), MAX_QUOTA_BYTES / (1024 * 1024));
    }

    fs::create_dir_all(&dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;

    let img_exists = img.exists();
    let fail = |msg: String| -> Result<Backing, String> {
        if img_exists {
            Err(format!("{msg} — refusing to launch {app_id} with an empty \
                         overlay while its state is in {}", img.display()))
        } else {
            eprintln!("portcullis: WARNING: {msg}; {app_id}'s overlay stays a \
                       plain directory on the shared volume, so the dedup \
                       existence oracle is closed only by the `deferred` \
                       policy (portcullis.md §4.1)");
            Ok(Backing::Directory)
        }
    };

    if !have("mdconfig") || !have("mkfs-tessera") {
        return fail("mdconfig(8) or mkfs-tessera is not available".into());
    }

    /* MIGRATION GUARD. An app installed under the old layout has its state in
     * the overlay DIRECTORY. Mounting a fresh empty volume on top of it would
     * hide every one of those files — the data is still on disk, but the app
     * sees an empty overlay, which presents exactly as data loss. Leave it a
     * directory and say what has to happen; the `deferred` fallback still
     * closes the oracle, so this is a slower shape, not an unsafe one. */
    if !img_exists {
        let occupied = fs::read_dir(&dir).map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if occupied {
            eprintln!("portcullis: {app_id}'s overlay already holds state as a \
                       plain directory; NOT mounting a volume over it (that \
                       would hide the state). It keeps the `deferred` dedup \
                       policy. To convert: stop the app, move {} aside, launch \
                       once to build the volume, then copy the state back in.",
                dir.display());
            return Ok(Backing::Directory);
        }
    }

    let vols = img.parent().unwrap_or(Path::new(OVERLAY_VOLS_DIR)).to_path_buf();
    if !img_exists {
        if let Err(e) = fs::create_dir_all(&vols) {
            return fail(format!("create {}: {e}", vols.display()));
        }
        /* The image can be no larger than the store holding it: Tessera
         * refuses to extend a file past its own volume's capacity. Clamp here
         * rather than let mkfs-tessera die with EFBIG and demote the app to a
         * directory. */
        if let Some(store) = store_bytes(&vols) {
            let fits = max_quota_fitting(store);
            if fits < MIN_QUOTA_BYTES {
                return fail(format!("{} is only {} MiB, too \
                    small for a {} MiB overlay", vols.display(), store / (1024 * 1024),
                    MIN_QUOTA_BYTES / (1024 * 1024)));
            }
            if quota > fits {
                eprintln!("portcullis: {app_id}'s {} MiB overlay does not fit \
                           the {} MiB store; using {} MiB",
                    quota / (1024 * 1024), store / (1024 * 1024),
                    fits / (1024 * 1024));
                quota = fits;
            }
        }
        let mib = image_mib_for_quota(quota);
        let out = Command::new("mkfs-tessera")
            .args(["--create", "-s", &mib.to_string()])
            .arg(&img)
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let _ = fs::remove_file(&img);
                return fail(format!("mkfs-tessera {}: {}", img.display(),
                    String::from_utf8_lossy(&o.stderr).trim()));
            }
            Err(e) => {
                let _ = fs::remove_file(&img);
                return fail(format!("mkfs-tessera {}: {e}", img.display()));
            }
        }
        eprintln!("portcullis: created {} ({} MiB image for a {} MiB overlay)",
            img.display(), mib, quota / (1024 * 1024));
    }

    /* An existing image's filesystem was sized when it was made and does not
     * grow with the manifest. Mounting it with a bigger quota would have `df`
     * advertise space the volume cannot hold. Cap to what the image fits, and
     * say how to get more. */
    if img_exists {
        if let Ok(meta) = fs::metadata(&img) {
            let fits = max_quota_fitting(meta.len());
            if quota > fits {
                eprintln!("portcullis: {app_id} asks for a {} MiB overlay but its \
                           existing image holds {} MiB; mounting at {} MiB. A \
                           larger overlay needs the image recreated (portcullis \
                           remove --keep-overlay does not do that).",
                    quota / (1024 * 1024), fits / (1024 * 1024),
                    fits / (1024 * 1024));
                quota = fits;
            }
        }
    }

    let unit = match attach(&img) {
        Ok(u) => u,
        Err(e) => return fail(e),
    };
    let device = format!("/dev/{unit}");

    let st = Command::new("mount")
        .args(["-t", "tessera", "-o", &format!("tessera.quota_bytes={quota}")])
        .arg(&device)
        .arg(&dir)
        .status();
    match st {
        Ok(s) if s.success() => {}
        other => {
            detach(&unit);
            let what = match other {
                Ok(s)  => format!("exited {s}"),
                Err(e) => format!("{e}"),
            };
            /* A first-run image we just built and cannot mount is useless and
             * would be re-found as "existing" next time, turning a soft
             * fallback into a hard failure forever. Remove it. */
            if !img_exists {
                let _ = fs::remove_file(&img);
            }
            return fail(format!("mount -t tessera {device} {}: {what}",
                dir.display()));
        }
    }

    eprintln!("portcullis: mounted {} at {} with a {} MiB quota",
        device, dir.display(), quota / (1024 * 1024));
    Ok(Backing::Volume { device })
}

/// Unmount the app's overlay volume and release its `md(4)` device. Leaves the
/// image file alone — the app's state is in there. Used when the app is
/// uninstalled with its overlay kept, and as the first half of [`destroy`].
pub fn teardown(pl: &Place) -> Result<(), String> {
    let dir = &pl.dir;
    let Some(device) = mounted_volume(pl) else { return Ok(()) };

    let st = Command::new("umount").arg(&dir).status();
    match st {
        Ok(s) if s.success() => {}
        Ok(s)  => return Err(format!("umount {}: exited {s}", dir.display())),
        Err(e) => return Err(format!("umount {}: {e}", dir.display())),
    }
    if let Some(unit) = device.strip_prefix("/dev/") {
        detach(unit);
    }
    Ok(())
}

/// Unmount the overlay volume and delete its backing image. This DESTROYS the
/// app's persistent state.
///
/// `portcullis remove` used to `rm -rf` the overlay directory. Against a
/// mounted volume that empties the volume's contents and then fails to remove
/// the mount point, leaving the volume mounted and the image on disk — the
/// state is gone but the space is not.
pub fn destroy(pl: &Place) -> Result<(), String> {
    teardown(pl)?;
    let img = &pl.img;
    if img.exists() {
        fs::remove_file(&img)
            .map_err(|e| format!("remove {}: {e}", img.display()))?;
    }
    let dir = &pl.dir;
    if dir.exists() {
        // Now an ordinary empty directory (or a directory-backed overlay).
        fs::remove_dir_all(&dir)
            .map_err(|e| format!("remove {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Destroy every app instance place of `person`'s persona `persona`
/// (`persona-apps/<persona>/<id>` and its image, or a pre-E6
/// `apps/<id>@<persona>`; personas.md §4.1, Remove). The app ids whose
/// places went; one that couldn't go is logged and left.
pub fn remove_persona_places(person: &str, persona: &str) -> Vec<String> {
    let mut gone = Vec::new();
    if !persona_ok(persona) {
        return gone;
    }
    let Some(home) = home_of(person) else { return gone };
    let mut places: Vec<(String, Place)> = old_persona_overlays(&home).into_iter()
        .filter(|(_, p)| p == persona)
        .map(|(id, p)| (id.clone(), old_persona_place(&home, person, &id, &p)))
        .collect();
    let dir = persona_apps_dir(&home, persona);
    let mut ids = std::collections::BTreeSet::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = name.strip_suffix(".img").unwrap_or(&name);
            if plain_segment(key) && !key.contains('@') && !key.starts_with('.') {
                ids.insert(key.to_string());
            }
        }
    }
    places.extend(ids.into_iter().filter_map(|id| persona_place(person, &id, persona).map(|p| (id, p))));
    for (id, pl) in places {
        match destroy(&pl) {
            Ok(()) => gone.push(id),
            Err(e) => eprintln!("portcullis: {person}'s {id}@{persona} not removed: {e}"),
        }
    }
    gone
}

/// Move an overlay from the machine-wide place into `pl` (a person's home),
/// the first time that person launches the app after homes.md (§3,
/// "Migration"). Only when `pl` holds nothing yet: a home that already has
/// the app's state keeps it, and the machine-wide copy is left alone.
///
/// A volume is unmounted and its image moved (renamed, or copied and then
/// removed when the home is on another file system); a plain-directory
/// overlay is moved the same way. The volume is mounted again at its new
/// place by the caller.
pub fn adopt_legacy(pl: &Place, app_id: &str) -> Result<(), String> {
    let old = legacy_place(app_id);
    if pl.person.is_none() || pl.img.exists() {
        return Ok(());
    }
    let occupied = |d: &Path| fs::read_dir(d).map(|mut r| r.next().is_some()).unwrap_or(false);
    if occupied(&pl.dir) {
        return Ok(());
    }
    if old.img.exists() {
        teardown(&old)?;
        move_path(&old.img, &pl.img)?;
        let _ = fs::remove_dir(&old.dir);
        eprintln!("portcullis: moved {app_id}'s overlay into {} (homes.md §3)", pl.img.display());
    } else if occupied(&old.dir) && mount_at(&old.dir).is_none() {
        let _ = fs::remove_dir(&pl.dir);
        move_path(&old.dir, &pl.dir)?;
        eprintln!("portcullis: moved {app_id}'s overlay folder into {} (homes.md §3)", pl.dir.display());
    }
    Ok(())
}

/// Rename, or copy (keeping modes, owners and times) and remove when the two
/// are on different file systems.
fn move_path(from: &Path, to: &Path) -> Result<(), String> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let st = Command::new("cp").arg("-Rp").arg(from).arg(to).status().map_err(|e| format!("cp {}: {e}", from.display()))?;
            if !st.success() {
                let _ = if to.is_dir() { fs::remove_dir_all(to) } else { fs::remove_file(to) };
                return Err(format!("cp -Rp {} {}: {st}", from.display(), to.display()));
            }
            if from.is_dir() { fs::remove_dir_all(from) } else { fs::remove_file(from) }.map_err(|e| format!("remove {}: {e}", from.display()))
        }
        Err(e) => Err(format!("move {} to {}: {e}", from.display(), to.display())),
    }
}

/// Parse a size like `512M`, `2G`, `1024K` or a bare byte count, as used by
/// `[resources]` in `atrium.toml`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() { return None; }
    let (digits, mult) = match s.as_bytes()[s.len() - 1].to_ascii_uppercase() {
        b'K' => (&s[..s.len() - 1], 1024u64),
        b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        b'T' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _    => (s, 1),
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("2G"),   Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("4k"),   Some(4096));
        assert_eq!(parse_size(" 8M "), Some(8 * 1024 * 1024));
        assert_eq!(parse_size(""),     None);
        assert_eq!(parse_size("M"),    None);
        assert_eq!(parse_size("hello"), None);
    }

    #[test]
    fn size_overflow_is_rejected_not_wrapped() {
        assert_eq!(parse_size("99999999999999999999T"), None);
    }

    #[test]
    fn image_leaves_room_for_the_quota() {
        // Overhead is the journal plus a ~6.25% metadata reserve, so the image
        // must exceed the quota by more than that at every size.
        for quota_mib in [64u64, 256, 1024, 8192] {
            let img = image_mib_for_quota(quota_mib * 1024 * 1024);
            let reserve = img / 16 + 1;
            assert!(img - reserve > quota_mib,
                "{quota_mib} MiB quota: {img} MiB image only leaves \
                 {} MiB", img - reserve);
        }
    }

    #[test]
    fn max_quota_fitting_is_the_inverse_of_image_size() {
        /* The image built for the returned quota must fit in the space it was
         * computed from — at every size, including the small stores where the
         * 64 MiB headroom floor dominates — and it must not be needlessly
         * pessimistic (within a few MiB of the space). */
        for mib in [128u64, 200, 320, 480, 1000, 1024, 8192, 10_000, 65_536] {
            let bytes = mib * 1024 * 1024;
            let q = max_quota_fitting(bytes);
            let img = image_mib_for_quota(q);
            assert!(img <= mib, "{mib} MiB space: {} MiB quota needs {img} MiB",
                q / (1024 * 1024));
            assert!(mib - img <= 4, "{mib} MiB space: only {img} MiB used");
        }
        assert_eq!(max_quota_fitting(0), 0);
        assert_eq!(max_quota_fitting(32 * 1024 * 1024), 0);
    }

    #[test]
    fn defaults_are_inside_the_bounds() {
        assert!(MIN_QUOTA_BYTES <= DEFAULT_QUOTA_BYTES);
        assert!(DEFAULT_QUOTA_BYTES <= MAX_QUOTA_BYTES);
        /* The pre-2026-09-13 overlays were 480 MiB images. An existing one
         * must still mount, at the quota it was built for. */
        assert_eq!(max_quota_fitting(480 * 1024 * 1024), 384 * 1024 * 1024);
    }

    #[test]
    fn paths_are_per_app() {
        let l = legacy_place("org.atrium.edit");
        assert_eq!(l.dir, PathBuf::from("/var/lib/atrium/overlays/org.atrium.edit"));
        assert_eq!(l.img, PathBuf::from("/var/lib/atrium/overlay-vols/org.atrium.edit.img"));
        // No such person: the machine-wide place.
        assert_eq!(place(Some("nobody-here-at-all"), "org.atrium.edit"), l);
        assert_eq!(place(Some("../etc"), "org.atrium.edit"), l);
        assert_eq!(place(None, "org.atrium.edit"), l);
    }

    #[test]
    fn a_person_with_a_home_gets_theirs() {
        // root's home is a real folder on any test host.
        let Some(home) = home_of("root") else { return };
        let p = place(Some("root"), "org.atrium.edit");
        assert_eq!(p.dir, home.join(".atrium/apps/org.atrium.edit"));
        assert_eq!(p.img, home.join(".atrium/apps/org.atrium.edit.img"));
        assert_eq!(p.person.as_deref(), Some("root"));
        assert!(!plain_segment("a/b") && !plain_segment("..") && plain_segment("org.atrium.edit"));
    }

    #[test]
    fn moving_a_folder_keeps_its_contents() {
        let t = std::env::temp_dir().join(format!("ovl-move-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        fs::create_dir_all(t.join("a/sub")).unwrap();
        fs::write(t.join("a/sub/state"), b"kept").unwrap();
        move_path(&t.join("a"), &t.join("b")).unwrap();
        assert_eq!(fs::read(t.join("b/sub/state")).unwrap(), b"kept");
        assert!(!t.join("a").exists());
        let _ = fs::remove_dir_all(&t);
    }
}
