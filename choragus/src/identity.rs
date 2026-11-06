//! Who is connecting, and what audio it may do (lyra-v1.md §4).
//!
//! Two independent checks must both allow a capability:
//!
//! - **The socket it reached.** choragusd listens on one socket per audio
//!   capability (`audio/`, `audio-capture/`, `audio-monitor/` under
//!   `/atrium/sockets`), and Portcullis mounts each directory only into jails
//!   that hold that capability. So the socket caps what a connection may ask for.
//! - **The user's grant.** The peer uid resolves through the Portcullis launch
//!   registry to `(user, app id)`, and that user's approved policy must grant the
//!   capability. This covers jails that see the whole socket tree.
//!
//! The app's own hello is never identity in strict mode (the service): an uid
//! not in the registry gets nothing.

use crate::app::{CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
use portcullis_peer::AppRegistry;

/// choragusd's sockets: `<root>/<name>/<name>.sock`, and what each allows. The
/// names match the directories Portcullis mounts for each capability
/// (portcullis-jail `apply_audio`, `apply_microphone`, `apply_audio_monitor`).
pub const SOCKETS: [(&str, u8); 3] = [
    ("audio", CAP_AUDIO),
    ("audio-capture", CAP_AUDIO | CAP_MICROPHONE),
    ("audio-monitor", CAP_AUDIO | CAP_MONITOR),
];

/// A connection's verified identity and the capability bits it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub app_id: String,
    pub user: String,
    pub caps: u8,
}

/// Resolve a connection.
///
/// - `uid`: the peer uid from the kernel.
/// - `claimed`: the app id in its hello (only used when `strict` is off and
///   there is no registry: the host development mode).
/// - `registry`: the launch registry, if one is loaded.
/// - `strict`: refuse anything the registry can't name (the service).
/// - `socket_caps`: what the socket it reached allows.
/// - `grant(user, app_id)`: the user's approved capability bits for the app.
///
/// `None` means refuse the connection.
pub fn resolve(
    uid: u32,
    claimed: &str,
    registry: Option<&AppRegistry>,
    strict: bool,
    socket_caps: u8,
    grant: impl Fn(&str, &str) -> u8,
) -> Option<Verified> {
    let (app_id, user) = match registry.and_then(|r| r.resolve(uid)) {
        Some((user, app)) => (app.to_string(), user.to_string()),
        None if strict || registry.is_some() => return None,
        None => {
            // Development only: no registry, trust the hello, scoped to the uid's user.
            let user = portcullis_peer::username(uid).unwrap_or_else(|| format!("uid{uid}"));
            (claimed.to_string(), user)
        }
    };
    let caps = grant(&user, &app_id) & socket_caps;
    Some(Verified { app_id, user, caps })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> AppRegistry {
        AppRegistry::parse("50001 alice org.atrium.player\n50002 alice org.atrium.recorder\n")
    }
    fn grant(_user: &str, app: &str) -> u8 {
        match app {
            "org.atrium.player" => CAP_AUDIO,
            "org.atrium.recorder" => CAP_AUDIO | CAP_MICROPHONE | CAP_MONITOR,
            _ => 0,
        }
    }
    const ALL: u8 = CAP_AUDIO | CAP_MICROPHONE | CAP_MONITOR;

    #[test]
    fn the_registry_names_the_app_not_its_hello() {
        let r = reg();
        let v = resolve(50001, "org.atrium.recorder", Some(&r), true, ALL, grant).unwrap();
        assert_eq!((v.app_id.as_str(), v.user.as_str(), v.caps), ("org.atrium.player", "alice", CAP_AUDIO));
    }

    #[test]
    fn strict_mode_refuses_an_unregistered_uid() {
        let r = reg();
        assert_eq!(resolve(4242, "org.atrium.recorder", Some(&r), true, ALL, grant), None);
        assert_eq!(resolve(4242, "org.atrium.recorder", None, true, ALL, grant), None, "no registry at all");
        assert_eq!(resolve(4242, "org.atrium.recorder", Some(&r), false, ALL, grant), None,
            "a loaded registry that can't name the uid refuses even off strict");
    }

    #[test]
    fn the_socket_caps_what_the_grant_allows() {
        let r = reg();
        // The recorder holds everything, but reached only the playback socket.
        let v = resolve(50002, "", Some(&r), true, CAP_AUDIO, grant).unwrap();
        assert_eq!(v.caps, CAP_AUDIO);
        // The player reached the monitor socket (a jail that sees the whole tree),
        // but its user never granted it the monitor.
        let v = resolve(50001, "", Some(&r), true, CAP_MONITOR, grant).unwrap();
        assert_eq!(v.caps, 0);
    }

    #[test]
    fn development_mode_trusts_the_hello_only_without_a_registry() {
        let v = resolve(unsafe { libc::getuid() }, "org.atrium.player", None, false, ALL, grant).unwrap();
        assert_eq!((v.app_id.as_str(), v.caps), ("org.atrium.player", CAP_AUDIO));
    }
}
