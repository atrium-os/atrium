//! atrium-prelumd: Atrium's print daemon (docs/spec/printing.md §6.4).
//!
//! ```text
//! atrium-prelumd [--socket PATH] [--state DIR] [--spool DIR] [--network NAME]
//!         [--direct BIN_DIR] [--jaild PATH] [--rctl]
//!         [--netconf DIR | --mdns-legacy ADDR:PORT]
//!         [--sheet-uid UID] [--registry PATH]
//! ```
//!
//! Serves `CLASS_PRINT` on `--socket`. Workers run through jaild
//! (FreeBSD), or with `--direct BIN_DIR` as plain children from that
//! directory (development and the Linux tests only: no jail at all).
//! `--network` names the network for the pin (§7.3) until atrium-net
//! says it (R3). `--rctl` caps each render jail (1 GB, one CPU); it needs
//! `kern.racct.enable=1` and jaild's `allow_rctl`. Printers on the LAN
//! (§7.1) are looked for on the two mDNS sockets atrium-net hands out on
//! atrium-netconf's root-only `print-discovery` socket under `--netconf`
//! (default `/atrium/sockets`), asked each time the list opens; `--mdns-legacy` looks
//! for printers by asking one mDNS responder by unicast (RFC 6762 §6.7;
//! `127.0.0.1:5353` reaches the host's Avahi): development and the Linux
//! tests, where atrium-net isn't there to hand over the LAN's sockets.
//!
//! `--sheet-uid` is prelum-sheet's uid (`_prelum-sheet`), the one peer
//! that may act for an app (`FOR`); the app's person and persona come from
//! the launch registry at `--registry` (portcullis-peer's default).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use prelumd::daemon::{Config, Daemon};
use prelumd::launch::{Direct, Launcher};
use prelumd::nearby::MdnsSockets;

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
    let mut mdns_legacy: Option<std::net::SocketAddr> = None;
    let mut netconf = PathBuf::from("/atrium/sockets");
    let mut sheet_uid: Option<u32> = None;
    let mut registry = portcullis_peer::DEFAULT_REGISTRY.to_string();
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
            "--netconf" => netconf = v.into(),
            "--registry" => registry = v,
            "--sheet-uid" => match v.parse() {
                Ok(u) => sheet_uid = Some(u),
                Err(_) => {
                    eprintln!("prelumd: --sheet-uid UID");
                    return ExitCode::from(2);
                }
            },
            "--mdns-legacy" => match v.parse() {
                Ok(a) => mdns_legacy = Some(a),
                Err(_) => {
                    eprintln!("prelumd: --mdns-legacy ADDR:PORT");
                    return ExitCode::from(2);
                }
            },
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
    let mdns: Option<Box<dyn Fn() -> std::io::Result<MdnsSockets> + Send + Sync>> = match mdns_legacy {
        Some(a) => Some(Box::new(move || {
            let s = std::net::UdpSocket::bind(if a.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
            s.connect(a)?;
            let r = s.try_clone()?;
            Ok(MdnsSockets { recv: r.into(), send: s.into(), legacy: true })
        })),
        None => Some(Box::new(move || {
            use atrium_netconf::{client::Client, Grant};
            let mut c = Client::connect(&Client::path(&netconf, Grant::PrintDiscovery))?;
            let (_iface, recv, send) = c.print_discovery().map_err(|f| std::io::Error::other(f.1))?;
            Ok(MdnsSockets { recv, send, legacy: false })
        })),
    };
    // An app's uid to its person (the registry's owning user) and persona,
    // read fresh each time: apps come and go.
    let person_of = Box::new(move |app: u32| {
        let reg = portcullis_peer::AppRegistry::load(&registry).ok()?;
        let (user, _) = reg.resolve(app)?;
        let person = uid_of(user)?;
        Some((person, reg.persona(app).flatten().map(str::to_string)))
    });
    let d = Daemon::new(Config { state, spool, network: Box::new(move || network.clone()), mdns, sheet_uid, person_of }, launcher);
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

/// A user name's uid (getpwnam_r).
fn uid_of(user: &str) -> Option<u32> {
    let c = std::ffi::CString::new(user).ok()?;
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: a zeroed passwd is a valid out-parameter; buf outlives the call.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live buffer of the size given.
    let r = unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    (r == 0 && !out.is_null()).then_some(pw.pw_uid)
}
