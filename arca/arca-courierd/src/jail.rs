//! One jail per courier, made by jaild (`[couriers]` in its policy).
//!
//! The jail's root is `/var/lib/atrium/jails/system-arca-courier-<uid>`:
//! the base libraries and the installed binaries read-only, arcad's socket
//! directory, the courier registry (read-only, one file) and a copy of
//! the trust store. Nothing of the user's, no other service's socket. Its
//! network is its own vnet whose pf anchor passes only the endpoint's
//! addresses on its port (and the host's resolvers, which jaild adds to
//! every restricted anchor). It may lock memory: it holds a secret while
//! it builds a request.

use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use arcad::courier::CourierLabel;
use jaild::protocol::{
    CreateJailRequest, EnvPair, ExecSpec, MountKind, MountSpec, NetDest, NetGrants, NetworkConfig, Request, Response,
};

pub const JAILD_SOCKET: &str = "/var/run/atrium/jaild.sock";
pub const JAILS_DIR: &str = "/var/lib/atrium/jails";
/// jaild's `[couriers]`: these two must match its policy.
pub const NAME_PREFIX: &str = "system-arca-courier-";
pub const EXEC_PATH: &str = "/usr/local/bin/atrium-arca-courier";
/// The app devfs ruleset (portcullis_jail::APP_DEVFS_RULESET): null,
/// zero, random and little else.
const DEVFS_RULESET: u32 = 22;
/// How many of portcullis_mounts' 10 s waits a dying jail gets.
const DYING_TRIES: u32 = 18;

pub struct Config {
    pub jaild: String,
    pub registry: String,
    /// The trust store's bytes, copied into every root (a symlinked
    /// /etc/ssl would point outside it).
    pub ca: Vec<u8>,
    /// Tests only: the courier admits private addresses too.
    pub grant_private_network: bool,
}

pub fn name(uid: u32) -> String {
    format!("{NAME_PREFIX}{uid}")
}

pub fn root(uid: u32) -> PathBuf {
    Path::new(JAILS_DIR).join(name(uid))
}

pub struct Courier {
    pub uid: u32,
    /// The app's end.
    pub client: OwnedFd,
    /// The courier process; readable at its exit.
    pub procdesc: OwnedFd,
}

/// Label `uid`, build its root, and have jaild start the courier in it.
/// On any failure the label is retired and the root removed before the
/// error returns, so the uid can go back to the pool.
pub fn launch(cfg: &Config, uid: u32, label: &CourierLabel, addrs: &[Ipv4Addr]) -> Result<Courier, String> {
    let line = arcad::courier::label_line(uid, label).ok_or("label has no endpoint")?;
    let arca_store::Recipient::Endpoint { port, .. } = &label.endpoint else { return Err("label has no endpoint".into()) };
    let port = *port;
    let root = root(uid);
    teardown(&name(uid), &root);
    build_root(cfg, &root).inspect_err(|_| teardown(&name(uid), &root))?;
    arcad::courier::append(&cfg.registry, &line).map_err(|e| {
        teardown(&name(uid), &root);
        format!("registry: {e}")
    })?;
    match start(cfg, uid, &root, port, addrs) {
        Ok(c) => Ok(c),
        Err(e) => {
            retire(cfg, uid);
            remove(cfg, uid);
            Err(e)
        }
    }
}

fn build_root(cfg: &Config, root: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    std::fs::create_dir_all(root.join("etc/ssl")).map_err(|e| io("root", e))?;
    std::fs::create_dir_all(root.join("var/run/atrium")).map_err(|e| io("root", e))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(root.join("etc/ssl/cert.pem"))
        .map_err(|e| io("trust store", e))?;
    std::io::Write::write_all(&mut f, &cfg.ca).map_err(|e| io("trust store", e))?;
    // A file mount needs a file to land on; jaild makes only directories.
    std::fs::File::create(root.join("var/run/atrium/courier-registry")).map_err(|e| io("registry mountpoint", e))?;
    Ok(())
}

fn start(cfg: &Config, uid: u32, root: &Path, port: u16, addrs: &[Ipv4Addr]) -> Result<Courier, String> {
    let ro = |p: &str| MountSpec { source: p.into(), dest: p.into(), kind: MountKind::RoNullfs, size_mb: None, owner: None };
    let mut mounts = vec![ro("/lib"), ro("/libexec"), ro("/usr/lib"), ro("/usr/local/bin"), ro(&cfg.registry)];
    mounts[4].dest = arcad::courier::DEFAULT_REGISTRY.into();
    mounts.push(MountSpec {
        source: "/atrium/sockets/arca".into(),
        dest: "/atrium/sockets/arca".into(),
        kind: MountKind::RwNullfs,
        size_mb: None,
        owner: None,
    });
    let id = portcullis_identity::for_app(&format!("org.atrium.arca.courier.{uid}")).map_err(|e| format!("host identity: {e}"))?;
    let grants = NetGrants {
        app_key: portcullis_identity::consent_key(&name(uid)),
        outbound_any: false,
        outbound: addrs.iter().map(|a| NetDest { cidr: format!("{a}/32"), port: Some(port), udp: false }).collect(),
        peers: vec![],
        inbound: vec![],
    };
    let list: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
    let mut env = vec![
        EnvPair { key: "ATRIUM_COURIER_ADDRS".into(), value: list.join(",") },
        EnvPair { key: "PATH".into(), value: "/usr/bin:/bin".into() },
    ];
    if cfg.grant_private_network {
        env.push(EnvPair { key: "ATRIUM_COURIER_GRANT_PRIVATE_NETWORK".into(), value: "1".into() });
    }
    let req = Request::CreateJail(CreateJailRequest {
        name: name(uid),
        path: root.to_string_lossy().into_owned(),
        children_max: 0,
        mounts,
        devfs_ruleset: DEVFS_RULESET,
        network: NetworkConfig::Routed { mac: id.mac.clone(), grants: Some(grants) },
        exec: Some(ExecSpec { path: EXEC_PATH.into(), argv: vec![EXEC_PATH.into()], env, uid, gid: uid, stdio: true }),
        rctl: None,
        hostname: Some(name(uid)),
        hostid: Some(id.hostid),
        hostuuid: Some(id.hostuuid.clone()),
        allow_mlock: true,
    });

    let (app, courier) = std::os::unix::net::UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    let mut c = jaild::client::Client::connect(&cfg.jaild).map_err(|e| format!("connect {}: {e}", cfg.jaild))?;
    // stdin and stdout are the socket; stderr is the launcher's log.
    let err = std::io::stderr();
    let sent = c.send_with_fds(&req, &[courier.as_fd(), courier.as_fd(), err.as_fd()], 1);
    drop(courier);
    let (resp, got) = sent.map_err(|e| format!("jaild: {e}"))?;
    // SAFETY: jaild handed these over; each is owned once.
    let mut got: Vec<OwnedFd> = got.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect();
    match resp {
        Response::JailCreated(r) if r.procdesc_attached => match got.pop() {
            Some(pd) => Ok(Courier { uid, client: app.into(), procdesc: pd }),
            None => Err("jaild sent no procdesc".into()),
        },
        Response::PolicyDenied { rule, detail } => Err(format!("jaild refused ({rule}): {detail}")),
        Response::SyscallFailed { name, errno, msg } => Err(format!("jaild: {name} (errno {errno}): {msg}")),
        other => Err(format!("jaild: unexpected {other:?}")),
    }
}

/// Block until the courier exits.
pub fn wait(pd: &OwnedFd) -> std::io::Result<i32> {
    // SAFETY: kqueue returns a new descriptor or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: just created, owned here.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    // SAFETY: a zeroed kevent is a valid value to fill in.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = pd.as_raw_fd() as usize;
    ev.filter = libc::EVFILT_PROCDESC;
    ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
    ev.fflags = libc::NOTE_EXIT;
    loop {
        // SAFETY: as above.
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: one change, one event, both valid.
        let n = unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, &mut out, 1, std::ptr::null()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 1 && out.fflags & libc::NOTE_EXIT != 0 {
            return Ok(out.data as i32);
        }
    }
}

/// End `uid`'s label. Done first when a courier exits, so arcad stops
/// filling for that uid before anything else happens to it.
pub fn retire(cfg: &Config, uid: u32) {
    if let Err(e) = arcad::courier::append(&cfg.registry, &format!("{uid} {}", arcad::courier::RETIRED)) {
        eprintln!("arca-courierd: retire {uid}: {e}");
    }
}

/// jaild's record, the jail's network and mounts, and the root.
pub fn remove(cfg: &Config, uid: u32) {
    if let Ok(mut c) = jaild::client::Client::connect(&cfg.jaild) {
        let _ = c.send(&Request::RemoveJail { jid: None, name: Some(name(uid)) });
    }
    teardown(&name(uid), &root(uid));
}

fn teardown(name: &str, root: &Path) {
    let _ = std::process::Command::new("jail").arg("-r").arg(name).stderr(std::process::Stdio::null()).status();
    // A vnet jail stays dying while its TCP connections sit in TIME_WAIT
    // (2 MSL, about a minute), and its mounts stay pinned until it goes.
    // This thread has nothing else to do: wait it out.
    for _ in 0..DYING_TRIES {
        if portcullis_mounts::wait_jail_gone(name, portcullis_mounts::JAIL_GONE_TIMEOUT) {
            break;
        }
    }
    let left = portcullis_mounts::converge(&[root], portcullis_mounts::Force::Yes);
    portcullis_mounts::warn_survivors("arca-courierd", &left);
    // ★ Never remove_dir_all: a mount that survived would take the
    // removal into /atrium/sockets/arca. Only what build_root wrote is
    // unlinked, then directories that are empty.
    let _ = std::fs::remove_file(root.join("etc/ssl/cert.pem"));
    let _ = std::fs::remove_file(root.join("var/run/atrium/courier-registry"));
    for d in [
        "lib", "libexec", "usr/lib", "usr/local/bin", "usr/local", "usr", "atrium/sockets/arca", "atrium/sockets",
        "atrium", "var/run/atrium", "var/run", "var", "etc/ssl", "etc", "dev", "",
    ] {
        let _ = std::fs::remove_dir(root.join(d));
    }
}

/// At start: every courier jail a previous launcher left is wreckage.
pub fn sweep(cfg: &Config) {
    let Ok(rd) = std::fs::read_dir(JAILS_DIR) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(uid) = n.strip_prefix(NAME_PREFIX).and_then(|u| u.parse::<u32>().ok()) else { continue };
        eprintln!("arca-courierd: reclaiming {n}");
        remove(cfg, uid);
    }
}
