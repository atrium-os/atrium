//! arca-courierd: start each courier in its own jail (arca.md §5.2).
//!
//! Runs as root (jaild takes only root callers) and is never jailed
//! itself; it holds no secret and reads no item.
//!
//! Environment (defaults in brackets):
//!   ARCA_COURIERD_SOCKET     [/atrium/sockets/arca-courier/courierd.sock]
//!   ARCA_APP_REGISTRY        [/var/run/atrium/app-registry]
//!   ARCA_COURIER_REGISTRY    [/var/run/atrium/courier-registry]
//!   ARCA_COURIERD_CA         [/etc/ssl/cert.pem] copied into each courier's root
//!   JAILD_SOCKET             [/var/run/atrium/jaild.sock]
//!   ARCA_ALLOW_UNSIGNED      [unset; dev only, as for arcad]
//!   ARCA_COURIERD_GRANT_PRIVATE_NETWORK=1   tests only: couriers may reach LAN addresses

#[cfg(not(target_os = "freebsd"))]
fn main() -> std::process::ExitCode {
    eprintln!("arca-courierd: jails are FreeBSD's");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "freebsd")]
fn main() -> std::process::ExitCode {
    freebsd::main()
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use std::net::ToSocketAddrs;
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};

    use arca_courierd::jail::{self, Config};
    use arca_courierd::{admit, decode_request, read_request, reply, reset_registry, send_reply, Pool};
    use arca_store::{Recipient, Scheme};
    use arcad::courier::{app_persona, CourierLabel};
    use arcad::label::{Label, Resolver};
    use navigator_fetch::addr;

    fn env(k: &str, d: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| d.to_string())
    }

    struct Daemon {
        cfg: Config,
        resolver: Resolver,
        policy: addr::Policy,
        pool: Mutex<Pool>,
    }

    pub fn main() -> ExitCode {
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("arca-courierd: jaild serves only root");
            return ExitCode::FAILURE;
        }
        let grant = std::env::var("ARCA_COURIERD_GRANT_PRIVATE_NETWORK").as_deref() == Ok("1");
        let ca = match std::fs::read(env("ARCA_COURIERD_CA", "/etc/ssl/cert.pem")) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("arca-courierd: trust store: {e}");
                return ExitCode::FAILURE;
            }
        };
        let registry = env("ARCA_COURIER_REGISTRY", arcad::courier::DEFAULT_REGISTRY);
        let d = Arc::new(Daemon {
            cfg: Config { jaild: env("JAILD_SOCKET", jail::JAILD_SOCKET), registry: registry.clone(), ca, grant_private_network: grant },
            resolver: Resolver {
                registry: env("ARCA_APP_REGISTRY", "/var/run/atrium/app-registry").into(),
                vestibulum_uid: None,
                ui_uid: None,
                allow_unsigned: std::env::var_os("ARCA_ALLOW_UNSIGNED").is_some(),
                courier_registry: None,
            },
            policy: addr::Policy { grant_private_network: grant },
            pool: Mutex::new(Pool::default()),
        });
        // No courier is live before the first launch: whatever a previous
        // launcher left goes, then its labels.
        jail::sweep(&d.cfg);
        if let Err(e) = reset_registry(&registry) {
            eprintln!("arca-courierd: registry {registry}: {e}");
            return ExitCode::FAILURE;
        }

        let path = env("ARCA_COURIERD_SOCKET", arca_courierd::DEFAULT_SOCKET);
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_file(&path);
        let l = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("arca-courierd: bind {path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        // Any app may ask; who it is comes from the kernel.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
        eprintln!("arca-courierd: listening on {path}");
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let d = d.clone();
            std::thread::spawn(move || serve(&d, s));
        }
        ExitCode::SUCCESS
    }

    fn peer_uid(s: &UnixStream) -> Option<u32> {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: two out-parameters on a connected unix socket.
        (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
    }

    fn serve(d: &Daemon, mut s: UnixStream) {
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let (status, courier) = match ask(d, &mut s) {
            Ok(c) => (reply::OK, Some(c)),
            Err(code) => (code, None),
        };
        let sent = send_reply(&s, status, courier.as_ref().map(|c| c.client.as_fd()));
        drop(s);
        let Some(c) = courier else { return };
        if let Err(e) = sent {
            eprintln!("arca-courierd: courier {} for a client that left: {e}", c.uid);
        }
        // The app's end now lives only in the app; when the app closes it
        // the courier reads EOF and exits.
        drop(c.client);
        if let Err(e) = jail::wait(&c.procdesc) {
            eprintln!("arca-courierd: wait {}: {e}", c.uid);
        }
        jail::retire(&d.cfg, c.uid);
        jail::remove(&d.cfg, c.uid);
        d.pool.lock().unwrap().give_back(c.uid);
    }

    fn ask(d: &Daemon, s: &mut UnixStream) -> Result<jail::Courier, u8> {
        let uid = peer_uid(s).ok_or(reply::NOT_AN_APP)?;
        let Label::App { user, publisher, app_id } = d.resolver.resolve(uid) else { return Err(reply::NOT_AN_APP) };
        let body = read_request(s).map_err(|_| reply::BAD_ENDPOINT)?;
        let (host, port) = decode_request(&body).ok_or(reply::BAD_ENDPOINT)?;
        let resolved = (host.as_str(), port).to_socket_addrs().map_err(|_| reply::NO_ADDRESS)?;
        let addrs = admit(resolved, d.policy);
        if addrs.is_empty() {
            return Err(reply::NO_ADDRESS);
        }
        let label = CourierLabel {
            user: user.clone(),
            persona: app_persona(&publisher, &app_id),
            endpoint: Recipient::endpoint(Scheme::Https, &host, port, None).map_err(|_| reply::BAD_ENDPOINT)?,
        };
        let owner = format!("{user} {} {app_id}", label.persona);
        let cuid = d.pool.lock().unwrap().take(&owner)?;
        match jail::launch(&d.cfg, cuid, &label, &addrs) {
            Ok(c) => {
                eprintln!("arca-courierd: courier {cuid} for {app_id} ({user}) to {host}:{port} via {addrs:?}");
                Ok(c)
            }
            Err(e) => {
                eprintln!("arca-courierd: courier {cuid} for {app_id} to {host}:{port}: {e}");
                d.pool.lock().unwrap().give_back(cuid);
                Err(reply::FAILED)
            }
        }
    }
}
