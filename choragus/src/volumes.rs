//! The user's volumes (lyra-v1.md §7): one level and mute per `(user, app id)`,
//! and the user's master level and mute, kept across restarts.
//!
//! One file per user, `<dir>/<user>.toml`, written whole through a temporary
//! file and a rename, so a crash leaves the old file or the new one:
//!
//! ```toml
//! master = 0.8
//! master_muted = false
//! [apps."org.atrium.player"]
//! volume = 0.5
//! muted = false
//! ```
//!
//! Levels are linear, 0 to 1: the user can lower an app, never amplify it.
//! Only the control socket ([`crate::control`]) changes them; an app's own
//! `SetVolume` multiplies under them and is not stored.

use lyra_protocol::wire::VolumeMode;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub volume: f32,
    pub muted: bool,
    /// The app's streams go through the output's virtual surround (the
    /// user's opt-out is `false`; lyra-spatial.md §3.1).
    #[serde(default = "yes")]
    pub spatial: bool,
}

fn yes() -> bool {
    true
}

impl Default for Level {
    fn default() -> Self {
        Level { volume: 1.0, muted: false, spatial: true }
    }
}

impl Level {
    /// The linear gain this level contributes (0 when muted).
    pub fn gain(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }
}

/// One user's volumes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserVolumes {
    #[serde(default = "one")]
    pub master: f32,
    #[serde(default)]
    pub master_muted: bool,
    #[serde(default)]
    pub apps: BTreeMap<String, Level>,
    /// Titles and artists on the lock screen (atrium-player-pip-keys.md §4.5).
    #[serde(default)]
    pub lock_titles: bool,
}

fn one() -> f32 {
    1.0
}

impl Default for UserVolumes {
    fn default() -> Self {
        UserVolumes { master: 1.0, master_muted: false, apps: BTreeMap::new(), lock_titles: false }
    }
}

impl UserVolumes {
    pub fn app(&self, app_id: &str) -> Level {
        self.apps.get(app_id).copied().unwrap_or_default()
    }

    pub fn master_gain(&self) -> f32 {
        if self.master_muted { 0.0 } else { self.master }
    }

    /// Clamp everything read from disk into range (a hand-edited or damaged
    /// file must not amplify or poison the mix).
    fn sanitize(&mut self) {
        self.master = clamp(self.master);
        for l in self.apps.values_mut() {
            l.volume = clamp(l.volume);
        }
    }
}

/// A level from the wire or a file: 0 to 1, and not a number counts as 0.
pub fn clamp(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

/// Apps a user's file may hold; past this the least recently set entries are
/// not kept. (App ids come from the registry, but the file stays bounded.)
pub const MAX_APPS: usize = 512;

/// A user name safe to use as a file name.
fn safe_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 64
        && !user.starts_with('.')
        && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Every user's volumes, loaded on first use and saved on each change.
pub struct VolumeStore {
    /// `None`: keep them in memory only (development, tests without a dir).
    dir: Option<String>,
    users: HashMap<String, UserVolumes>,
}

impl VolumeStore {
    pub fn new(dir: Option<String>) -> VolumeStore {
        VolumeStore { dir, users: HashMap::new() }
    }

    fn path(&self, user: &str) -> Option<String> {
        let dir = self.dir.as_ref()?;
        safe_user(user).then(|| format!("{dir}/{user}.toml"))
    }

    /// The user's volumes (defaults if there is no file or it can't be read).
    pub fn user(&mut self, user: &str) -> &UserVolumes {
        if !self.users.contains_key(user) {
            let mut v = self
                .path(user)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| toml::from_str::<UserVolumes>(&t).ok())
                .unwrap_or_default();
            v.sanitize();
            self.users.insert(user.to_string(), v);
        }
        &self.users[user]
    }

    /// Change the user's volumes with `f`, then save. The change stands in
    /// memory even if saving fails (the error is returned for the log).
    pub fn update(&mut self, user: &str, f: impl FnOnce(&mut UserVolumes)) -> io::Result<()> {
        self.user(user);
        let v = self.users.get_mut(user).unwrap();
        f(v);
        v.sanitize();
        while v.apps.len() > MAX_APPS {
            let k = v.apps.keys().next().cloned().unwrap();
            v.apps.remove(&k);
        }
        let Some(path) = self.path(user) else { return Ok(()) };
        let text = toml::to_string(&self.users[user]).map_err(io::Error::other)?;
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }
}

/// The outputs' own settings, system-wide (not any user's): each output's
/// left/right balance, by output name, in `<dir>/.outputs.toml` (a name no
/// user file can have: user names never start with a dot).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct OutputSettings {
    #[serde(default)]
    pub balance: BTreeMap<String, f32>,
    /// Speaker layout by output name: 2, 6 or 8 channels (default 2).
    #[serde(default)]
    pub layout: BTreeMap<String, u8>,
    /// Headphone virtual surround by output name (default off).
    #[serde(default)]
    pub spatial: BTreeMap<String, bool>,
    /// Where the volume is applied, by output name (or `card:<name>` for a
    /// built-in card), when the user chose (default automatic).
    #[serde(default)]
    pub volume_mode: BTreeMap<String, VolumeMode>,
}

/// Speaker layouts an output may have: stereo, 5.1, 7.1.
pub const LAYOUTS: [u8; 3] = [2, 6, 8];

/// A balance from the wire or a file: −1 to 1, not a number is centred.
pub fn clamp_balance(b: f32) -> f32 {
    if b.is_finite() { b.clamp(-1.0, 1.0) } else { 0.0 }
}

impl OutputSettings {
    fn path(dir: &Option<String>) -> Option<String> {
        dir.as_ref().map(|d| format!("{d}/.outputs.toml"))
    }

    pub fn load(dir: &Option<String>) -> OutputSettings {
        let mut o: OutputSettings = Self::path(dir)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        o.balance.retain(|_, b| b.is_finite());
        o.balance.values_mut().for_each(|b| *b = clamp_balance(*b));
        o.layout.retain(|_, c| LAYOUTS.contains(c));
        o
    }

    pub fn layout(&self, output: &str) -> u8 {
        self.layout.get(output).copied().unwrap_or(2)
    }

    pub fn set_layout(&mut self, dir: &Option<String>, output: &str, channels: u8) -> io::Result<()> {
        if LAYOUTS.contains(&channels) {
            self.layout.insert(output.to_string(), channels);
        }
        self.save(dir)
    }

    pub fn spatial(&self, output: &str) -> bool {
        self.spatial.get(output).copied().unwrap_or(false)
    }

    pub fn set_spatial(&mut self, dir: &Option<String>, output: &str, on: bool) -> io::Result<()> {
        self.spatial.insert(output.to_string(), on);
        self.save(dir)
    }

    pub fn volume_mode(&self, output: &str) -> Option<VolumeMode> {
        self.volume_mode.get(output).copied()
    }

    pub fn set_volume_mode(&mut self, dir: &Option<String>, output: &str, mode: Option<VolumeMode>) -> io::Result<()> {
        match mode {
            Some(m) => { self.volume_mode.insert(output.to_string(), m); }
            None => { self.volume_mode.remove(output); }
        }
        self.save(dir)
    }

    pub fn balance(&self, output: &str) -> f32 {
        self.balance.get(output).copied().unwrap_or(0.0)
    }

    pub fn set_balance(&mut self, dir: &Option<String>, output: &str, b: f32) -> io::Result<()> {
        self.balance.insert(output.to_string(), clamp_balance(b));
        self.save(dir)
    }

    fn save(&self, dir: &Option<String>) -> io::Result<()> {
        let Some(path) = Self::path(dir) else { return Ok(()) };
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("choragus-vol-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into()
    }

    #[test]
    fn levels_survive_a_restart() {
        let d = dir("persist");
        let mut s = VolumeStore::new(Some(d.clone()));
        s.update("alice", |v| {
            v.apps.insert("org.atrium.player".into(), Level { volume: 0.25, muted: false, spatial: false });
            v.master = 0.5;
        }).unwrap();
        let mut s = VolumeStore::new(Some(d));
        let v = s.user("alice");
        assert_eq!(v.app("org.atrium.player").volume, 0.25);
        assert_eq!(v.master, 0.5);
        assert_eq!(v.app("org.other"), Level::default(), "unknown apps play at the user's full level");
    }

    #[test]
    fn a_damaged_file_cannot_amplify() {
        let d = dir("damaged");
        std::fs::write(format!("{d}/bob.toml"), "master = 40.0\n[apps.\"x\"]\nvolume = 9.0\nmuted = false\n").unwrap();
        let mut s = VolumeStore::new(Some(d.clone()));
        assert_eq!(s.user("bob").master, 1.0);
        assert_eq!(s.user("bob").app("x").volume, 1.0);
        std::fs::write(format!("{d}/carol.toml"), "not toml [[[").unwrap();
        assert_eq!(s.user("carol"), &UserVolumes::default());
    }

    #[test]
    fn an_outputs_volume_mode_is_kept_and_auto_forgets_it() {
        let d = Some(dir("vmode"));
        let mut o = OutputSettings::load(&d);
        assert_eq!(o.volume_mode("card:Auris EarMen Sparrow"), None);
        o.set_volume_mode(&d, "card:Auris EarMen Sparrow", Some(VolumeMode::Fixed)).unwrap();
        assert_eq!(OutputSettings::load(&d).volume_mode("card:Auris EarMen Sparrow"), Some(VolumeMode::Fixed));
        assert_eq!(OutputSettings::load(&d).volume_mode("Speakers"), None, "per card, not every output");
        o.set_volume_mode(&d, "card:Auris EarMen Sparrow", None).unwrap();
        assert_eq!(OutputSettings::load(&d).volume_mode("card:Auris EarMen Sparrow"), None);
    }

    #[test]
    fn a_user_name_never_escapes_the_directory() {
        let d = dir("escape");
        let mut s = VolumeStore::new(Some(d.clone()));
        s.update("../etc", |v| v.master = 0.1).unwrap();
        s.update(".hidden", |v| v.master = 0.1).unwrap();
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        // In memory it still applies for this run.
        assert_eq!(s.user("../etc").master, 0.1);
    }

    #[test]
    fn balance_is_kept_and_clamped() {
        let d = Some(dir("balance"));
        let mut o = OutputSettings::load(&d);
        assert_eq!(o.balance("Speakers"), 0.0);
        o.set_balance(&d, "Speakers", 5.0).unwrap();
        assert_eq!(OutputSettings::load(&d).balance("Speakers"), 1.0);
        o.set_balance(&d, "Speakers", f32::NAN).unwrap();
        assert_eq!(OutputSettings::load(&d).balance("Speakers"), 0.0);
    }

    #[test]
    fn not_a_number_is_silence() {
        assert_eq!(clamp(f32::NAN), 0.0);
        assert_eq!(clamp(f32::INFINITY), 0.0);
        assert_eq!(clamp(-1.0), 0.0);
        assert_eq!(clamp(2.0), 1.0);
    }
}
