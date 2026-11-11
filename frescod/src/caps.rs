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

impl Source {
    /// What a change to any grant that can matter shows up in: the
    /// registry, the system policy, and the policy of each user with a
    /// client (§6). Policies are replaced by rename, so the size and
    /// modification time of the path are compared, not a watch on a file.
    fn stamp(&self, uids: &[Option<u32>]) -> Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)> {
        let st = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()));
        let registry = AppRegistry::load(&self.registry.to_string_lossy()).unwrap_or_default();
        let mut paths = vec![self.registry.clone(), self.system_policy.clone()];
        for user in uids.iter().flatten().filter_map(|&u| registry.resolve(u)).map(|(user, _)| user) {
            if !user.contains('/') && !user.starts_with('.') {
                let p = self.policy_base.join(user).join("policy.toml");
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        paths.into_iter().map(|p| { let s = st(&p); (p, s) }).collect()
    }
}

/// Once a second, look for changed grants; on a change, work out every
/// live client's capabilities again and call `changed` for each one whose
/// capabilities changed (fresco-capabilities.md §6). `live` lists the
/// clients as (id, peer uid, embed-only).
pub fn watch(live: fn() -> Vec<(u8, Option<u32>, bool)>, changed: fn(u8)) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("frescod-caps".into())
            .spawn(move || {
                let src = SOURCE.get_or_init(Source::from_env);
                let mut last = None;
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let clients = live();
                    let uids: Vec<_> = clients.iter().map(|c| c.1).collect();
                    let stamp = src.stamp(&uids);
                    if last.as_ref() == Some(&stamp) {
                        continue;
                    }
                    last = Some(stamp);
                    for id in recompute(src, &clients) {
                        changed(id);
                    }
                }
            })
            .ok();
    });
}

/// Work the clients' capabilities out again; the ids whose changed.
fn recompute(src: &Source, clients: &[(u8, Option<u32>, bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(id, uid, embed) in clients {
        let new = src.caps_for(uid, embed);
        let old = std::mem::replace(&mut CLIENT_CAPS.lock().unwrap()[id as usize], new);
        if old != new {
            log::info!("frescod: client {id} uid {uid:?} input caps {old:?} -> {new:?}");
            out.push(id);
        }
    }
    out
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

    /// §6: a changed policy shows in the stamp, and recomputing takes the
    /// grant away from a live client.
    #[test]
    fn a_changed_policy_is_seen_and_revokes() {
        let (s, d) = setup("revoke");
        // Ids 200-201 are not used by the other tests.
        set(200, s.caps_for(Some(50001), false));
        set(201, s.caps_for(Some(50002), false));
        let clients = [(200, Some(50001), false), (201, Some(50002), false)];
        let before = s.stamp(&[Some(50001), Some(50002)]);
        assert_eq!(before.len(), 3, "registry, system policy, alice's policy: {before:?}");
        assert!(recompute(&s, &clients).is_empty());
        let p = d.join("alice/policy.toml");
        let text = std::fs::read_to_string(&p).unwrap().replace("input.gamepad_exclusive = true\n", "");
        std::fs::write(&p, text).unwrap();
        assert_ne!(s.stamp(&[Some(50001), Some(50002)]), before);
        assert_eq!(recompute(&s, &clients), vec![200]);
        assert!(of(200).has(Caps::GAMEPAD) && !of(200).has(Caps::GAMEPAD_EXCLUSIVE));
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
