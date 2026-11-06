//! `tabulad` — the clipboard service (docs/spec/tabula.md).
//!
//! Listens on `TABULA_SOCKET` (default `/var/run/atrium/tabulad.sock`,
//! nullfs-mounted into jails whose manifest says `clipboard = true`) and
//! checks every gesture with frescod on `TABULA_FRESCOD_ACTIVATION`
//! (default `/var/run/atrium/fresco-activation.sock`). If frescod cannot
//! be reached, every copy and paste is refused: there is no mode without
//! the gesture check.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

fn main() -> ExitCode {
    let sock = std::env::var("TABULA_SOCKET").unwrap_or_else(|_| "/var/run/atrium/tabulad.sock".into());
    let act = std::env::var("TABULA_FRESCOD_ACTIVATION")
        .unwrap_or_else(|_| "/var/run/atrium/fresco-activation.sock".into());
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tabulad: bind {sock}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("tabulad: listening on {sock}, gestures checked with {act}");
    let shared = tabula::server::Shared::new(Box::new(tabula::gate::Frescod::new(act)));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let uid = match portcullis_peer::uid_gid(stream.as_raw_fd()) {
            Ok((uid, _)) => uid,
            Err(e) => {
                eprintln!("tabulad: peer credentials: {e}");
                continue;
            }
        };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Ok(conn) = aqueduct::Connection::wrap(stream) {
                let _ = tabula::server::serve(conn, uid, shared);
            }
        });
    }
    ExitCode::SUCCESS
}
