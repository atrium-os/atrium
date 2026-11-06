//! limend, the embed broker (docs/spec/limen.md).
//!
//! Environment:
//! - `LIMEN_SOCKET` (default /var/run/atrium/limend.sock)
//! - `LIMEN_APP_REGISTRY` (default the launch registry)
//! - `LIMEN_DEFAULTS`: `role=app-id,...`, the provider of each user role
//!   until Curia holds the user's choice (limen.md §3.2).
//!
//! Not wired yet: frescod's slot op (limen.md §12 L2) and portcullisd's
//! embed-mode launch (L3). Until they land, binds and launches are refused
//! and logged, so an embed stays unattached instead of half-working.

use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use limen::catalogue::SlotKind;
use limen::daemon::{Compositor, Launcher, Shared};
use limen::proto::{EmbedId, WindowToken};

struct NoSlotsYet;
impl Compositor for NoSlotsYet {
    fn bind(&mut self, _: WindowToken, _: WindowToken, _: SlotKind, rect: Option<[f32; 4]>) -> Result<(u32, u32), u8> {
        if rect.is_some() {
            eprintln!("limend: frescod's slot op is not built yet (limen.md §12 L2)");
        }
        Err(u8::MAX)
    }
}

struct NoLaunchYet;
impl Launcher for NoLaunchYet {
    fn launch(&mut self, app: &str, _: EmbedId, _: UnixStream) -> Result<u32, String> {
        Err(format!("embed-mode launch of {app} is not built yet (limen.md §12 L3)"))
    }
    fn end(&mut self, _: EmbedId) {}
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
    let shared = Shared::new(Box::new(NoSlotsYet), Box::new(NoLaunchYet), resolve, defaults);
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
