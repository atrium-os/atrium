//! Per-app overlay VOLUMES (`docs/spec/portcullis.md` §4.1, §4.2).
//!
//! Each app's writable overlay is its own Tessera volume mounted at
//! `/var/lib/atrium/overlays/<app.id>`, backed by an image file under
//! `/var/lib/atrium/overlay-vols/<app.id>.img` through `md(4)`:
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

/// Where the app's overlay is mounted.
pub fn overlay_dir(app_id: &str) -> PathBuf {
    PathBuf::from(OVERLAYS_DIR).join(app_id)
}

/// The backing image for the app's overlay volume.
pub fn image_path(app_id: &str) -> PathBuf {
    PathBuf::from(OVERLAY_VOLS_DIR).join(format!("{app_id}.img"))
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

/// Is the app's overlay currently mounted as its own Tessera volume?
pub fn mounted_volume(app_id: &str) -> Option<String> {
    match mount_at(&overlay_dir(app_id)) {
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
pub fn ensure_mounted(app_id: &str, quota_bytes: u64) -> Result<Backing, String> {
    let dir = overlay_dir(app_id);
    let img = image_path(app_id);

    if let Some(device) = mounted_volume(app_id) {
        return Ok(Backing::Volume { device });
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

    if !img_exists {
        if let Err(e) = fs::create_dir_all(OVERLAY_VOLS_DIR) {
            return fail(format!("create {OVERLAY_VOLS_DIR}: {e}"));
        }
        /* The image can be no larger than the store holding it: Tessera
         * refuses to extend a file past its own volume's capacity. Clamp here
         * rather than let mkfs-tessera die with EFBIG and demote the app to a
         * directory. */
        if let Some(store) = store_bytes(Path::new(OVERLAY_VOLS_DIR)) {
            let fits = max_quota_fitting(store);
            if fits < MIN_QUOTA_BYTES {
                return fail(format!("{OVERLAY_VOLS_DIR} is only {} MiB, too \
                    small for a {} MiB overlay", store / (1024 * 1024),
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
pub fn teardown(app_id: &str) -> Result<(), String> {
    let dir = overlay_dir(app_id);
    let Some(device) = mounted_volume(app_id) else { return Ok(()) };

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
pub fn destroy(app_id: &str) -> Result<(), String> {
    teardown(app_id)?;
    let img = image_path(app_id);
    if img.exists() {
        fs::remove_file(&img)
            .map_err(|e| format!("remove {}: {e}", img.display()))?;
    }
    let dir = overlay_dir(app_id);
    if dir.exists() {
        // Now an ordinary empty directory (or a directory-backed overlay).
        fs::remove_dir_all(&dir)
            .map_err(|e| format!("remove {}: {e}", dir.display()))?;
    }
    Ok(())
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
        assert_eq!(overlay_dir("org.atrium.edit"),
            PathBuf::from("/var/lib/atrium/overlays/org.atrium.edit"));
        assert_eq!(image_path("org.atrium.edit"),
            PathBuf::from("/var/lib/atrium/overlay-vols/org.atrium.edit.img"));
    }
}
