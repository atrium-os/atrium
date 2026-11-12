//! `OpenCdm` (docs/spec/hospes.md §3, §8.4): the library and the store a content decryption
//! module host is given. portcullisd decides both; the caller names only the key system
//! (from a fixed table) and the origin it plays from.
//!
//! - The library is the system's install for the key system (`hospes-install` puts Widevine
//!   at `CDM_ROOT/com.widevine.alpha/current/`), opened read-only and limited to reading and
//!   mapping.
//! - The store is `STORE_ROOT/<caller app>/<key system>/<origin, percent-encoded>/`, made
//!   on first use, mode 0700, owned by the CDM host's uid, and limited to the record
//!   operations. One app can never name another app's stores: the app is the caller's peer
//!   uid's, from the launch registry.
// Two FFI calls: the user's ids (getpwnam_r) and cap_rights_limit.
#![allow(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const CDM_ROOT: &str = "/var/db/atrium/cdm";
pub const STORE_ROOT: &str = "/var/db/atrium/cdm-store";
/// The host's heap limit, MiB (the player's own default).
pub const MEM_MIB: u32 = 512;
/// Longest origin we key a store by.
pub const ORIGIN_MAX: usize = 200;

/// Key system -> library file name under `CDM_ROOT/<key system>/current/`. Clear Key is the
/// H3 test CDM (`hospes/test-cdm`), for checks.
pub const LIBRARIES: &[(&str, &str)] = &[("com.widevine.alpha", "libwidevinecdm.so"), ("org.w3.clearkey", "libclearkeycdm.so")];

pub fn library_path(key_system: &str) -> Option<PathBuf> {
    LIBRARIES.iter().find(|(k, _)| *k == key_system).map(|(k, f)| Path::new(CDM_ROOT).join(k).join("current").join(f))
}

/// The store's directory name for `origin`: printable ASCII, no spaces, at most
/// `ORIGIN_MAX` bytes; everything but `[A-Za-z0-9.-]` percent-encoded, so the name is one
/// path component (never `.` or `..`) and reads back as the origin (spec §8.4: listed in settings by origin).
pub fn origin_dir(origin: &str) -> Result<String, String> {
    if origin.is_empty() || origin.len() > ORIGIN_MAX || !origin.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!("origin: 1 to {ORIGIN_MAX} printable characters, no spaces"));
    }
    // A leading dot is encoded too, so the name is never `.`, `..` or hidden.
    Ok(origin.bytes().enumerate()
        .map(|(i, b)| if b.is_ascii_alphanumeric() || b == b'-' || (b == b'.' && i > 0) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect())
}

/// Open the library and the store for `app` (the caller), `key_system` and `origin`; the
/// store is made for `run_as` (the CDM host's user) if it is new.
pub fn open(app: &str, key_system: &str, origin: &str, run_as: &str) -> Result<(OwnedFd, OwnedFd), String> {
    let lib_path = library_path(key_system).ok_or_else(|| format!("unknown key system {key_system:?}"))?;
    let o = origin_dir(origin)?;
    if app.is_empty() || app.contains('/') || app.starts_with('.') {
        return Err("bad app id".into());
    }
    let lib = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(&lib_path)
        .map_err(|e| format!("{key_system} is not installed ({}: {e})", lib_path.display()))?;
    let (uid, gid) = user_ids(run_as).ok_or_else(|| format!("no user {run_as}"))?;
    let dir = Path::new(STORE_ROOT).join(app).join(key_system).join(o);
    make_store(&dir, uid, gid).map_err(|e| format!("store {}: {e}", dir.display()))?;
    let store = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&dir)
        .map_err(|e| format!("store {}: {e}", dir.display()))?;
    let (lib, store) = (OwnedFd::from(lib), OwnedFd::from(store));
    limit(&lib, &store).map_err(|e| format!("cap_rights_limit: {e}"))?;
    Ok((lib, store))
}

/// `STORE_ROOT` and each level below it: root's, 0711, so the host's uid can reach its own
/// leaf and list nothing; the leaf 0700, the host's.
fn make_store(dir: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let root = Path::new(STORE_ROOT);
    let mut p = root.to_path_buf();
    fs::create_dir_all(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o711))?;
    let comps: Vec<_> = dir.strip_prefix(root).map_err(std::io::Error::other)?.components().collect();
    for (i, c) in comps.iter().enumerate() {
        p.push(c);
        match fs::create_dir(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let md = fs::symlink_metadata(&p)?;
        if !md.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        if i + 1 == comps.len() {
            std::os::unix::fs::chown(&p, Some(uid), Some(gid))?;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700))?;
        } else {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o711))?;
        }
    }
    Ok(())
}

fn user_ids(name: &str) -> Option<(u32, u32)> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam_r (portcullisd is threaded) fills `pwd` from `buf`, both ours; we
    // copy two integers out before they go.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let r = libc::getpwnam_r(c.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
        (r == 0 && !result.is_null()).then(|| (pwd.pw_uid, pwd.pw_gid))
    }
}

/// The library: read and map; the store: the record operations (hospes-cdm narrows them
/// again before it loads anything).
fn limit(lib: &OwnedFd, store: &OwnedFd) -> std::io::Result<()> {
    #[cfg(target_os = "freebsd")]
    {
        use std::os::fd::AsRawFd;
        use libc::*;
        // SAFETY: __cap_rights_init fills a cap_rights_t we own from a zero-terminated list;
        // cap_rights_limit only reads it.
        unsafe {
            let mut r: cap_rights_t = std::mem::zeroed();
            __cap_rights_init(CAP_RIGHTS_VERSION, &mut r, CAP_READ, CAP_SEEK, CAP_FSTAT, CAP_MMAP_RX, 0u64);
            if cap_rights_limit(lib.as_raw_fd(), &r) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut r: cap_rights_t = std::mem::zeroed();
            __cap_rights_init(CAP_RIGHTS_VERSION, &mut r, CAP_LOOKUP, CAP_CREATE, CAP_READ, CAP_WRITE, CAP_SEEK, CAP_FSTAT, CAP_FTRUNCATE,
                CAP_FSYNC, CAP_RENAMEAT_SOURCE, CAP_RENAMEAT_TARGET, CAP_UNLINKAT, 0u64);
            if cap_rights_limit(store.as_raw_fd(), &r) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    let _ = (lib, store);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_become_one_readable_component() {
        assert_eq!(origin_dir("https://www.example.com:443").unwrap(), "https%3A%2F%2Fwww.example.com%3A443");
        for bad in ["", "a b", "x\n", &"a".repeat(ORIGIN_MAX + 1), "é"] {
            assert!(origin_dir(bad).is_err(), "{bad:?}");
        }
        assert!(!origin_dir("../../etc").unwrap().contains('/'));
        assert_eq!(origin_dir("..").unwrap(), "%2E.");
        assert_eq!(origin_dir(".").unwrap(), "%2E");
    }

    #[test]
    fn only_known_key_systems() {
        assert!(library_path("com.widevine.alpha").unwrap().ends_with("com.widevine.alpha/current/libwidevinecdm.so"));
        assert!(library_path("../x").is_none());
    }
}
