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

    /// Atomically write to `path`: serialize to a sibling temp file, fsync,
    /// then `rename` over `path`. A crash mid-write leaves the prior snapshot
    /// (or nothing) — never a torn file.
    pub fn save_atomic(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Temp name beside the target (same fs → rename is atomic). The pid
        // keeps concurrent writers from colliding on the temp path.
        let pid = std::process::id();
        let tmp = path.with_extension(format!("tmp.{pid}"));
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// Load from `path`. Returns an empty state (not an error) when the file
    /// is absent or carries a different [`VERSION`] — a fresh or upgraded
    /// `stoad` simply starts clean.
    pub fn load(path: &Path) -> io::Result<PersistState> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PersistState::new()),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<PersistState>(&bytes) {
            Ok(s) if s.version == VERSION => Ok(s),
            // Wrong version or unparseable → start clean rather than crash.
            _ => Ok(PersistState::new()),
        }
    }
}

impl Default for PersistState {
    fn default() -> Self {
        Self::new()
    }
}

/// Default session-state file: `$STOA_STATE`, else a per-uid path under the
/// temp dir (dev). Production points `$STOA_STATE` at the jail's persistent
/// store (e.g. `/var/db/atrium/stoa/<user>/state.json`).
pub fn default_state_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("STOA_STATE") {
        return std::path::PathBuf::from(s);
    }
    let uid = unsafe { libc::getuid() };
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(format!("{}/stoad-{uid}.state.json", tmp.trim_end_matches('/')))
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

    #[test]
    fn save_then_load_is_identity() {
        let dir = std::env::temp_dir().join(format!("stoa-persist-test-{}", std::process::id()));
        let path = dir.join("state.json");
        let s = sample();
        s.save_atomic(&path).unwrap();
        let back = PersistState::load(&path).unwrap();
        assert_eq!(s, back);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty_state() {
        let path = std::env::temp_dir().join("stoa-persist-does-not-exist-xyz.json");
        let _ = fs::remove_file(&path);
        let s = PersistState::load(&path).unwrap();
        assert!(s.sessions.is_empty());
        assert_eq!(s.version, VERSION);
    }

    #[test]
    fn wrong_version_is_empty_state() {
        let dir = std::env::temp_dir().join(format!("stoa-persist-ver-{}", std::process::id()));
        let path = dir.join("state.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, br#"{"version":999,"sessions":[]}"#).unwrap();
        let s = PersistState::load(&path).unwrap();
        assert!(s.sessions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_file_is_empty_state_not_error() {
        let dir = std::env::temp_dir().join(format!("stoa-persist-junk-{}", std::process::id()));
        let path = dir.join("state.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, b"not json at all }{").unwrap();
        let s = PersistState::load(&path).unwrap();
        assert!(s.sessions.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
