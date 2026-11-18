//! `praecod` — notifications (docs/spec/tabula.md §5).
//!
//! `PRAECO_SOCKET` (default `/var/run/atrium/praecod.sock`, mounted into
//! jails with `notify = true`); apps are named from the Portcullis launch
//! registry (`PRAECO_APP_REGISTRY`, default
//! `portcullis_peer::DEFAULT_REGISTRY`), re-read when it changes; only
//! the uids in `PRAECO_SUBSCRIBERS` (comma-separated; Forum's) may
//! subscribe. `PRAECO_SYSTEM_POSTERS` (comma-separated `uid:name`) names
//! system services that may post too, as `name`, to every user's shelf
//! (`proto::SYSTEM_USER`): Forum, telling the user about a monitor.
//! `PRAECO_FRESCOD_ACTIVATION` (default
//! `/var/run/atrium/fresco-activation.sock`): frescod's activation
//! socket, where praecod (as `_praeco`) spends the press behind each
//! notification action (tabula.md §5.3).

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

fn main() -> ExitCode {
    let sock = std::env::var("PRAECO_SOCKET").unwrap_or_else(|_| "/var/run/atrium/praecod.sock".into());
    let registry = std::env::var("PRAECO_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
    let subscribers: Vec<u32> = std::env::var("PRAECO_SUBSCRIBERS").unwrap_or_default()
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let system: Vec<(u32, String)> = std::env::var("PRAECO_SYSTEM_POSTERS").unwrap_or_default()
        .split(',').filter_map(|s| {
            let (uid, name) = s.trim().split_once(':')?;
            Some((uid.parse().ok()?, name.to_string())).filter(|(_, n): &(u32, String)| !n.is_empty())
        }).collect();
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("praecod: bind {sock}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("praecod: listening on {sock}; registry {registry}; subscribers {subscribers:?}; system posters {system:?}");
    // The registry changes as apps launch: read it per lookup (it is a
    // small file; posts are rate-limited).
    let registry2 = registry.clone();
    let resolve = move |uid: u32| {
        if let Some((_, name)) = system.iter().find(|(u, _)| *u == uid) {
            return Some((praeco::proto::SYSTEM_USER.to_string(), name.clone()));
        }
        let reg = portcullis_peer::AppRegistry::load(&registry2).ok()?;
        reg.resolve(uid).map(|(u, a)| (u.to_string(), a.to_string()))
    };
    let reg2 = registry.clone();
    let persona_of = move |uid: u32| {
        let reg = portcullis_peer::AppRegistry::load(&reg2).ok()?;
        reg.persona(uid).flatten().map(str::to_string)
    };
    let act = std::env::var("PRAECO_FRESCOD_ACTIVATION").unwrap_or_else(|_| "/var/run/atrium/fresco-activation.sock".into());
    let gate = tabula::gate::Frescod::new(&act);
    let shared = praeco::server::Shared::with(Box::new(resolve), subscribers, Box::new(persona_of), Box::new(gate));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok((uid, _)) = portcullis_peer::uid_gid(stream.as_raw_fd()) else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Ok(conn) = aqueduct::Connection::wrap(stream) {
                let _ = praeco::server::serve(conn, uid, shared);
            }
        });
    }
    ExitCode::SUCCESS
}
