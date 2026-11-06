//! `arcad` — Arca, the system secret store (docs/spec/arca.md).
//!
//! Environment:
//! - `ARCA_DIR` (default `/var/db/arca`): the per-user stores and audit log.
//! - `ARCA_SOCKET` (default `/atrium/sockets/arca/arcad.sock`).
//! - `ARCA_APP_REGISTRY` (default the launch registry).
//! - `ARCA_VESTIBULUM_UID`, `ARCA_UI_UID`: the system uids allowed the
//!   Vestibulum and trusted-UI ops. Unset = nobody.
//! - `ARCA_ALLOW_UNSIGNED=1`: development only; serves apps launched without
//!   a verified publisher. Logged loudly.
//!
//! arcad hardens itself before reading anything: no core dumps, not
//! traceable. It never opens a network socket.

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arcad::label::{Label, Resolver};

const MAX_CONNS: usize = 256;

fn env_uid(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn main() -> ExitCode {
    if let Err(e) = arca_store::secret::harden_process() {
        eprintln!("arcad: cannot harden process: {e}");
        return ExitCode::FAILURE;
    }
    let dir = std::env::var("ARCA_DIR").unwrap_or_else(|_| "/var/db/arca".into());
    let sock = std::env::var("ARCA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/arca/arcad.sock".into());
    let resolver = Resolver {
        registry: std::env::var("ARCA_APP_REGISTRY")
            .unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into())
            .into(),
        vestibulum_uid: env_uid("ARCA_VESTIBULUM_UID"),
        ui_uid: env_uid("ARCA_UI_UID"),
        allow_unsigned: std::env::var("ARCA_ALLOW_UNSIGNED").as_deref() == Ok("1"),
    };
    if resolver.allow_unsigned {
        eprintln!("arcad: WARNING ARCA_ALLOW_UNSIGNED=1: unsigned apps are served; development only");
    }
    let state = match arcad::server::State::new(dir.clone().into()) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            eprintln!("arcad: state dir {dir}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("arcad: bind {sock}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Anyone may connect: identity is the peer uid, checked per connection.
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666));
    eprintln!("arcad: listening on {sock}, stores in {dir}");
    let live = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if live.load(Ordering::SeqCst) >= MAX_CONNS {
            continue; // dropped: the client sees the connection close
        }
        let label = match portcullis_peer::uid_gid(stream.as_raw_fd()) {
            Ok((uid, _)) => resolver.resolve(uid),
            Err(_) => Label::Unknown,
        };
        let state = state.clone();
        let live = live.clone();
        live.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let _ = arcad::serve_conn(stream, &label, &state);
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
    ExitCode::SUCCESS
}
