//! `tabulad` — the clipboard service (docs/spec/tabula.md).
//!
//! Listens on `TABULA_SOCKET` (default
//! `/atrium/sockets/clipboard/clipboard.sock`, its directory nullfs-mounted
//! into jails whose manifest says `clipboard = true`) and checks every
//! gesture with frescod on `TABULA_FRESCOD_ACTIVATION` (default
//! `/atrium/sockets/fresco-activation/fresco-activation.sock`). If frescod
//! cannot be reached, every copy and paste is refused: there is no mode
//! without the gesture check. Each entry is in the persona of the app that
//! copied it, read from the launch registry (`TABULA_REGISTRY`), and pastes
//! only there unless the person's switch is on (personas.md §6).

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

fn main() -> ExitCode {
    let sock = std::env::var("TABULA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/clipboard/clipboard.sock".into());
    let act = std::env::var("TABULA_FRESCOD_ACTIVATION")
        .unwrap_or_else(|_| "/atrium/sockets/fresco-activation/fresco-activation.sock".into());
    let _ = std::fs::remove_file(&sock);
    if let Some(d) = std::path::Path::new(&sock).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tabulad: bind {sock}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Every app with `clipboard = true` connects; the peer uid is who it is.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666));
    }
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
