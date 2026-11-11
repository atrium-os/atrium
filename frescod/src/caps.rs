//! Per-app capabilities (docs/spec/fresco-capabilities.md).
//!
//! A client is named by its peer uid. The launch registry turns the uid into
//! `(user, app_id)`, and that user's Portcullis policy says what the app was
//! granted, as choragusd does. Keyboard and pointer are not gated (they come
//! with `graphics`); the bits here are the gated `input.*` keys. Computed
//! when a client connects, kept per client id, read on the hot path.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use portcullis_peer::AppRegistry;
use portcullis_policy::Policy;

/// The gated `input.*` capabilities a client holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps(u8);

impl Caps {
    pub const GAMEPAD: Caps = Caps(1);
    pub const GAMEPAD_EXCLUSIVE: Caps = Caps(2);
    pub const POINTER_LOCK: Caps = Caps(4);
    pub const RAW_HID: Caps = Caps(8);
    pub const NONE: Caps = Caps(0);
    pub const ALL: Caps = Caps(15);

    pub fn union(self, c: Caps) -> Caps {
        Caps(self.0 | c.0)
    }

    pub fn has(self, c: Caps) -> bool {
        self.0 & c.0 == c.0
    }
    fn with(self, c: Caps, on: bool) -> Caps {
        if on { Caps(self.0 | c.0) } else { self }
    }
    /// What an embed may hold at most (§2, decision 5): only the ungated
    /// keyboard and pointer, so nothing here.
    pub fn for_embed(self) -> Caps {
        Caps::NONE
    }
}

/// Where the lookup reads from, and whether development mode is on.
#[derive(Debug, Clone)]
pub struct Source {
    pub registry: PathBuf,
    pub policy_base: PathBuf,
    pub system_policy: PathBuf,
    /// `FRESCOD_DEV_CAPS=all` (§7): unregistered peers get everything.
    pub dev_all: bool,
}

impl Source {
    /// From the environment: `FRESCOD_APP_REGISTRY`, `ATRIUM_POLICY_BASE`,
    /// `FRESCOD_DEV_CAPS`. A jailed or `_frescod` frescod refuses dev mode.
    pub fn from_env() -> Source {
        let dev_asked = std::env::var("FRESCOD_DEV_CAPS").map(|v| v == "all").unwrap_or(false);
        let dev_all = dev_asked && !production();
        if dev_asked && !dev_all {
            eprintln!("frescod: FRESCOD_DEV_CAPS=all refused: running jailed or as _frescod");
        }
        Source {
            registry: std::env::var("FRESCOD_APP_REGISTRY")
                .unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into()).into(),
            policy_base: std::env::var("ATRIUM_POLICY_BASE")
                .unwrap_or_else(|_| "/var/db/atrium".into()).into(),
            system_policy: portcullis_policy::SYSTEM_PATH.into(),
            dev_all,
        }
    }

    /// The capabilities of a client whose peer is `uid`. `embed` for an
    /// embed-only connection, whose uid is its owner's.
    pub fn caps_for(&self, uid: Option<u32>, embed: bool) -> Caps {
        let registry = AppRegistry::load(&self.registry.to_string_lossy()).unwrap_or_default();
        let c = match uid.and_then(|u| registry.resolve(u)) {
            Some((user, app_id)) => granted(&self.policy_base, &self.system_policy, user, app_id),
            None if self.dev_all => Caps::ALL,
            // Unregistered (§7): gamepad reaches no further than focus.
            None => Caps::GAMEPAD,
        };
        if embed { c.for_embed() } else { c }
    }
}

/// The app's grant: the user's policy, else the installer's (system) one.
fn granted(base: &Path, system: &Path, user: &str, app_id: &str) -> Caps {
    if user.contains('/') || user.starts_with('.') {
        return Caps::NONE;
    }
    let user_policy = Policy::load(&base.join(user).join("policy.toml")).unwrap_or_default();
    let grant = match user_policy.grants.get(app_id) {
        Some(g) => Some(g.clone()),
        None => Policy::load_system(system).ok().and_then(|p| p.grants.get(app_id).cloned()),
    };
    let Some(i) = grant.and_then(|g| g.capabilities.input) else { return Caps::NONE };
    let on = |v: Option<bool>| v == Some(true);
    Caps::NONE
        .with(Caps::GAMEPAD, on(i.gamepad))
        .with(Caps::GAMEPAD_EXCLUSIVE, on(i.gamepad_exclusive))
        .with(Caps::POINTER_LOCK, on(i.pointer_lock))
        .with(Caps::RAW_HID, on(i.raw_hid))
    // system_keys is never honoured for apps (decision 2).
}

/// Running as `_frescod`, or inside a jail.
fn production() -> bool {
    let uid = unsafe { libc::getuid() };
    let frescod = std::ffi::CString::new("_frescod").ok()
        .map(|c| unsafe { libc::getpwnam(c.as_ptr()) })
        .filter(|p| !p.is_null())
        .map(|p| unsafe { (*p).pw_uid });
    frescod == Some(uid) || jailed()
}

#[cfg(target_os = "freebsd")]
fn jailed() -> bool {
    let mut v: libc::c_int = 0;
    let mut n = std::mem::size_of::<libc::c_int>();
    let name = c"security.jail.jailed";
    // SAFETY: a read of an int sysctl into a local of that size.
    let r = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut v as *mut libc::c_int).cast(), &mut n, std::ptr::null(), 0) };
    r == 0 && v != 0
}

#[cfg(not(target_os = "freebsd"))]
fn jailed() -> bool {
    false
}

static SOURCE: std::sync::OnceLock<Source> = std::sync::OnceLock::new();
static CLIENT_CAPS: Mutex<[Caps; 256]> = Mutex::new([Caps::NONE; 256]);

/// Work out and keep client `id`'s capabilities, at connect.
pub fn admit(id: u8, uid: Option<u32>, embed: bool) -> Caps {
    let c = SOURCE.get_or_init(Source::from_env).caps_for(uid, embed);
    CLIENT_CAPS.lock().unwrap()[id as usize] = c;
    log::info!("frescod: client {id} uid {uid:?} input caps {c:?}");
    c
}

/// Client `id`'s capabilities (none once it has gone).
pub fn of(id: u8) -> Caps {
    CLIENT_CAPS.lock().unwrap()[id as usize]
}

/// Set client `id`'s capabilities directly (tests).
#[cfg(test)]
pub fn set(id: u8, c: Caps) {
    CLIENT_CAPS.lock().unwrap()[id as usize] = c;
}

pub fn forget(id: u8) {
    CLIENT_CAPS.lock().unwrap()[id as usize] = Caps::NONE;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str) -> (Source, PathBuf) {
        let d = std::env::temp_dir().join(format!("frescod-caps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("alice")).unwrap();
        std::fs::write(d.join("registry"), "50001 alice org.example.game\n50002 alice org.example.editor\n50003 bob org.example.game\n").unwrap();
        std::fs::write(d.join("alice/policy.toml"), r#"
[grants."org.example.game"]
manifest_hash = "h"
granted_at = "t"
[grants."org.example.game".capabilities]
graphics = "fresco"
input.gamepad = true
input.gamepad_exclusive = true
input.system_keys = true
[grants."org.example.editor"]
manifest_hash = "h"
granted_at = "t"
[grants."org.example.editor".capabilities]
graphics = "fresco"
"#).unwrap();
        let s = Source {
            registry: d.join("registry"),
            policy_base: d.clone(),
            system_policy: d.join("no-system-policy.toml"),
            dev_all: false,
        };
        (s, d)
    }

    #[test]
    fn grants_come_from_the_users_policy() {
        let (s, d) = setup("grants");
        let game = s.caps_for(Some(50001), false);
        assert!(game.has(Caps::GAMEPAD) && game.has(Caps::GAMEPAD_EXCLUSIVE));
        assert!(!game.has(Caps::POINTER_LOCK) && !game.has(Caps::RAW_HID));
        assert_eq!(s.caps_for(Some(50002), false), Caps::NONE, "editor declared no gamepad");
        assert_eq!(s.caps_for(Some(50003), false), Caps::NONE, "bob never granted the game");
        assert_eq!(s.caps_for(Some(50001), true), Caps::NONE, "an embed gets none of its owner's");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn unregistered_peers_get_gamepad_or_dev_mode() {
        let (mut s, d) = setup("dev");
        assert_eq!(s.caps_for(Some(0), false), Caps::GAMEPAD);
        assert_eq!(s.caps_for(None, false), Caps::GAMEPAD);
        s.dev_all = true;
        assert_eq!(s.caps_for(Some(1001), false), Caps::ALL);
        assert!(s.caps_for(Some(50002), false) == Caps::NONE, "dev mode never widens a registered app");
        std::fs::remove_dir_all(d).ok();
    }
}
