//! The identity-device agent (docs/spec/sigillum.md §4, §5.1): who gets each
//! `hidraw` node.
//!
//! The kernel classifies every HID device by its top-level collections and
//! says so on devd; [`atrium_devevents::hidowner`] turns that into
//! decisions, and this thread carries them out as root:
//!
//! - an **input** device's node is shown to tactusd in its jail's own devfs
//!   mount (`devfs -m <tactusd>/dev rule apply path hidrawN unhide`).
//!   Ruleset 26 no longer shows `hidraw*`, so a node nobody classified, or
//!   a security key, is never in tactusd's `/dev`. Every 2 s the agent
//!   re-shows the input nodes, because a restarted tactusd gets a fresh
//!   mount where everything is hidden again;
//! - an **identity** device (a FIDO collection) gets a jail of its own,
//!   `atrium-sigillum-fido-N`, on ruleset 27, shown that one node by jaild
//!   (`devfs_grant`, owned by `_sigillum-dev`, 0600), running
//!   `atrium-sigillum-fido` as `_sigillum-dev` with no network and the
//!   `device` syscall profile; it connects to `sigillumd`'s driver socket
//!   (the only socket path it sees) and enters Capsicum. Unplugging the
//!   key removes the jail;
//! - anything else stays hidden.
//!
//! Devices already present when the agent starts are read from
//! `devinfo -v`, since devd does not replay attaches.
//!
//! `PORTCULLIS_IDENTITY_DEVICES=off` disables the agent (then tactusd sees
//! no HID devices at all: fail closed).

pub const TACTUS_DEV: &str = "/var/lib/atrium/jails/tactusd/dev";
pub const DRIVER_BIN: &str = "/usr/local/bin/atrium-sigillum-fido";
pub const DRIVER_SOCKET_DIR: &str = "/atrium/sockets/sigillum-dev";
pub const DRIVER_UID: u32 = 65029;
pub const IDENTITY_RULESET: u32 = 27;
const JAIL_PREFIX: &str = "atrium-sigillum-fido-";
const JAIL_ROOTS: &str = "/var/lib/atrium/jails";

/// The jaild request for `hidrawN`'s driver jail.
pub fn driver_request(hidraw: u32) -> jaild::protocol::CreateJailRequest {
    use jaild::protocol::{CreateJailRequest, ExecSpec, MountKind, MountSpec, NetworkConfig};
    let ro = |p: &str| MountSpec {
        source: p.to_string(),
        dest: p.trim_start_matches('/').to_string(),
        kind: MountKind::RoNullfs,
        size_mb: None,
        owner: None,
    };
    let node = format!("hidraw{hidraw}");
    CreateJailRequest {
        rctl: None,
        name: format!("{JAIL_PREFIX}{hidraw}"),
        path: format!("{JAIL_ROOTS}/sigillum-fido-{hidraw}"),
        children_max: 0,
        devfs_ruleset: IDENTITY_RULESET,
        network: NetworkConfig::Disable,
        mounts: vec![ro("/libexec"), ro("/lib"), ro("/usr/lib"), ro("/usr/local/bin"), ro(DRIVER_SOCKET_DIR)],
        exec: Some(ExecSpec {
            path: DRIVER_BIN.into(),
            argv: vec![
                "atrium-sigillum-fido".into(),
                "--dev".into(),
                format!("/dev/{node}"),
                "--connect".into(),
                format!("{DRIVER_SOCKET_DIR}/drivers.sock"),
            ],
            env: vec![],
            uid: DRIVER_UID,
            gid: DRIVER_UID,
            stdio: false,
        }),
        hostname: None,
        hostid: None,
        hostuuid: None,
        allow_mlock: false,
        syscall_profile: Some("device".into()),
        devfs_grant: Some(node),
    }
}

#[cfg(target_os = "freebsd")]
pub use run::spawn;

#[cfg(not(target_os = "freebsd"))]
pub fn spawn(_jaild_socket: &'static str) {}

#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)] // poll(2) and adopting jaild's procdesc
mod run {
    use std::collections::BTreeMap;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    use atrium_devevents::hidowner::{Op, Owners};
    use atrium_devevents::{DeviceWatcher, Event};

    use super::*;
    use crate::jaild_client::Client;

    const RESHOW: Duration = Duration::from_secs(2);

    struct Agent {
        jaild: &'static str,
        owners: Owners,
        drivers: BTreeMap<u32, OwnedFd>,
        /// The last failure showing each input node, so a tactusd that is
        /// down is logged once, not every 2 s.
        show_errors: BTreeMap<u32, String>,
    }

    pub fn spawn(jaild_socket: &'static str) {
        if std::env::var("PORTCULLIS_IDENTITY_DEVICES").as_deref() == Ok("off") {
            eprintln!("portcullisd: identity-device agent off: tactusd gets no HID devices");
            return;
        }
        std::thread::spawn(move || {
            let mut a = Agent { jaild: jaild_socket, owners: Owners::default(), drivers: BTreeMap::new(), show_errors: BTreeMap::new() };
            loop {
                // Listen first, then enumerate, so nothing falls between.
                let w = match DeviceWatcher::open() {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("portcullisd: identity agent: devd: {e}; retrying in 5 s");
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                };
                match std::process::Command::new("devinfo").arg("-v").output() {
                    Ok(o) if o.status.success() => {
                        let ops = a.owners.devinfo(&String::from_utf8_lossy(&o.stdout));
                        a.run(ops);
                    }
                    other => eprintln!("portcullisd: identity agent: devinfo -v: {other:?}"),
                }
                a.serve(&w);
                eprintln!("portcullisd: identity agent: devd connection lost; reconnecting");
            }
        });
    }

    impl Agent {
        fn serve(&mut self, w: &DeviceWatcher) {
            let mut last = Instant::now();
            loop {
                let mut p = libc::pollfd { fd: w.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                // SAFETY: one valid pollfd.
                let r = unsafe { libc::poll(&mut p, 1, RESHOW.as_millis() as libc::c_int) };
                if r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
                if r > 0 {
                    match w.recv() {
                        Ok(Event::Other { raw }) => {
                            let ops = self.owners.line(&raw);
                            self.run(ops);
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
                if last.elapsed() >= RESHOW {
                    last = Instant::now();
                    let ops: Vec<Op> = self.owners.input_nodes().map(|hidraw| Op::ShowInput { hidraw }).collect();
                    self.run(ops);
                }
            }
        }

        fn run(&mut self, ops: Vec<Op>) {
            for op in ops {
                match op {
                    Op::ShowInput { hidraw } => self.show_input(hidraw),
                    Op::StartIdentity { hidraw, vendor, product, .. } => {
                        self.stop(hidraw);
                        self.start(hidraw, vendor, product);
                    }
                    Op::StopIdentity { hidraw } => self.stop(hidraw),
                    Op::Log(m) => eprintln!("portcullisd: identity agent: {m}"),
                }
            }
        }

        fn show_input(&mut self, hidraw: u32) {
            let node = format!("hidraw{hidraw}");
            let dev = std::path::Path::new(TACTUS_DEV);
            // Already visible there (a hidden node does not resolve), or no
            // host node at all (unplugged meanwhile): nothing to do.
            if dev.join(&node).exists() || !std::path::Path::new("/dev").join(&node).exists() {
                self.show_errors.remove(&hidraw);
                return;
            }
            let out = std::process::Command::new("devfs").args(["-m", TACTUS_DEV, "rule", "apply", "path", &node, "unhide"]).output();
            let err = match out {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Err(e) => Some(e.to_string()),
            };
            match err {
                None => {
                    self.show_errors.remove(&hidraw);
                    eprintln!("portcullisd: identity agent: {node} shown to tactusd");
                }
                Some(e) => {
                    if self.show_errors.get(&hidraw) != Some(&e) {
                        eprintln!("portcullisd: identity agent: show {node} to tactusd: {e} (is tactusd running?)");
                        self.show_errors.insert(hidraw, e);
                    }
                }
            }
        }

        fn start(&mut self, hidraw: u32, vendor: u16, product: u16) {
            let req = driver_request(hidraw);
            let name = req.name.clone();
            let sent = Client::connect(self.jaild).and_then(|mut c| c.send(&jaild::protocol::Request::CreateJail(req)));
            match sent {
                Ok((jaild::protocol::Response::JailCreated(r), fd)) => {
                    eprintln!("portcullisd: identity agent: hidraw{hidraw} ({vendor:04x}:{product:04x}) -> {name} (jid {}, pid {})", r.jid, r.pid);
                    if let Some(raw) = fd {
                        // SAFETY: jaild just passed us this procdesc and we own it;
                        // holding it keeps the driver's persist=0 jail alive.
                        self.drivers.insert(hidraw, unsafe { OwnedFd::from_raw_fd(raw) });
                    }
                }
                Ok((other, _)) => eprintln!("portcullisd: identity agent: {name}: jaild: {other:?}"),
                Err(e) => eprintln!("portcullisd: identity agent: {name}: jaild: {e}"),
            }
        }

        fn stop(&mut self, hidraw: u32) {
            let req = driver_request(hidraw);
            let had = self.drivers.remove(&hidraw).is_some();
            let rm = jaild::protocol::Request::RemoveJail { jid: None, name: Some(req.name.clone()) };
            let _ = Client::connect(self.jaild).and_then(|mut c| c.send(&rm));
            // jaild's exec path does not record create-time mounts.
            for m in &req.mounts {
                let _ = crate::host_mount::unmount_jail_dest(&req.path, &m.dest);
            }
            if had {
                eprintln!("portcullisd: identity agent: {} removed", req.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_jail_is_narrow() {
        let r = driver_request(7);
        assert_eq!(r.name, "atrium-sigillum-fido-7");
        assert_eq!(r.devfs_ruleset, 27);
        assert_eq!(r.devfs_grant.as_deref(), Some("hidraw7"));
        assert!(matches!(r.network, jaild::protocol::NetworkConfig::Disable));
        assert_eq!(r.syscall_profile.as_deref(), Some("device"));
        let e = r.exec.unwrap();
        assert_eq!((e.uid, e.gid), (65029, 65029));
        assert!(e.argv.windows(2).any(|w| w[0] == "--dev" && w[1] == "/dev/hidraw7"));
        assert!(r.mounts.iter().all(|m| matches!(m.kind, jaild::protocol::MountKind::RoNullfs)));
        assert!(!r.mounts.iter().any(|m| m.source == "/atrium/sockets"), "only the driver socket's own directory");
    }
}
