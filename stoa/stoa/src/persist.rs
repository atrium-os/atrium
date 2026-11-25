//! # Session-state persistence (S3a — survives `stoad` restart + host reboot)
//!
//! `stoad` holds its session table in memory; a restart (crash, upgrade) or a
//! host reboot loses it. S3a persists the **structure** — sessions, windows,
//! panes, layout, titles, and each pane's last-known cwd — so a restarted
//! `stoad` reconstructs the sessions and **respawns** the shells at their cwd
//! (the live processes themselves can't survive losing their parent / RAM;
//! that's the aspirational S3b, spec §5.5). Scrollback restore rides on top
//! of this later.
//!
//! This module is the pure data model + atomic file I/O — no daemon types, so
//! it round-trips in isolation. `stoad` converts its live `Inner` to a
//! [`PersistState`] on change and back on startup. The on-disk form is JSON
//! (spec §5.3 calls it `meta.json`); writes are crash-safe via temp-file +
//! `rename` (atomic on the same filesystem), so a crash mid-write leaves the
//! previous good snapshot intact.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// On-disk schema version — bump on an incompatible change so a stale file is
/// rejected (treated as "no state") rather than mis-parsed.
pub const VERSION: u32 = 1;

/// The metadata file name inside the state directory.
const META: &str = "meta.json";

/// Max scrollback lines persisted per window (retention; drop-oldest beyond).
/// A second backstop to the storage volume's own quota.
pub const SCROLLBACK_MAX: usize = 1000;

/// The whole persisted session table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistState {
    pub version: u32,
    pub sessions: Vec<PersistSession>,
}

/// One session: its name, where its shells run, the client size, and windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistSession {
    pub name: String,
    /// Whose shell it runs (the minting uid) and in which persona. A store
    /// portcullisd handed over holds one (person, persona)'s alone.
    #[serde(default)]
    pub owner: u32,
    #[serde(default = "personal")]
    pub persona: String,
    /// Spawn target string (`"session"` or `"jail:<id>"`) — same form `stoad`
    /// already keeps in `SessionState.target`.
    pub target: String,
    pub cols: u16,
    pub rows: u16,
    pub active: usize,
    pub last_active: usize,
    pub windows: Vec<PersistWindow>,
}

fn personal() -> String {
    "personal".into()
}

/// One window: liveness (closed windows stay as tombstones so `Ctrl-B <n>`
/// indices are stable), the focused pane, an optional title, and its panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistWindow {
    pub alive: bool,
    pub active_pane: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divider: Option<PersistDivider>,
    pub panes: Vec<PersistPane>,
}

/// A v1 two-pane split boundary (mirrors `stoad`'s `Divider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistDivider {
    Vertical(u16),
    Horizontal(u16),
}

/// One pane: its region and the shell's last-known cwd (where it respawns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistPane {
    pub alive: bool,
    pub top: u16,
    pub left: u16,
    pub prows: u16,
    pub pcols: u16,
    /// Last-known working directory, captured per-OS at persist time; `None`
    /// if it couldn't be read (respawn falls back to the shell default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl PersistState {
    pub fn new() -> PersistState {
        PersistState { version: VERSION, sessions: Vec::new() }
    }
}

impl Default for PersistState {
    fn default() -> Self {
        Self::new()
    }
}

/// Where sessions are kept: an open directory (stoa.md §5.4a). For a
/// person's sessions it is the one portcullisd opened in that persona's
/// key group and handed over; stoad never names a path of its own for it.
/// Every file is reached relative to it, never through a link: `meta.json`
/// and `scrollback/<hex(session)>/w<idx>.txt`.
pub struct Store {
    dir: OwnedFd,
    /// What it is, for the log (never a path stoad opens again).
    pub label: String,
    /// One writer at a time: the temporary names are per process.
    lock: Mutex<()>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Store({})", self.label)
    }
}

fn cstr(name: &str) -> io::Result<CString> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file name"));
    }
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a name"))
}

/// `openat` below `dir`, never following a link.
fn open_at(dir: RawFd, name: &str, flags: libc::c_int, mode: libc::c_uint) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    // SAFETY: a NUL-terminated single component, relative to an open
    // directory; the descriptor is ours alone on success.
    let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The subdirectory `name` of `dir`, made (0700) if `create`.
fn subdir(dir: RawFd, name: &str, create: bool) -> io::Result<OwnedFd> {
    if create {
        let c = cstr(name)?;
        // SAFETY: a NUL-terminated single component relative to `dir`.
        if unsafe { libc::mkdirat(dir, c.as_ptr(), 0o700) } != 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
        }
    }
    open_at(dir, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

fn unlink_at(dir: RawFd, name: &str, flags: libc::c_int) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: a NUL-terminated single component relative to `dir`.
    if unsafe { libc::unlinkat(dir, c.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The names in the directory open on `dir` (not `.` or `..`).
fn names(dir: RawFd) -> io::Result<Vec<String>> {
    // SAFETY: fdopendir takes over a descriptor of its own (a dup), closed
    // by closedir; readdir's entries are read before the next call.
    unsafe {
        let d = libc::dup(dir);
        if d < 0 {
            return Err(io::Error::last_os_error());
        }
        let dp = libc::fdopendir(d);
        if dp.is_null() {
            let e = io::Error::last_os_error();
            libc::close(d);
            return Err(e);
        }
        libc::rewinddir(dp);
        let mut out = Vec::new();
        loop {
            let e = libc::readdir(dp);
            if e.is_null() {
                break;
            }
            let n = std::ffi::CStr::from_ptr((*e).d_name.as_ptr()).to_string_lossy().into_owned();
            if n != "." && n != ".." {
                out.push(n);
            }
        }
        libc::closedir(dp);
        Ok(out)
    }
}

/// Write `name` in `dir` whole or not at all: a fresh temporary (0600,
/// O_EXCL), fsync, `renameat` over the old one.
fn write_atomic(dir: RawFd, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = format!(".{name}.tmp.{}", std::process::id());
    let _ = unlink_at(dir, &tmp, 0);
    let fd = open_at(dir, &tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
    let mut f = fs::File::from(fd);
    let r = f.write_all(bytes).and_then(|_| f.sync_all());
    if let Err(e) = r {
        let _ = unlink_at(dir, &tmp, 0);
        return Err(e);
    }
    let (a, b) = (cstr(&tmp)?, cstr(name)?);
    // SAFETY: two NUL-terminated single components relative to `dir`.
    if unsafe { libc::renameat(dir, a.as_ptr(), dir, b.as_ptr()) } != 0 {
        let e = io::Error::last_os_error();
        let _ = unlink_at(dir, &tmp, 0);
        return Err(e);
    }
    Ok(())
}

fn read_at(dir: RawFd, name: &str, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let f = fs::File::from(open_at(dir, name, libc::O_RDONLY, 0)?);
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a file"));
    }
    let mut out = Vec::new();
    f.take(max).read_to_end(&mut out)?;
    Ok(out)
}

/// The most read back of one file (meta or one window's scrollback).
const READ_MAX: u64 = 16 << 20;

impl Store {
    /// A directory handed over (portcullisd's, in a key group).
    pub fn from_fd(dir: OwnedFd, label: impl Into<String>) -> Store {
        Store { dir, label: label.into(), lock: Mutex::new(()) }
    }

    /// A directory named explicitly for a dev run (`$STOA_STATE`), made if
    /// missing.
    pub fn open_path(path: &Path) -> io::Result<Store> {
        fs::create_dir_all(path)?;
        let c = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the path"))?;
        // SAFETY: a NUL-terminated path; the descriptor is ours alone.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just opened.
        Ok(Store::from_fd(unsafe { OwnedFd::from_raw_fd(fd) }, path.display().to_string()))
    }

    /// Atomically write the session table (`meta.json`). A crash mid-write
    /// leaves the previous one.
    pub fn save_state(&self, st: &PersistState) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(st).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        write_atomic(self.dir.as_raw_fd(), META, &json)
    }

    /// The session table: empty (not an error) when absent, unreadable as
    /// JSON or of another [`VERSION`].
    pub fn load_state(&self) -> io::Result<PersistState> {
        let bytes = match read_at(self.dir.as_raw_fd(), META, READ_MAX) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PersistState::new()),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<PersistState>(&bytes) {
            Ok(s) if s.version == VERSION => Ok(s),
            _ => Ok(PersistState::new()),
        }
    }

    /// A window's scrollback (its last [`SCROLLBACK_MAX`] lines).
    pub fn save_scrollback(&self, session: &str, win_idx: usize, lines: &[String]) -> io::Result<()> {
        let start = lines.len().saturating_sub(SCROLLBACK_MAX);
        let body = lines[start..].join("\n");
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let sb = subdir(self.dir.as_raw_fd(), "scrollback", true)?;
        let d = subdir(sb.as_raw_fd(), &crate::to_hex(session.as_bytes()), true)?;
        write_atomic(d.as_raw_fd(), &format!("w{win_idx}.txt"), body.as_bytes())
    }

    /// A window's scrollback; empty when missing or unreadable.
    pub fn load_scrollback(&self, session: &str, win_idx: usize) -> Vec<String> {
        let read = || -> io::Result<Vec<u8>> {
            let sb = subdir(self.dir.as_raw_fd(), "scrollback", false)?;
            let d = subdir(sb.as_raw_fd(), &crate::to_hex(session.as_bytes()), false)?;
            read_at(d.as_raw_fd(), &format!("w{win_idx}.txt"), READ_MAX)
        };
        match read() {
            Ok(b) if !b.is_empty() => String::from_utf8_lossy(&b).split('\n').map(str::to_string).collect(),
            _ => Vec::new(),
        }
    }

    /// Remove a session's scrollback. Best-effort.
    pub fn remove_scrollback(&self, session: &str) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(sb) = subdir(self.dir.as_raw_fd(), "scrollback", false) else { return };
        let hex = crate::to_hex(session.as_bytes());
        if let Ok(d) = subdir(sb.as_raw_fd(), &hex, false) {
            for n in names(d.as_raw_fd()).unwrap_or_default() {
                let _ = unlink_at(d.as_raw_fd(), &n, 0);
            }
        }
        let _ = unlink_at(sb.as_raw_fd(), &hex, libc::AT_REMOVEDIR);
    }
}

/// The directory a dev run keeps every session in: `$STOA_STATE` when it is
/// set (and not `off`), else none. There is no default: a person's sessions
/// are kept only in the directory portcullisd hands over (stoa.md §5.4a).
pub fn dev_state_dir() -> Option<std::path::PathBuf> {
    match std::env::var_os("STOA_STATE") {
        Some(s) if !s.is_empty() && s != "off" => Some(s.into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sample() -> PersistState {
        PersistState {
            version: VERSION,
            sessions: vec![PersistSession {
                name: "work".into(),
                owner: 1001,
                persona: "work".into(),
                target: "session".into(),
                cols: 120,
                rows: 40,
                active: 1,
                last_active: 0,
                windows: vec![
                    PersistWindow {
                        alive: true,
                        active_pane: 0,
                        title: "edit".into(),
                        divider: None,
                        panes: vec![PersistPane {
                            alive: true,
                            top: 0,
                            left: 0,
                            prows: 40,
                            pcols: 120,
                            cwd: Some("/home/g/src".into()),
                        }],
                    },
                    PersistWindow {
                        alive: true,
                        active_pane: 1,
                        title: String::new(),
                        divider: Some(PersistDivider::Vertical(59)),
                        panes: vec![
                            PersistPane { alive: true, top: 0, left: 0, prows: 40, pcols: 59, cwd: None },
                            PersistPane {
                                alive: true,
                                top: 0,
                                left: 60,
                                prows: 40,
                                pcols: 60,
                                cwd: Some("/var/log".into()),
                            },
                        ],
                    },
                ],
            }],
        }
    }

    #[test]
    fn json_round_trips() {
        let s = sample();
        let json = serde_json::to_vec(&s).unwrap();
        let back: PersistState = serde_json::from_slice(&json).unwrap();
        assert_eq!(s, back);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("stoa-persist-{tag}-{}", std::process::id()))
    }

    fn store(dir: &Path) -> Store {
        Store::open_path(dir).unwrap()
    }

    #[test]
    fn save_then_load_is_identity() {
        let dir = scratch("id");
        let s = sample();
        let st = store(&dir);
        st.save_state(&s).unwrap();
        let back = st.load_state().unwrap();
        assert!(!dir.join(".meta.json.tmp").exists());
        assert_eq!(fs::metadata(dir.join(META)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(s, back);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_empty_state() {
        let dir = scratch("missing-xyz");
        let _ = fs::remove_dir_all(&dir);
        let s = store(&dir).load_state().unwrap();
        assert!(s.sessions.is_empty());
        assert_eq!(s.version, VERSION);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_version_is_empty_state() {
        let dir = scratch("ver");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(META), br#"{"version":999,"sessions":[]}"#).unwrap();
        let s = store(&dir).load_state().unwrap();
        assert!(s.sessions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_file_is_empty_state_not_error() {
        let dir = scratch("junk");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(META), b"not json at all }{").unwrap();
        let s = store(&dir).load_state().unwrap();
        assert!(s.sessions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrollback_round_trips_and_caps() {
        let dir = scratch("scroll");
        let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let st = store(&dir);
        st.save_scrollback("my/sess:1", 2, &lines).unwrap();
        let back = st.load_scrollback("my/sess:1", 2);
        assert_eq!(back, lines);
        // Different session / window is independent + missing → empty.
        assert!(st.load_scrollback("other", 0).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrollback_retention_drops_oldest() {
        let dir = scratch("retain");
        let lines: Vec<String> = (0..SCROLLBACK_MAX + 50).map(|i| format!("L{i}")).collect();
        let st = store(&dir);
        st.save_scrollback("s", 0, &lines).unwrap();
        let back = st.load_scrollback("s", 0);
        assert_eq!(back.len(), SCROLLBACK_MAX);
        assert_eq!(back[0], "L50"); // first 50 dropped
        assert_eq!(back.last().unwrap(), &format!("L{}", SCROLLBACK_MAX + 49));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_scrollback_clears_session() {
        let dir = scratch("rm");
        let st = store(&dir);
        st.save_scrollback("doomed", 0, &["x".to_string()]).unwrap();
        st.save_scrollback("doomed", 1, &["y".to_string()]).unwrap();
        assert!(!st.load_scrollback("doomed", 0).is_empty());
        st.remove_scrollback("doomed");
        assert!(st.load_scrollback("doomed", 0).is_empty());
        assert!(!dir.join("scrollback").join(crate::to_hex(b"doomed")).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_in_the_store_is_not_followed() {
        let dir = scratch("link");
        let out = scratch("link-out");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&out, dir.join("scrollback")).unwrap();
        let st = store(&dir);
        assert!(st.save_scrollback("s", 0, &["secret".to_string()]).is_err());
        assert!(fs::read_dir(&out).unwrap().next().is_none(), "nothing written through the link");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn no_state_dir_without_stoa_state() {
        std::env::remove_var("STOA_STATE");
        assert!(dev_state_dir().is_none());
    }
}
