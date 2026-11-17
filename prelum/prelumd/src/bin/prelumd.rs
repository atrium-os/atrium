//! atrium-prelumd: Atrium's print daemon (docs/spec/printing.md §6.4).
//!
//! ```text
//! atrium-prelumd [--socket PATH] [--state DIR] [--spool DIR] [--network NAME]
//!         [--direct BIN_DIR] [--jaild PATH] [--rctl]
//! ```
//!
//! Serves `CLASS_PRINT` on `--socket`. Workers run through jaild
//! (FreeBSD), or with `--direct BIN_DIR` as plain children from that
//! directory (development and the Linux tests only: no jail at all).
//! `--network` names the network for the pin (§7.3) until atrium-net
//! says it (R3). `--rctl` caps each render jail (1 GB, one CPU); it needs
//! `kern.racct.enable=1` and jaild's `allow_rctl`.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use prelumd::daemon::{Config, Daemon};
use prelumd::launch::{Direct, Launcher};

const SOCKET: &str = "/atrium/sockets/print/print.sock";
const STATE: &str = "/var/db/atrium/prelum";
const SPOOL: &str = "/var/spool/atrium/prelum";

#[cfg_attr(not(target_os = "freebsd"), allow(unused_assignments, unused_variables))]
fn main() -> ExitCode {
    let mut socket = PathBuf::from(SOCKET);
    let mut state = PathBuf::from(STATE);
    let mut spool = PathBuf::from(SPOOL);
    let mut network = "local".to_string();
    let mut direct: Option<PathBuf> = None;
    let mut rctl = false;
    let mut jaild = "/var/run/atrium/jaild.sock".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        if k == "--rctl" {
            rctl = true;
            continue;
        }
        let Some(v) = it.next() else {
            eprintln!("prelumd: {k} needs a value");
            return ExitCode::from(2);
        };
        match k.as_str() {
            "--socket" => socket = v.into(),
            "--state" => state = v.into(),
            "--spool" => spool = v.into(),
            "--network" => network = v,
            "--direct" => direct = Some(v.into()),
            "--jaild" => jaild = v,
            _ => {
                eprintln!("prelumd: unknown option {k}");
                return ExitCode::from(2);
            }
        }
    }
    let launcher: Box<dyn Launcher> = match direct {
        Some(bin_dir) => {
            eprintln!("prelumd: workers run unjailed from {} (development only)", bin_dir.display());
            Box::new(Direct { bin_dir })
        }
        None => {
            #[cfg(target_os = "freebsd")]
            {
                match prelumd::jail::Jailed::new(&jaild, rctl) {
                    Ok(j) => Box::new(j),
                    Err(e) => {
                        eprintln!("prelumd: {e}");
                        return ExitCode::from(1);
                    }
                }
            }
            #[cfg(not(target_os = "freebsd"))]
            {
                eprintln!("prelumd: jails need FreeBSD; use --direct BIN_DIR to develop");
                return ExitCode::from(2);
            }
        }
    };
    let d = Daemon::new(Config { state, spool, network: Box::new(move || network.clone()) }, launcher);
    if let Some(dir) = socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&socket);
    let l = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("prelumd: {}: {e}", socket.display());
            return ExitCode::from(1);
        }
    };
    // Anyone may connect; each sees only their own printers and jobs.
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666));
    eprintln!("prelumd: serving {}", socket.display());
    for s in l.incoming() {
        match s {
            Ok(s) => {
                let d = d.clone();
                std::thread::spawn(move || {
                    let _ = prelumd::server::serve(s, d);
                });
            }
            Err(e) => eprintln!("prelumd: accept: {e}"),
        }
    }
    ExitCode::SUCCESS
}
