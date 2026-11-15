//! Per-user space config — the persistent preference layer (`forum.md`
//! §2.4). A user who never writes one gets the default flow (4 unnamed
//! spaces, every window opening on the active one).
//!
//! Location: `$FORUM_CONFIG` if set, else `/var/db/atrium/<user>/forum.toml`
//! (the same per-user convention as portcullis `policy.toml`). A missing file
//! is normal (defaults); a malformed file logs a warning and falls back to
//! defaults — config never takes the WM down.
//!
//! ```toml
//! spaces = 6
//! names = ["main", "web", "comms", "media", "build", "scratch"]
//!
//! # Per-app landing rules: app-id → space index (0-based). These apply
//! # once frescod reports real app-ids in WM_ENUMERATE (today owner_app is a
//! # per-connection id, so rules stay inert and a new window opens on the
//! # active space — the default). Forward-compatible: the rule plumbing is
//! # live, only the key isn't populated yet.
//! [assign]
//! "org.atrium.navigator" = 1
//! "org.atrium.term" = 0
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Hard ceiling on space count — a sanity clamp, not a design limit.
const MAX_SPACES: usize = 32;
/// Default space count when unconfigured. Mirrors
/// `forum_wm::daemon::DEFAULT_SPACES`.
const DEFAULT_SPACES: usize = 4;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForumConfig {
    /// Number of spaces (virtual desktops).
    /// `workspaces` is the name it had before desktops became spaces.
    #[serde(default = "default_spaces", alias = "workspaces")]
    pub spaces: usize,
    /// Optional space names, indexed `0..spaces` (cosmetic — used in
    /// logs / the overview). Extra names past `spaces` are ignored.
    #[serde(default)]
    pub names: Vec<String>,
    /// Per-app space assignment: app-id → space index (0-based).
    #[serde(default)]
    pub assign: HashMap<String, usize>,
}

fn default_spaces() -> usize {
    DEFAULT_SPACES
}

impl Default for ForumConfig {
    fn default() -> Self {
        Self { spaces: DEFAULT_SPACES, names: Vec::new(), assign: HashMap::new() }
    }
}

impl ForumConfig {
    /// Load the per-user config, or defaults if absent/invalid (never fails).
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<ForumConfig>(&text) {
                Ok(cfg) => {
                    let cfg = cfg.sanitized();
                    eprintln!(
                        "forum-wm: loaded config {path} — {} space(s), {} app rule(s)",
                        cfg.spaces,
                        cfg.assign.len(),
                    );
                    cfg
                }
                Err(e) => {
                    eprintln!("forum-wm: config {path}: parse error: {e} — using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(), // no config → the unconfigured default flow
        }
    }

    /// Clamp the space count into `[1, MAX_SPACES]` and drop assignment
    /// rules pointing past the space range.
    fn sanitized(mut self) -> Self {
        self.spaces = self.spaces.clamp(1, MAX_SPACES);
        self.assign.retain(|_, ws| *ws < self.spaces);
        self
    }

    /// The name of space `i`, if configured.
    pub fn name(&self, i: usize) -> Option<&str> {
        self.names.get(i).map(|s| s.as_str())
    }
}

/// `$FORUM_CONFIG` → `/var/db/atrium/<user>/forum.toml`.
fn config_path() -> String {
    if let Ok(p) = std::env::var("FORUM_CONFIG") {
        return p;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
    format!("/var/db/atrium/{user}/forum.toml")
}

// ── Runtime state: learned per-app placements (persisted manual moves) ─────────
//
// Distinct from the hand-authored config: when the human *moves* a window to a
// space, the WM remembers that app's placement here so it survives relaunch
// and reboot. Config `[assign]` seeds the placement; the state file overlays it
// (a move wins over a config default). Delete the state file to forget moves.

/// `$FORUM_STATE` → `/var/db/atrium/<user>/forum-spaces.state`.
fn state_path() -> String {
    if let Ok(p) = std::env::var("FORUM_STATE") {
        return p;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
    format!("/var/db/atrium/{user}/forum-spaces.state")
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    assign: HashMap<String, usize>,
}

/// Load the learned app-id → space placements (empty if no state yet).
pub fn load_state() -> HashMap<String, usize> {
    let path = state_path();
    std::fs::read_to_string(&path)
        // The file's name before desktops became spaces.
        .or_else(|_| std::fs::read_to_string(path.replace("forum-spaces.state", "forum-workspaces.state")))
        .ok()
        .and_then(|t| toml::from_str::<State>(&t).ok())
        .map(|s| s.assign)
        .unwrap_or_default()
}

/// Persist the effective app-id → space map (best-effort; logs on failure).
pub fn save_state(assign: &HashMap<String, usize>) {
    let path = state_path();
    let text = match toml::to_string(&State { assign: assign.clone() }) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("forum-wm: serialize state: {e}");
            return;
        }
    };
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!("forum-wm: save state {path}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spaces_names_and_assign() {
        let cfg: ForumConfig = toml::from_str(
            r#"
            spaces = 3
            names = ["main", "web", "comms"]
            [assign]
            "org.atrium.navigator" = 1
            "#,
        )
        .unwrap();
        let cfg = cfg.sanitized();
        assert_eq!(cfg.spaces, 3);
        assert_eq!(cfg.name(1), Some("web"));
        assert_eq!(cfg.assign.get("org.atrium.navigator"), Some(&1));
    }

    #[test]
    fn state_serializes_round_trip() {
        let mut m = HashMap::new();
        m.insert("org.atrium.term".to_string(), 2usize);
        let text = toml::to_string(&State { assign: m.clone() }).unwrap();
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back.assign, m);
    }

    #[test]
    fn empty_config_is_defaults() {
        let cfg: ForumConfig = toml::from_str("").unwrap();
        let cfg = cfg.sanitized();
        assert_eq!(cfg.spaces, DEFAULT_SPACES);
        assert!(cfg.names.is_empty() && cfg.assign.is_empty());
    }

    #[test]
    fn out_of_range_count_and_rules_are_clamped() {
        let cfg = ForumConfig { spaces: 999, names: vec![], assign: HashMap::new() }.sanitized();
        assert_eq!(cfg.spaces, MAX_SPACES);
        // a rule pointing past the (clamped) range is dropped.
        let cfg = ForumConfig {
            spaces: 2,
            names: vec![],
            assign: HashMap::from([("a".into(), 5usize), ("b".into(), 1usize)]),
        }
        .sanitized();
        assert_eq!(cfg.assign.get("a"), None, "rule past range dropped");
        assert_eq!(cfg.assign.get("b"), Some(&1));
    }
}
