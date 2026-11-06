//! `praecod` — notifications (docs/spec/tabula.md §5).
//!
//! `PRAECO_SOCKET` (default `/var/run/atrium/praecod.sock`, mounted into
//! jails with `notify = true`); apps are named from the Portcullis launch
//! registry (`PRAECO_APP_REGISTRY`, default
//! `portcullis_peer::DEFAULT_REGISTRY`), re-read when it changes; only
//! the uids in `PRAECO_SUBSCRIBERS` (comma-separated; Forum's) may
//! subscribe.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

fn main() -> ExitCode {
    let sock = std::env::var("PRAECO_SOCKET").unwrap_or_else(|_| "/var/run/atrium/praecod.sock".into());
    let registry = std::env::var("PRAECO_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
    let subscribers: Vec<u32> = std::env::var("PRAECO_SUBSCRIBERS").unwrap_or_default()
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("praecod: bind {sock}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("praecod: listening on {sock}; registry {registry}; subscribers {subscribers:?}");
    // The registry changes as apps launch: read it per lookup (it is a
    // small file; posts are rate-limited).
    let resolve = move |uid: u32| {
        let reg = portcullis_peer::AppRegistry::load(&registry).ok()?;
        reg.resolve(uid).map(|(u, a)| (u.to_string(), a.to_string()))
    };
    let shared = praeco::server::Shared::new(Box::new(resolve), subscribers);
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
