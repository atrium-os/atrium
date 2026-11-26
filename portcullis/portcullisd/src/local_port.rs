//! `OpenLocalPort` (background-apps.md §4.4): a connected socket to the
//! person's own server, for Navigator's broker.
//!
//! Who asks and for whom come only from the caller's uid: it must be an
//! app instance whose installed manifest holds `navigator-broker`, and the
//! port must be a `this-computer` listen port applied now to a running
//! instance of the same person in the same persona. jaild names the jail's
//! own address (never the host's loopback, where anything could listen),
//! and only while its rule is loaded; portcullisd connects as root, which
//! the base rules allow and nothing else on the host gets.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use portcullis_ipc::Response;

/// How long a connect to the app may take.
pub const CONNECT: Duration = Duration::from_secs(5);
/// Sockets one persona may be handed in [`WINDOW`].
pub const PER_WINDOW: usize = 32;
pub const WINDOW: Duration = Duration::from_secs(10);

/// The asks a persona made in the last [`WINDOW`]: true when one more fits.
fn admit(key: (String, String), now: Instant) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<(String, String), Vec<Instant>>>> = OnceLock::new();
    let mut m = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, v| {
        v.retain(|t| now.duration_since(*t) < WINDOW);
        !v.is_empty()
    });
    let v = m.entry(key).or_default();
    if v.len() >= PER_WINDOW {
        return false;
    }
    v.push(now);
    true
}

/// The answer and socket for `uid`'s ask for `port`, or why not.
pub fn open(uid: u32, registry: &portcullis_peer::AppRegistry, port: u16) -> Result<(Response, TcpStream), String> {
    let Some((user, app)) = registry.resolve(uid) else {
        return Err("open local port: the caller is not a launched app".into());
    };
    if registry.is_session(uid) || !crate::launch::app_is_navigator_broker(app) {
        eprintln!("portcullisd: OpenLocalPort {port} DENIED to {app} (uid {uid}): no navigator-broker");
        return Err("open local port: only the Navigator may open your own servers".into());
    }
    let persona = registry.persona(uid).flatten();
    let persona_key = persona.unwrap_or(portcullis_policy::PERSONAL);
    let nothing = || format!("Nothing of yours is serving port {port}");
    let Some((jail, app_id)) = crate::listen_run::local_target(user, persona, port) else {
        return Err(nothing());
    };
    if !admit((user.to_string(), persona_key.to_string()), Instant::now()) {
        return Err(format!("open local port: more than {PER_WINDOW} in {} s", WINDOW.as_secs()));
    }
    let addr = portcullisd::jaild_jail::listen_target(&jail, port).map_err(|why| {
        eprintln!("portcullisd: OpenLocalPort {port} for {user}: {jail}: {why}");
        nothing()
    })?;
    let to: SocketAddr = addr.parse().map_err(|_| format!("jaild named {addr:?}, not an address"))?;
    let s = TcpStream::connect_timeout(&to, CONNECT).map_err(|e| format!("{app_id} didn't answer on port {port}: {e}"))?;
    if s.peer_addr().ok() != Some(to) {
        return Err("open local port: the socket is not to the app".into());
    }
    let app_name = crate::launch::installed_texts().into_iter().find(|(_, _, m)| m.app.id == app_id)
        .map(|(_, _, m)| m.app.name).unwrap_or_else(|| app_id.clone());
    eprintln!("portcullisd: OpenLocalPort {port} for {user} ({persona_key}) -> {app_id} at {addr}");
    Ok((Response::LocalPort { app_id, app_name, addr }, s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_persona_gets_at_most_its_window() {
        let t = Instant::now();
        let k = || ("ada".to_string(), "work".to_string());
        for _ in 0..PER_WINDOW {
            assert!(admit(k(), t));
        }
        assert!(!admit(k(), t), "one past the window is refused");
        assert!(admit(("ada".into(), "personal".into()), t), "another persona has its own");
        assert!(admit(k(), t + WINDOW), "the window moves on");
    }
}
