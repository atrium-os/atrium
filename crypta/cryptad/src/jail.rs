//! One jail per live tunnel's engine, made by jaild (`[crypta_engines]`,
//! vpn.md §3), the way arca-courierd makes couriers'.
//!
//! The root is `/var/lib/atrium/jails/system-crypta-<uid>`: the base
//! libraries and the installed binaries read-only, nothing else. Its
//! network is its own vnet whose pf anchor passes only UDP to the gateway's
//! address and port, tagged `CRYPTA_OUTER` (so atrium-net's `atrium/vpn`
//! anchor lets those flows, and only those, out to the gateway). Its stdio
//! is cryptad's socket (and its log); the tun comes as fd 3.

use std::net::SocketAddrV4;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use jaild::protocol::{CreateJailRequest, ExecSpec, MountKind, MountSpec, NetDest, NetGrants, NetworkConfig, Request, Response};

pub const JAILS_DIR: &str = "/var/lib/atrium/jails";
/// jaild's `[crypta_engines]`: these must match its policy.
pub const NAME_PREFIX: &str = "system-crypta-";
pub const EXEC_PATH: &str = "/usr/local/bin/atrium-crypta-wg";
/// The AnyConnect engine (V4): TCP to its gateway, the system's CA file
/// read before its sandbox, its secrets helper at fd 4.
pub const EXEC_AC: &str = "/usr/local/bin/atrium-crypta-ac";
pub const UID_FIRST: u32 = 61000;
pub const UID_LAST: u32 = 61499;
/// The app devfs ruleset: null, zero, random and little else. The engine
/// has no device of its own; the tun is a descriptor.
const DEVFS_RULESET: u32 = 22;
const TAG: &str = "CRYPTA_OUTER";

pub fn name(uid: u32) -> String {
    format!("{NAME_PREFIX}{uid}")
}

pub fn root(uid: u32) -> PathBuf {
    Path::new(JAILS_DIR).join(name(uid))
}

pub struct Engine {
    pub uid: u32,
    /// cryptad's end of the engine's socket.
    pub sock: std::os::unix::net::UnixStream,
    pub procdesc: OwnedFd,
}

/// Start an engine for a tunnel to `gw` on `tun`: WireGuard (UDP), or with
/// `secrets` AnyConnect (TCP, the secrets helper's socket at fd 4). On
/// failure the jail is released before the error returns; its root is the
/// janitor's.
pub fn launch(jaild: &str, uid: u32, gw: SocketAddrV4, tun: BorrowedFd<'_>, secrets: Option<BorrowedFd<'_>>, gp: bool) -> Result<Engine, String> {
    let ac = secrets.is_some();
    if !try_teardown(uid) {
        return Err(format!("{} is still being torn down", name(uid)));
    }
    let root = root(uid);
    std::fs::create_dir_all(&root).map_err(|e| format!("root: {e}"))?;
    let ro = |p: &str| MountSpec { noexec: false, source: p.into(), dest: p.into(), kind: MountKind::RoNullfs, size_mb: None, owner: None };
    let mut mounts = vec![ro("/lib"), ro("/libexec"), ro("/usr/lib"), ro("/usr/local/bin")];
    if ac {
        // The system's CA file (certctl's /etc/ssl/cert.pem), read once.
        mounts.push(ro("/etc/ssl"));
    }
    let id = portcullis_identity::for_app(&format!("org.atrium.crypta.engine.{uid}")).map_err(|e| format!("host identity: {e}"))?;
    let cidr = format!("{}/32", gw.ip());
    let mut outbound = vec![NetDest { cidr: cidr.clone(), port: Some(gw.port()), udp: !ac }];
    if gp {
        outbound.push(NetDest { cidr, port: Some(net_proto::tunnel::GLOBALPROTECT_ESP_PORT), udp: true });
    }
    let grants = NetGrants {
        app_key: portcullis_identity::consent_key(&name(uid)),
        outbound_any: false,
        outbound,
        peers: vec![],
        inbound: vec![],
        listen: vec![],
        no_resolver: true,
        tag: Some(TAG.into()),
    };
    let req = Request::CreateJail(CreateJailRequest {
        class: None,
        devices: vec![],
        name: name(uid),
        path: root.to_string_lossy().into_owned(),
        children_max: 0,
        mounts,
        devfs_ruleset: DEVFS_RULESET,
        network: NetworkConfig::Routed { mac: id.mac.clone(), grants: Some(grants), fib: None },
        exec: Some(ExecSpec { cwd: None,
            groups: vec![],
            path: if ac { EXEC_AC } else { EXEC_PATH }.into(),
            argv: vec![if ac { EXEC_AC } else { EXEC_PATH }.into()],
            env: vec![],
            uid,
            gid: uid,
            stdio: true,
            listen: Vec::new(),
            ready: false,
        }),
        rctl: None,
        hostname: Some(name(uid)),
        hostid: Some(id.hostid),
        hostuuid: Some(id.hostuuid.clone()),
        allow_mlock: false,
        allow_tessera_keys: false,
        syscall_profile: None,
        devfs_grant: None,
        devfs_grants: vec![],
        fuse: None,
    });
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    let mut c = jaild::client::Client::connect(jaild).map_err(|e| format!("connect {jaild}: {e}"))?;
    let err = std::io::stderr();
    // stdin and stdout are the socket, stderr our log; the tun lands at fd
    // 3, an AnyConnect engine's secrets helper at 4.
    let mut fds = vec![theirs.as_fd(), theirs.as_fd(), err.as_fd(), tun];
    fds.extend(secrets);
    let sent = c.send_with_fds(&req, &fds, 1);
    drop(theirs);
    let (resp, got) = sent.map_err(|e| format!("jaild: {e}"))?;
    // SAFETY: jaild handed these over; each is owned once.
    let mut got: Vec<OwnedFd> = got.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect();
    let r = match resp {
        Response::JailCreated(r) if r.procdesc_attached => match got.pop() {
            Some(pd) => Ok(Engine { uid, sock: ours, procdesc: pd }),
            None => Err("jaild sent no procdesc".into()),
        },
        Response::PolicyDenied { rule, detail } => Err(format!("jaild refused ({rule}): {detail}")),
        Response::SyscallFailed { name, errno, msg } => Err(format!("jaild: {name} (errno {errno}): {msg}")),
        other => Err(format!("jaild: unexpected {other:?}")),
    };
    if r.is_err() {
        release(jaild, uid);
    }
    r
}

/// Drop jaild's record of the jail, with its network.
pub fn release(jaild: &str, uid: u32) {
    if let Ok(mut c) = jaild::client::Client::connect(jaild) {
        let _ = c.send(&Request::RemoveJail { jid: None, name: Some(name(uid)) });
    }
}

fn jail_exists(name: &str) -> bool {
    std::process::Command::new("jls")
        .args(["-d", "-j", name, "jid"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One attempt to take `uid`'s jail apart; true once nothing of it is
/// left. Never waits: a dying jail or a pinned mount means "later".
pub fn try_teardown(uid: u32) -> bool {
    let (name, root) = (name(uid), root(uid));
    let _ = std::process::Command::new("jail").arg("-r").arg(&name).stderr(std::process::Stdio::null()).status();
    if jail_exists(&name) {
        return false;
    }
    if !root.exists() {
        return true;
    }
    if !portcullis_mounts::converge(&[root.as_path()], portcullis_mounts::Force::Yes).is_empty() {
        return false;
    }
    // ★ Never remove_dir_all: only empty directories go.
    for d in ["lib", "libexec", "usr/lib", "usr/local/bin", "usr/local", "usr", "etc/ssl", "etc", "dev", ""] {
        let _ = std::fs::remove_dir(root.join(d));
    }
    !root.exists()
}

/// Engine jails a previous cryptad left: their uids, for the janitor.
pub fn sweep(jaild: &str) -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir(JAILS_DIR) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(uid) = n.strip_prefix(NAME_PREFIX).and_then(|u| u.parse::<u32>().ok()) else { continue };
        if n != name(uid) || !(UID_FIRST..=UID_LAST).contains(&uid) {
            continue;
        }
        eprintln!("cryptad: reclaiming {n}");
        release(jaild, uid);
        out.push(uid);
    }
    out
}
