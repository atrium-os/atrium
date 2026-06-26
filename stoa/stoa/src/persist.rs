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

use std::fs;
use std::io;
use std::path::Path;

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
    /// Spawn target string (`"session"` or `"jail:<id>"`) — same form `stoad`
    /// already keeps in `SessionState.target`.
    pub target: String,
    pub cols: u16,
    pub rows: u16,
    pub active: usize,
    pub last_active: usize,
    pub windows: Vec<PersistWindow>,
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

    /// Atomically write the metadata to `<dir>/meta.json`: temp file, fsync,
    /// `rename`. A crash mid-write leaves the prior snapshot — never torn.
    pub fn save_atomic(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_atomic(&dir.join(META), &json)
    }

    /// Load metadata from `<dir>/meta.json`. Returns empty state (not an error)
    /// when absent or a different [`VERSION`] — a fresh/upgraded `stoad` starts
    /// clean.
    pub fn load(dir: &Path) -> io::Result<PersistState> {
        let bytes = match fs::read(dir.join(META)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PersistState::new()),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<PersistState>(&bytes) {
            Ok(s) if s.version == VERSION => Ok(s),
            _ => Ok(PersistState::new()),
        }
    }
}

/// Atomic file write (temp beside target + fsync + rename). Same-fs rename is
/// atomic, so readers see either the old or the new whole file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Per-window scrollback file: `<dir>/scrollback/<hex(session)>/w<idx>.txt`.
/// The session name is hex-encoded so any name is a safe, collision-free path.
fn scrollback_file(dir: &Path, session: &str, win_idx: usize) -> std::path::PathBuf {
    dir.join("scrollback")
        .join(crate::to_hex(session.as_bytes()))
        .join(format!("w{win_idx}.txt"))
}

/// Persist a window's scrollback (newline-separated text, last
/// [`SCROLLBACK_MAX`] lines), atomically. Best-effort at the call site.
pub fn save_scrollback(dir: &Path, session: &str, win_idx: usize, lines: &[String]) -> io::Result<()> {
    let start = lines.len().saturating_sub(SCROLLBACK_MAX);
    let body = lines[start..].join("\n");
    write_atomic(&scrollback_file(dir, session, win_idx), body.as_bytes())
}

/// Load a window's persisted scrollback. Best-effort: a missing or unreadable
/// file yields an empty Vec (the session reconstructs without that history).
pub fn load_scrollback(dir: &Path, session: &str, win_idx: usize) -> Vec<String> {
    match fs::read_to_string(scrollback_file(dir, session, win_idx)) {
        Ok(s) if !s.is_empty() => s.split('\n').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// Remove a session's persisted scrollback subtree (on kill / GC). Best-effort.
pub fn remove_scrollback(dir: &Path, session: &str) {
    let _ = fs::remove_dir_all(dir.join("scrollback").join(crate::to_hex(session.as_bytes())));
}

impl Default for PersistState {
    fn default() -> Self {
        Self::new()
    }
}

/// Default session-state **directory** (holds `meta.json` + `scrollback/`):
/// `$STOA_STATE`, else a per-uid dir under the temp dir (dev). Production
/// points `$STOA_STATE` at stoad's persistent data volume (`/atrium-data`).
pub fn default_state_dir() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("STOA_STATE") {
        return std::path::PathBuf::from(s);
    }
    let uid = unsafe { libc::getuid() };
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(format!("{}/stoad-{uid}.state", tmp.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PersistState {
        PersistState {
            version: VERSION,
            sessions: vec![PersistSession {
                name: "work".into(),
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

    #[test]
    fn save_then_load_is_identity() {
        let dir = scratch("id");
        let s = sample();
        s.save_atomic(&dir).unwrap();
        let back = PersistState::load(&dir).unwrap();
        assert_eq!(s, back);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_empty_state() {
        let dir = scratch("missing-xyz");
        let _ = fs::remove_dir_all(&dir);
        let s = PersistState::load(&dir).unwrap();
        assert!(s.sessions.is_empty());
        assert_eq!(s.version, VERSION);
    }

    #[test]
    fn wrong_version_is_empty_state() {
        let dir = scratch("ver");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(META), br#"{"version":999,"sessions":[]}"#).unwrap();
        let s = PersistState::load(&dir).unwrap();
        assert!(s.sessions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_file_is_empty_state_not_error() {
        let dir = scratch("junk");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(META), b"not json at all }{").unwrap();
        let s = PersistState::load(&dir).unwrap();
        assert!(s.sessions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrollback_round_trips_and_caps() {
        let dir = scratch("scroll");
        let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        save_scrollback(&dir, "my/sess:1", 2, &lines).unwrap();
        let back = load_scrollback(&dir, "my/sess:1", 2);
        assert_eq!(back, lines);
        // Different session / window is independent + missing → empty.
        assert!(load_scrollback(&dir, "other", 0).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrollback_retention_drops_oldest() {
        let dir = scratch("retain");
        let lines: Vec<String> = (0..SCROLLBACK_MAX + 50).map(|i| format!("L{i}")).collect();
        save_scrollback(&dir, "s", 0, &lines).unwrap();
        let back = load_scrollback(&dir, "s", 0);
        assert_eq!(back.len(), SCROLLBACK_MAX);
        assert_eq!(back[0], "L50"); // first 50 dropped
        assert_eq!(back.last().unwrap(), &format!("L{}", SCROLLBACK_MAX + 49));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_scrollback_clears_session() {
        let dir = scratch("rm");
        save_scrollback(&dir, "doomed", 0, &["x".to_string()]).unwrap();
        assert!(!load_scrollback(&dir, "doomed", 0).is_empty());
        remove_scrollback(&dir, "doomed");
        assert!(load_scrollback(&dir, "doomed", 0).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
