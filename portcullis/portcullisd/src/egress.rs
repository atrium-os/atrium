//! A `registries` workspace's way out (dev-tools.md §5.2, dev-tools-plan.md
//! M8): `nuntius-egress`, a CONNECT-only proxy for the hosts the workspace
//! lists, in a one-shot jail of its own with the network, one per running
//! workspace.
//!
//! The workspace's jail has only its own loopback. Before anything runs in
//! it, portcullisd has jaild bind 127.0.0.1:3128 **in the workspace's network
//! stack** (a socket keeps the stack it was made in) and gives that listening
//! socket to the proxy as its stdin. So the proxy accepts what the workspace
//! connects to on its loopback and dials out from its own stack; nothing in
//! the workspace can take the port first or reach the proxy's network.

use std::os::fd::OwnedFd;

/// The proxy's bundle (system-installed, like Nuntius's fetcher).
pub const EGRESS_APP: &str = "org.atrium.nuntius-egress";
/// Where the workspace reaches it.
pub const PROXY_PORT: u16 = 3128;

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

/// A socket listening on 127.0.0.1:`port` (0: any) in workspace jail
/// `ws_jail`'s network stack, made by jaild: the proxy's here, a debugger's
/// way in for M10.
pub fn bind_in(ws_jail: &str, port: u16) -> Result<OwnedFd, String> {
    portcullisd::jaild_jail::listen_in(ws_jail, port)
}

/// Start the proxy for the workspace in jail `ws_jail`, for `hosts`. It
/// runs until [`stop`].
pub fn start(ws_jail: &str, hosts: &[String], fib: Option<u32>) -> Result<(), String> {
    let rule = portcullis_policy::Policy::load_system(std::path::Path::new(portcullis_policy::SYSTEM_PATH)).ok()
        .and_then(|p| p.workers.get(EGRESS_APP).cloned())
        .ok_or_else(|| format!("no system worker rule for {EGRESS_APP}: the proxy never runs as the workspace or as root"))?;
    let listener = bind_in(ws_jail, PROXY_PORT)?;
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
        pane: None,
        fib,
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
    let _ = portcullisd::jaild_jail::remove(&name);
}
