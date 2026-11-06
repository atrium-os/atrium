//! limend, the embed broker (docs/spec/limen.md).
//!
//! Environment:
//! - `LIMEN_SOCKET` (default /var/run/atrium/limend.sock)
//! - `LIMEN_APP_REGISTRY` (default the launch registry)
//! - `LIMEN_DEFAULTS`: `role=app-id,...`, the provider of each user role
//!   until Curia holds the user's choice (limen.md §3.2).
//!
//! - `LIMEN_USER`: limend's user name as worker rules name it (default `_limen`).
//!
//! User-role children start through portcullisd's `ExecInstance`
//! ([`limen::launch`]). frescod's slot op (limen.md §12 L2) is not wired yet:
//! binds are refused and logged, so an embed stays unattached instead of
//! half-working.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;

use limen::catalogue::SlotKind;
use limen::daemon::{Compositor, Shared};
use limen::proto::WindowToken;

struct NoSlotsYet;
impl Compositor for NoSlotsYet {
    fn bind(&mut self, _: WindowToken, _: WindowToken, _: SlotKind, rect: Option<[f32; 4]>) -> Result<(u32, u32), u8> {
        if rect.is_some() {
            eprintln!("limend: frescod's slot op is not built yet (limen.md §12 L2)");
        }
        Err(u8::MAX)
    }
}


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
    let shared = Shared::new(Box::new(NoSlotsYet), Box::new(limen::launch::Portcullis::new(std::env::var("LIMEN_USER").unwrap_or_else(|_| "_limen".into()))), resolve, defaults);
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
