//! limend, the embed broker (docs/spec/limen.md).
//!
//! Environment:
//! - `LIMEN_SOCKET` (default /var/run/atrium/limend.sock)
//! - `LIMEN_APP_REGISTRY` (default the launch registry)
//! - `LIMEN_DEFAULTS`: `role=app-id,...`, the provider of each user role
//!   until Curia holds the user's choice (limen.md §3.2).
//!
//! - `LIMEN_ARCA_UID`: the uid of arca-ui, the `credential` provider
//!   (default 65021, `_arca-ui`); only it may provide that role.
//! - `LIMEN_USER`: limend's user name as worker rules name it (default `_limen`).
//!
//! User-role children start through portcullisd's `ExecInstance`
//! ([`limen::launch`]). Slots are bound with frescod's slot op on its
//! activation socket (`LIMEN_FRESCOD_ACTIVATION`, default
//! /var/run/atrium/fresco-activation.sock; [`limen::fresco`]).

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;

use limen::daemon::Shared;

fn main() -> std::io::Result<()> {
    let path = std::env::var("LIMEN_SOCKET").unwrap_or_else(|_| limen::client::DEFAULT_SOCKET.into());
    let registry = std::env::var("LIMEN_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
    let defaults: Vec<(String, String)> = std::env::var("LIMEN_DEFAULTS").unwrap_or_default()
        .split(',')
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    let resolve = Box::new(move |uid: u32| {
        let reg = portcullis_peer::AppRegistry::load(&registry).ok()?;
        reg.resolve(uid).map(|(u, a)| (u.to_string(), a.to_string()))
    });
    let defaults = Box::new(move |_user: &str, role: &str| {
        defaults.iter().find(|(r, _)| r == role).map(|(_, a)| a.clone())
    });
    let arca_uid: u32 = std::env::var("LIMEN_ARCA_UID").ok().and_then(|v| v.parse().ok()).unwrap_or(65021);
    let shared = Shared::new(Box::new(limen::fresco::Frescod::new(std::env::var("LIMEN_FRESCOD_ACTIVATION").unwrap_or_else(|_| limen::fresco::DEFAULT_SOCKET.into()))), Box::new(limen::launch::Portcullis::new(std::env::var("LIMEN_USER").unwrap_or_else(|_| "_limen".into()))), resolve, defaults);
    // arca-ui, the credential provider, is a system service (`_arca-ui`),
    // not a registry app.
    shared.set_system_uid(limen::catalogue::ARCA, arca_uid);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!("limend: listening on {path}");
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let Ok((uid, _)) = portcullis_peer::uid_gid(s.as_raw_fd()) else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Ok(c) = aqueduct::Connection::wrap(s) {
                let _ = limen::daemon::serve(c, uid, shared);
            }
        });
    }
    Ok(())
}
