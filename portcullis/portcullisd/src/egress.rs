//! A `registries` workspace's way out (dev-tools.md §5.2, dev-tools-plan.md
//! M8): `nuntius-egress`, a CONNECT-only proxy for the hosts the workspace
//! lists, in a one-shot jail of its own with the network, one per running
//! workspace.
//!
//! The workspace's jail has only its own loopback. Before anything runs in
//! it, portcullisd binds 127.0.0.1:3128 **in the workspace's network stack**
//! (a helper process attached to the jail makes the socket and hands it
//! back; a socket keeps the stack it was made in) and gives that listening
//! socket to the proxy as its stdin. So the proxy accepts what the workspace
//! connects to on its loopback and dials out from its own stack; nothing in
//! the workspace can take the port first or reach the proxy's network.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// The proxy's bundle (system-installed, like Nuntius's fetcher).
pub const EGRESS_APP: &str = "org.atrium.nuntius-egress";
/// Where the workspace reaches it.
pub const PROXY_PORT: u16 = 3128;
/// The helper's argument: portcullisd re-run to bind in a jail.
pub const BIND_ARG: &str = "--bind-in-jail";

/// The environment a `registries` workspace gets.
pub fn proxy_env() -> Vec<String> {
    let u = format!("http://127.0.0.1:{PROXY_PORT}");
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY"]
        .iter().map(|k| format!("{k}={u}")).chain(std::iter::once("NO_PROXY=".to_string())).collect()
}

/// The proxy's jail instance for a workspace's jail name.
fn instance(ws_jail: &str) -> String {
    // jaild's 64-byte limit: the workspace jail's name, hashed short.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ws_jail.hash(&mut h);
    format!("w{:016x}", h.finish())
}

fn jail_name(ws_jail: &str) -> Option<String> {
    portcullis_jail::jaild_instance_name(EGRESS_APP, Some(&instance(ws_jail)))
}

/// The helper's half (`portcullisd --bind-in-jail <jid>`, stdout a Unix
/// socket): attach to the jail, bind 127.0.0.1:3128 there, listen, send
/// the socket back, exit. Runs in a fresh process, never in the daemon.
pub fn bind_in_jail_main(jid: &str) -> ! {
    let fail = |w: &str| -> ! {
        eprintln!("portcullisd {BIND_ARG}: {w}: {}", std::io::Error::last_os_error());
        std::process::exit(1)
    };
    let Ok(jid) = jid.parse::<i32>() else { fail("not a jid") };
    // SAFETY: plain syscalls on values we made.
    unsafe {
        if libc::jail_attach(jid) != 0 { fail("jail_attach") }
        let s = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 { fail("socket") }
        let one: libc::c_int = 1;
        libc::setsockopt(s, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&one as *const libc::c_int).cast(), 4);
        let mut sin: libc::sockaddr_in = std::mem::zeroed();
        sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = PROXY_PORT.to_be();
        sin.sin_addr.s_addr = u32::from_be_bytes([127, 0, 0, 1]).to_be();
        if libc::bind(s, (&sin as *const libc::sockaddr_in).cast(), sin.sin_len as libc::socklen_t) != 0 { fail("bind 127.0.0.1:3128") }
        if libc::listen(s, 64) != 0 { fail("listen") }
        let out = UnixStream::from_raw_fd(1);
        if portcullis_ipc::send_fds(&out, &[s]).is_err() { fail("send") }
    }
    std::process::exit(0)
}

/// Bind the proxy's port in jail `jid`'s network stack.
fn bind_in(jid: i32) -> Result<OwnedFd, String> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let st = std::process::Command::new(exe).arg(BIND_ARG).arg(jid.to_string())
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::from(OwnedFd::from(theirs)))
        .status().map_err(|e| format!("helper: {e}"))?;
    if !st.success() {
        return Err(format!("the helper could not bind 127.0.0.1:{PROXY_PORT} in the workspace ({st})"));
    }
    let mut fds = portcullis_ipc::recv_fds(&ours, 1).map_err(|e| format!("helper's socket: {e}"))?;
    fds.pop().ok_or_else(|| "the helper sent no socket".to_string())
}

/// Start the proxy for the workspace in jail `jid` (`ws_jail` its name),
/// for `hosts`. It runs until [`stop`].
pub fn start(ws_jail: &str, jid: i32, hosts: &[String]) -> Result<(), String> {
    let rule = portcullis_policy::Policy::load_system(std::path::Path::new(portcullis_policy::SYSTEM_PATH)).ok()
        .and_then(|p| p.workers.get(EGRESS_APP).cloned())
        .ok_or_else(|| format!("no system worker rule for {EGRESS_APP}: the proxy never runs as the workspace or as root"))?;
    let listener = bind_in(jid)?;
    let err = std::os::fd::AsFd::as_fd(&std::io::stderr()).try_clone_to_owned().map_err(|e| e.to_string())?;
    let err2 = err.try_clone().map_err(|e| e.to_string())?;
    stop(ws_jail);
    let spec = portcullis_oneshot::Spec {
        target: EGRESS_APP.into(),
        instance: Some(instance(ws_jail)),
        tmpfs_mb: 16,
        memory_mb: None,
        cpu_pct: None,
        require_memory_limit: false,
        user_name: rule.run_as.clone(),
        extra_env: vec![("ATRIUM_EGRESS_HOSTS".into(), hosts.join(","))],
        worker: None,
    };
    let name = ws_jail.to_string();
    std::thread::Builder::new().name("portcullisd-egress".into()).spawn(move || {
        let r = portcullis_oneshot::run_with_stdio(&spec, Some([listener, err, err2]));
        eprintln!("portcullisd: egress for {name} ended: {}", match r {
            portcullis_oneshot::OneShot::Exit { code, .. } => format!("exit {code:?}"),
            portcullis_oneshot::OneShot::Refused(w) => format!("refused: {w}"),
            portcullis_oneshot::OneShot::Failed(w) => format!("failed: {w}"),
        });
    }).map_err(|e| e.to_string())?;
    eprintln!("portcullisd: egress for {ws_jail}: {} hosts, as {}", hosts.len(), rule.run_as);
    Ok(())
}

/// End the workspace's proxy, if it has one.
pub fn stop(ws_jail: &str) {
    let Some(name) = jail_name(ws_jail) else { return };
    let quiet = || std::process::Stdio::null();
    let _ = std::process::Command::new("pkill").args(["-9", "-j", &name]).stdout(quiet()).stderr(quiet()).status();
    let _ = std::process::Command::new("jail").arg("-r").arg(&name).stdout(quiet()).stderr(quiet()).status();
}
