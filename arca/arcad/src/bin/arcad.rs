//! `arcad` — Arca, the system secret store (docs/spec/arca.md).
//!
//! Environment:
//! - `ARCA_DIR` (default `/var/db/arca`): the per-user stores and audit log.
//! - `ARCA_SOCKET` (default `/atrium/sockets/arca/arcad.sock`).
//! - `ARCA_APP_REGISTRY` (default the launch registry).
//! - `ARCA_WEB_BROKER`: `app_id publisher-hex`, the app that issues web
//!   tickets (arca.md §8.2); unset, no web courier gets a cookie.
//! - `ARCA_VESTIBULUM_UID`, `ARCA_UI_UID`: the system uids allowed the
//!   Vestibulum and trusted-UI ops. Unset = nobody.
//! - `ARCA_SIGILLUM_SOCKET` (default `/atrium/sockets/sigillum/sigillumd.sock`):
//!   sigillumd, which verifies the user before each use of a
//!   verify-each-use item (sigillum.md §8); `none`: those items stay
//!   unusable (`NEEDS_VERIFICATION`).
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
        courier_registry: Some(
            std::env::var("ARCA_COURIER_REGISTRY").unwrap_or_else(|_| arcad::courier::DEFAULT_REGISTRY.into()).into(),
        ),
        web_broker: std::env::var("ARCA_WEB_BROKER").ok().and_then(|s| arcad::label::parse_web_broker(&s)),
        services: std::env::var("ARCA_SERVICES").ok().map(|s| arcad::label::parse_services(&s)).unwrap_or_default(),
    };
    if resolver.allow_unsigned {
        eprintln!("arcad: WARNING ARCA_ALLOW_UNSIGNED=1: unsigned apps are served; development only");
    }
    let state = match arcad::server::State::new(dir.clone().into()) {
        Ok(mut s) => {
            // The machine store (§6.4) serves system services at the login
            // screen; a failure here leaves users' stores working.
            if let Err(e) = s.open_machine() {
                eprintln!("arcad: machine store: {e:?}");
            }
            Arc::new(Mutex::new(s))
        }
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
    let verifier: Option<Arc<dyn arcad::verify::Verify>> = match std::env::var("ARCA_SIGILLUM_SOCKET") {
        Ok(v) if v == "none" => None,
        v => Some(Arc::new(arcad::verify::Sigillum::new(v.unwrap_or_else(|_| "/atrium/sockets/sigillum/sigillumd.sock".into())))),
    };
    let live = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if live.load(Ordering::SeqCst) >= MAX_CONNS {
            continue; // dropped: the client sees the connection close
        }
        let (uid, label) = match portcullis_peer::uid_gid(stream.as_raw_fd()) {
            Ok((uid, _)) => (uid, resolver.resolve(uid)),
            Err(_) => (u32::MAX, Label::Unknown),
        };
        let state = state.clone();
        let live = live.clone();
        let verifier = verifier.clone();
        live.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            if let Err(e) = arcad::serve_conn_with(stream, &label, &state, verifier.as_deref()) {
                // No content, only the kind of failure and the peer uid.
                eprintln!("arcad: connection from uid {uid} ended: {:?}", e.kind());
            }
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
    ExitCode::SUCCESS
}
