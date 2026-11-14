//! Schema types for atrium.toml — deserialised from TOML via serde.
//!
//! Validation (separate from parsing) lives in `validate.rs` and runs
//! against an already-deserialised `Manifest`. This split keeps the
//! schema free of business rules and makes `Manifest` easy to use as
//! a typed configuration object once it's been validated.
//!
//! Forward compatibility: unknown top-level sections and unknown keys
//! within `[capabilities]` are NOT rejected at parse time. Validation
//! emits warnings for unknown capability keys (per spec §3.3).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app:          AppSection,
    /// Bundle/distribution facts: form (native|ir) + target arches + the entry's
    /// canonical home. Optional for back-compat; see atrium-bundle-format.md §4.
    /// Folded in from the former insula-manifest `[bundle]` section as part of
    /// unifying the two manifest schemas onto one `atrium.toml`.
    pub bundle:       Option<BundleSection>,
    #[serde(default)]
    pub capabilities: Capabilities,
    pub setup:        Option<SetupSection>,
    pub resources:    Option<ResourcesSection>,
    pub supervision:  Option<SupervisionSection>,
}

/// `[bundle]` — distribution facts (atrium-bundle-format.md §4). Carried in the
/// one canonical `atrium.toml` so a bundle can declare its form + target arches +
/// canonical entry, which the legacy `portcullis-toml` schema couldn't express.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSection {
    #[serde(default)]
    pub form:   BundleForm,
    #[serde(default)]
    pub arches: Vec<String>,
    /// Canonical home for the entry path. When absent the loader falls back to
    /// `[app].entry` (the legacy location) — see [`Manifest::entry`].
    pub entry:  Option<String>,
}

/// How the entry artifact is shipped: a native ELF per arch, or a portable IR
/// (WASM) that install-time AOT compiles (insula.md §3.2–3.3).
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BundleForm {
    #[default]
    Native,
    Ir,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSection {
    pub id:          String,
    pub name:        String,
    pub version:     String,
    pub entry:       String,
    /// SDK/ABI generation the app targets (folded in from insula-manifest). Optional.
    #[serde(rename = "sdk-version")]
    pub sdk_version: Option<String>,
    pub description: Option<String>,
    /// Icon the shell shows for this app — a named icon (e.g. "terminal", resolved
    /// against the system icon set) or a path to an SVG the app bundles. `None` →
    /// the shell falls back to a default.
    pub icon:        Option<String>,
}

/// Capability set. Used in two places: top-level `[capabilities]`
/// (runtime) and `[setup.capabilities]` (overrides during setup).
///
/// `extra` captures unknown keys for the validator to warn about
/// without rejecting (forward compat). All known capability keys
/// are explicit fields.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Capabilities {
    pub graphics:   Option<String>,             /* "fresco" today */
    pub clipboard:  Option<bool>,
    pub notify:     Option<bool>,
    #[serde(rename = "open-uri")]
    pub open_uri:   Option<bool>,
    pub audio:      Option<bool>,
    pub filesystem: Option<Vec<String>>,
    /// A mode string or a table of finer grants — see crate::network.
    pub network:    Option<crate::network::NetworkSpec>,
    pub fonts:      Option<FontsCap>,
    #[serde(rename = "tessera-cas-read")]
    pub tessera_cas_read: Option<bool>,         /* restricted: services only */
    #[serde(rename = "usb-hid")]
    pub usb_hid:    Option<bool>,               /* restricted */
    pub camera:     Option<bool>,
    pub microphone: Option<bool>,
    /// Tap the system audio output or another app's stream (the audio
    /// screen-record). Restricted + prominently surfaced — designed against
    /// the PulseAudio monitor-source leak (Lyra §9, enforced by Choragus).
    #[serde(rename = "audio-monitor")]
    pub audio_monitor: Option<bool>,
    /// Manage other apps' windows + route input across the whole session (the
    /// display analog of `audio-monitor`): cross-app window enumeration/control +
    /// input routing. Restricted — held only by the trusted session shell (Forum);
    /// normal apps get `graphics` (their own windows only). Enforced by the WM the
    /// way Choragus enforces the audio caps (display §12.5).
    #[serde(rename = "window-management")]
    pub window_management: Option<bool>,
    /// Drive the session's window manager over `forum-ctl` — the chrome apps
    /// (dock/bar/shelf/overview) that ask the WM core to list/focus surfaces. Held
    /// only by Forum's own chrome; ordinary apps get `graphics` (their own windows).
    /// Strictly weaker than `window-management` (the core mediates every intent),
    /// but still restricted so a random app can't drive the shell.
    #[serde(rename = "forum-control")]
    pub forum_control: Option<bool>,
    /// Ask portcullisd to list the installed apps and to launch one — the app
    /// LAUNCHER's grant (Forum's dock today). It is only ever the requester: the
    /// TCB still does the manifest verify, the policy check, the per-app uid and
    /// the jail, exactly as for a launch from the CLI. Restricted, and strictly
    /// narrower than handing an app the app tree: the daemon decides what the
    /// catalog contains, so the holder learns what it is allowed to launch and
    /// nothing else about the filesystem.
    #[serde(rename = "app-launch")]
    pub app_launch: Option<bool>,
    /// The volume UI's grant (Forum, Curia): list the session user's apps that
    /// are playing and set their volume, mute, the master level and the output,
    /// over choragusd's `audio-control/` socket. It hears nothing and reads no
    /// samples (that is `audio-monitor`). Restricted so an app can't silence or
    /// blast another (lyra-v1.md §7).
    #[serde(rename = "audio-control")]
    pub audio_control: Option<bool>,
    /// Now-playing and the media keys (Forum, the lock screen): see the
    /// active media session (app, state; title only if the user allows it on
    /// the lock screen) and send it play, pause, next, previous and seek over
    /// choragusd's `media-control/` socket. Restricted so an app can't watch
    /// what others play or steer them (atrium-player-pip-keys.md §4).
    #[serde(rename = "media-control")]
    pub media_control: Option<bool>,
    /// The USB sticks served for the session (Forum's bar): list them, hear
    /// one come and go, and eject one, over dispensatord's `removable-media/`
    /// socket (device-assignment.md §6.6). It reads no file on them: that is
    /// the mount, which the session's user owns. Restricted.
    #[serde(rename = "removable-media")]
    pub removable_media: Option<bool>,
    /// Lock the screen (Forum's bar, docs/spec/lock-screen.md): ask
    /// ostiarius, over its `ostiarius-lock/` socket, to lock the session.
    /// That socket takes `lock` and nothing else; unlocking is the lock
    /// screen's alone. Restricted.
    #[serde(rename = "session-lock")]
    pub session_lock: Option<bool>,
    /// Every device on the machine, who has it, and why one has none, over
    /// dispensatord's `device-list/` socket (device-assignment.md §6.10):
    /// Curia's Devices page. Hardware ids identify the machine, so it is
    /// restricted.
    #[serde(rename = "device-list")]
    pub device_list: Option<bool>,
    /// navigatord's page socket (`navigator-page/`, navigator-ui.md §2.1):
    /// the Navigator's doc-viewer child redeems a page's one-shot ticket
    /// there for its recording, and can do nothing else on it. Restricted
    /// to that child's bundle.
    #[serde(rename = "navigator-page")]
    pub navigator_page: Option<bool>,
    /// The file manager's standing grant (device-assignment.md §4.8): every
    /// served stick, read-write, in its own jail at `/media/<part>`, for as
    /// long as the jail runs. Implies the `removable-media/` socket (it asks
    /// for the grant there). Restricted to the file manager; every other app
    /// asks the system picker for one file or folder.
    #[serde(rename = "removable-files")]
    pub removable_files: Option<bool>,
    /// The system file picker (scrinium.md §4): serve Limen's `picker` role
    /// and ask dispensatord, over `removable-media/`, to put the folder the
    /// user picked into the jail of the app it picked for. It reads nothing
    /// by itself (that is `removable-files`). Restricted to Scrinium.
    #[serde(rename = "file-picker")]
    pub file_picker: Option<bool>,
    /// A status item in Forum's bar (forum-status-items.md): an icon and a
    /// menu the bar draws for this app, over forum-wm's `forum-status/`
    /// socket. The app never draws in the bar.
    #[serde(rename = "status-item")]
    pub status_item: Option<bool>,
    /// An output device driver (vinculum-audio): add an output to choragusd
    /// and read that output's mix over `audio-device/`. It hears no app's
    /// own samples and sets no app's level (lyra-external-device.md).
    #[serde(rename = "audio-device")]
    pub audio_device: Option<bool>,
    /// Fetch URLs through the system's jailed fetcher, Nuntius
    /// (docs/spec/nuntius.md §2): portcullisd starts a fetcher of the app's
    /// own on request and hands it the channel. No raw sockets: the address
    /// rule (no LAN, loopback or metadata endpoint), TLS and the caps are the
    /// fetcher's. An app that needs raw sockets asks for `network` instead,
    /// and the user sees the difference.
    #[serde(rename = "net-fetch")]
    pub net_fetch: Option<bool>,
    /// ★ Narrow the fetcher to these sites (nuntius.md §4): host names,
    /// exact (`example.com`) or every name below one (`*.example.com`).
    /// Absent: unrestricted. Only takes away, so it asks no consent; a list
    /// that does not parse refuses the fetcher rather than run it unlimited.
    #[serde(rename = "net-fetch-sites")]
    pub net_fetch_sites: Option<Vec<String>>,
    /// Play protected (DRM) media: portcullisd starts a content decryption
    /// module of the app's own (`OpenCdm`, docs/spec/hospes.md §3) in a jail
    /// of its own, as its own uid, with a store per site the app plays from.
    /// The app never gets the module, its library or its store. Asks consent:
    /// the module is a binary nobody here can read.
    #[serde(rename = "protected-media")]
    pub protected_media: Option<bool>,
    /// ★ The system call filter class the app's jail gets
    /// (docs/spec/jail-sysfil.md §5): a profile name from the `[sysfil]`
    /// section of jaild's policy, e.g. `renderer`. Absent: `networked` if the
    /// app has a `network` capability, `app` otherwise. `terminal` (exec,
    /// tty control) is restricted and asks for consent; the others only
    /// narrow what the app can call.
    pub syscalls: Option<String>,
    /// ★ Input from frescod (atrium-input.md §6, fresco-capabilities.md §3),
    /// written `input.gamepad = true` etc. Keyboard and pointer come with
    /// `graphics`; frescod enforces the rest per app from the user's grant.
    pub input: Option<InputCaps>,
    /// Unknown / future keys. Validator warns; doesn't reject.
    #[serde(flatten)]
    pub extra:      BTreeMap<String, toml::Value>,
}

/// `[capabilities] input.*` (fresco-capabilities.md §3). Underscored as
/// atrium-input.md §6 writes them; the kebab spelling the other keys use is
/// accepted too.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputCaps {
    /// Implied by `graphics`; written for clarity only.
    pub keyboard: Option<bool>,
    /// Implied by `graphics`; written for clarity only.
    pub pointer: Option<bool>,
    /// Game controller events while the app's window has focus. No prompt.
    pub gamepad: Option<bool>,
    /// Claim a controller so it stays with the app when focus moves. Asks.
    #[serde(alias = "gamepad-exclusive")]
    pub gamepad_exclusive: Option<bool>,
    /// Hide the cursor and take relative motion while focused. Asks.
    #[serde(alias = "pointer-lock")]
    pub pointer_lock: Option<bool>,
    /// Raw reports from a named HID device. Restricted.
    #[serde(alias = "raw-hid")]
    pub raw_hid: Option<bool>,
    /// System shortcuts (Cmd-Tab and the like). Never granted to apps: the
    /// window manager has them through `window-management`.
    #[serde(alias = "system-keys")]
    pub system_keys: Option<bool>,
}

impl InputCaps {
    fn merge(base: Option<InputCaps>, ovr: Option<InputCaps>) -> Option<InputCaps> {
        match (base, ovr) {
            (None, o) => o,
            (b, None) => b,
            (Some(b), Some(o)) => Some(InputCaps {
                keyboard:          o.keyboard.or(b.keyboard),
                pointer:           o.pointer.or(b.pointer),
                gamepad:           o.gamepad.or(b.gamepad),
                gamepad_exclusive: o.gamepad_exclusive.or(b.gamepad_exclusive),
                pointer_lock:      o.pointer_lock.or(b.pointer_lock),
                raw_hid:           o.raw_hid.or(b.raw_hid),
                system_keys:       o.system_keys.or(b.system_keys),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkCap {
    None,
    Loopback,
    Full,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FontsCap {
    pub mode:  String,        /* "read-only" today */
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSection {
    pub command:      String,
    pub timeout:      Option<String>,           /* duration like "120s" */
    pub capabilities: Option<Capabilities>,     /* overrides on top of runtime */
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesSection {
    pub memory: Option<String>,                 /* "512M", "2G" */
    pub cpu:    Option<u32>,                    /* percent of one core */
    pub files:  Option<u32>,
    /// Size of the app's overlay volume — everything it can write, since the
    /// app tree itself is mounted read-only (`portcullis.md` §4.2). Same size
    /// strings as `memory`. Absent = 1 GiB; accepted range 64M-8G, clamped
    /// with a message outside it (`portcullis_overlay::MAX_QUOTA_BYTES`
    /// records why 8G). This is a quota, not an allocation: an unused
    /// overlay costs its metadata, about 15 MiB.
    ///
    /// Unlike its siblings here, this field IS enforced — it becomes the
    /// overlay volume's whole-filesystem quota, and the jail's `df` reports
    /// it. `memory`, `cpu` and `files` are still declarations that nothing
    /// reads yet.
    pub storage: Option<String>,
    /// ★ How the memory federation budgets this app's ONE-SHOT jails
    /// (portcullis.md §6.5.2e): as one pool whose share does not grow with
    /// its membership. Read by `memfed --pools-from <apps dir>`; absent = the
    /// app's ephemeral jails are not pooled from its manifest.
    pub pool: Option<PoolSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSection {
    /// The pool's weight in the federation's water-fill (> 0).
    pub weight: f64,
    /// Protected minimum for the whole pool, a size string ("256M").
    pub floor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisionSection {
    pub restart:    Option<RestartPolicy>,
    #[serde(rename = "keep-alive")]
    pub keep_alive: Option<bool>,
    pub instances:  Option<InstancesPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    OnCrash,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstancesPolicy {
    Single,
    Multi,
}

impl Manifest {
    /// Parse + return a typed `Manifest`. No validation rules
    /// applied at this stage — call `validate()` for that.
    pub fn from_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// The entry path, resolved canonically: `[bundle].entry` if present, else the
    /// legacy `[app].entry`. Consumers should call this rather than reading either
    /// field directly (atrium-bundle-format.md §4).
    pub fn entry(&self) -> &str {
        self.bundle.as_ref()
            .and_then(|b| b.entry.as_deref())
            .unwrap_or(&self.app.entry)
    }
}

/// Merge `override_caps` over `base`, field by field. Any field set
/// in the override wins; otherwise the base value carries through.
/// List-shaped fields (filesystem, fonts.paths) replace wholesale —
/// override wins as a complete set, not as an addition. Spec §3.4
/// "two-phase capabilities" treats overrides as bidirectional, so a
/// setup phase can equally well *drop* a runtime capability.
pub fn merge_capabilities(base: &Capabilities, ovr: &Capabilities) -> Capabilities {
    Capabilities {
        graphics:         ovr.graphics.clone().or_else(|| base.graphics.clone()),
        clipboard:        ovr.clipboard.or(base.clipboard),
        notify:           ovr.notify.or(base.notify),
        open_uri:         ovr.open_uri.or(base.open_uri),
        audio:            ovr.audio.or(base.audio),
        filesystem:       ovr.filesystem.clone().or_else(|| base.filesystem.clone()),
        network:          ovr.network.clone().or(base.network.clone()),
        fonts:            ovr.fonts.clone().or_else(|| base.fonts.clone()),
        tessera_cas_read: ovr.tessera_cas_read.or(base.tessera_cas_read),
        usb_hid:          ovr.usb_hid.or(base.usb_hid),
        camera:           ovr.camera.or(base.camera),
        microphone:       ovr.microphone.or(base.microphone),
        audio_monitor:    ovr.audio_monitor.or(base.audio_monitor),
        window_management: ovr.window_management.or(base.window_management),
        forum_control:    ovr.forum_control.or(base.forum_control),
        app_launch:       ovr.app_launch.or(base.app_launch),
        audio_control:    ovr.audio_control.or(base.audio_control),
        media_control:    ovr.media_control.or(base.media_control),
        removable_media:  ovr.removable_media.or(base.removable_media),
        session_lock:     ovr.session_lock.or(base.session_lock),
        device_list:      ovr.device_list.or(base.device_list),
        navigator_page:   ovr.navigator_page.or(base.navigator_page),
        removable_files:  ovr.removable_files.or(base.removable_files),
        file_picker:      ovr.file_picker.or(base.file_picker),
        status_item:      ovr.status_item.or(base.status_item),
        audio_device:     ovr.audio_device.or(base.audio_device),
        net_fetch:        ovr.net_fetch.or(base.net_fetch),
        net_fetch_sites:  ovr.net_fetch_sites.clone().or_else(|| base.net_fetch_sites.clone()),
        protected_media:  ovr.protected_media.or(base.protected_media),
        syscalls:         ovr.syscalls.clone().or_else(|| base.syscalls.clone()),
        input:            InputCaps::merge(base.input, ovr.input),
        extra:            {
            let mut e = base.extra.clone();
            for (k, v) in &ovr.extra { e.insert(k.clone(), v.clone()); }
            e
        },
    }
}

#[cfg(test)]
mod bundle_tests {
    use super::*;

    const WITH_BUNDLE: &str = r#"
[app]
id = "org.atrium.edit"
name = "Edit"
version = "1.0.0"
entry = "bin/legacy"
sdk-version = "1.x"
[bundle]
form = "native"
arches = ["aarch64-freebsd", "aarch64-darwin"]
entry = "bin/atrium-edit"
[capabilities]
graphics = "fresco"
"#;

    #[test]
    fn bundle_section_parses_and_entry_resolves_canonically() {
        let m = Manifest::from_str(WITH_BUNDLE).expect("parse");
        let b = m.bundle.as_ref().expect("bundle");
        assert_eq!(b.form, BundleForm::Native);
        assert_eq!(b.arches, ["aarch64-freebsd", "aarch64-darwin"]);
        assert_eq!(m.app.sdk_version.as_deref(), Some("1.x"));
        // [bundle].entry wins over the legacy [app].entry.
        assert_eq!(m.entry(), "bin/atrium-edit");
    }

    #[test]
    fn entry_falls_back_to_app_entry_without_bundle() {
        // A manifest with no [bundle] (the forum apps' shape) resolves [app].entry.
        let m = Manifest::from_str(
            "[app]\nid=\"x\"\nname=\"X\"\nversion=\"1\"\nentry=\"bin/x\"\n").expect("parse");
        assert!(m.bundle.is_none());
        assert_eq!(m.entry(), "bin/x");
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn override_adds_network_keeps_runtime_clipboard() {
        let mut base = Capabilities::default();
        base.clipboard = Some(true);
        let mut ovr = Capabilities::default();
        ovr.network = Some(crate::network::NetworkSpec::Mode(NetworkCap::Full));
        let m = merge_capabilities(&base, &ovr);
        assert_eq!(m.clipboard, Some(true));
        assert_eq!(m.network.as_ref().map(|n| n.mode()), Some(NetworkCap::Full));
    }

    #[test]
    fn override_can_drop_network() {
        /* Bidirectional: setup phase deliberately strips runtime's
         * loopback in favor of "no network at all" during setup. */
        let mut base = Capabilities::default();
        base.network = Some(crate::network::NetworkSpec::Mode(NetworkCap::Loopback));
        let mut ovr = Capabilities::default();
        ovr.network = Some(crate::network::NetworkSpec::Mode(NetworkCap::None));
        let m = merge_capabilities(&base, &ovr);
        assert_eq!(m.network.as_ref().map(|n| n.mode()), Some(NetworkCap::None));
    }

    #[test]
    fn override_filesystem_replaces_wholesale() {
        let mut base = Capabilities::default();
        base.filesystem = Some(vec!["~/Documents".into()]);
        let mut ovr = Capabilities::default();
        ovr.filesystem = Some(vec!["~/Downloads".into()]);
        let m = merge_capabilities(&base, &ovr);
        assert_eq!(m.filesystem, Some(vec!["~/Downloads".to_string()]));
    }
}

#[cfg(test)]
mod pool_tests {
    const POOLED: &str = r#"
[app]
id = "org.atrium.navigator.converter"
name = "Navigator converter"
version = "1"
entry = "bin/prerender"
[resources]
memory = "4G"
cpu = 100
[resources.pool]
weight = 1.5
floor = "256M"
"#;

    #[test]
    fn a_pool_parses_and_validates() {
        let m = crate::Manifest::from_str(POOLED).unwrap();
        let r = m.resources.as_ref().unwrap();
        let p = r.pool.as_ref().unwrap();
        assert_eq!((p.weight, p.floor.as_deref(), r.cpu), (1.5, Some("256M"), Some(100)));
        assert!(crate::validate(&m).errors.is_empty(), "{:?}", crate::validate(&m).errors);
    }

    #[test]
    fn a_bad_pool_is_a_validation_error() {
        for (bad, what) in [("weight = 0.0", "weight"), ("weight = -1.0", "weight"),
                            ("weight = 1.0\nfloor = \"lots\"", "floor")] {
            let t = POOLED.replace("weight = 1.5\nfloor = \"256M\"", bad);
            let m = crate::Manifest::from_str(&t).unwrap();
            let errs = crate::validate(&m).errors;
            assert!(errs.iter().any(|e| e.contains("[resources.pool]") && e.contains(what)), "{bad}: {errs:?}");
        }
        // An unknown key is a parse error (deny_unknown_fields), not ignored.
        assert!(crate::Manifest::from_str(&POOLED.replace("floor = \"256M\"", "flor = \"256M\"")).is_err());
    }
    const GAME: &str = r#"
[app]
id = "org.example.game"
name = "Game"
version = "1"
entry = "bin/game"
[capabilities]
graphics = "fresco"
input.gamepad = true
input.gamepad-exclusive = true
input.pointer_lock = true
"#;

    #[test]
    fn input_capabilities_parse_and_merge() {
        use super::super::{merge_capabilities, Capabilities, InputCaps};
        let m = crate::Manifest::from_str(GAME).unwrap();
        let i = m.capabilities.input.unwrap();
        assert_eq!((i.gamepad, i.gamepad_exclusive, i.pointer_lock, i.raw_hid), (Some(true), Some(true), Some(true), None));
        assert!(m.capabilities.extra.is_empty(), "input is not an unknown key: {:?}", m.capabilities.extra);
        let r = crate::validate(&m);
        assert!(r.errors.is_empty() && r.warnings.iter().all(|w| !w.contains("input")), "{r:?}");
        // A setup override drops one key and keeps the rest.
        let ovr = Capabilities { input: Some(InputCaps { pointer_lock: Some(false), ..Default::default() }), ..Default::default() };
        let mi = merge_capabilities(&m.capabilities, &ovr).input.unwrap();
        assert_eq!((mi.gamepad, mi.pointer_lock), (Some(true), Some(false)));
        // A misspelt input key is an error, not silently ignored.
        assert!(crate::Manifest::from_str(&GAME.replace("input.gamepad = true", "input.gamepd = true")).is_err());
        // Restricted and never-for-apps keys warn.
        let m = crate::Manifest::from_str(&GAME.replace("input.gamepad = true", "input.raw_hid = true\ninput.system_keys = true")).unwrap();
        let w = crate::validate(&m).warnings;
        assert!(w.iter().any(|w| w.contains("raw_hid")) && w.iter().any(|w| w.contains("system_keys")), "{w:?}");
    }
}
