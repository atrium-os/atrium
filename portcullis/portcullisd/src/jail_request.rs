//! The typed jaild request for a jail portcullisd describes as a
//! [`JailConfig`] (app-jails-via-jaild.md §4). Every jail.conf parameter the
//! config carries either maps to a field jaild validates, or the whole
//! request is refused: nothing is dropped, and no parameter text reaches a
//! shell.
//!
//! What the config cannot say comes from the caller in [`RequestParts`]:
//! the class, who the entry runs as, the `full` network's resolved grants,
//! and the system call class (jaild resolves it from its own policy).

use std::collections::BTreeSet;
use std::path::Path;

use jaild::protocol::{
    CreateJailRequest, DeviceClass, EnvPair, ExecSpec, JailClass, MountKind, MountSpec,
    NetGrants, NetworkConfig,
};
use portcullis_jail::{JailConfig, Value};

/// Who the entry runs as, and how. `None` in [`RequestParts::run`] makes a
/// jail that persists with nothing in it (a workspace, a console).
#[derive(Debug, Clone)]
pub struct RunAs {
    pub uid:    u32,
    pub gid:    u32,
    /// Supplementary groups past `gid`: the owner's files groups.
    pub groups: Vec<u32>,
    /// Arguments after the entry's own name.
    pub args:   Vec<String>,
    pub env:    Vec<EnvPair>,
    /// The caller's stdin, stdout and stderr ride on the request.
    pub stdio:  bool,
    /// Where the entry starts, inside the jail (an app's: its account's
    /// home). `None`: the root.
    pub cwd:    Option<String>,
}

#[derive(Debug, Clone)]
pub struct RequestParts {
    pub class: JailClass,
    pub run:   Option<RunAs>,
    /// The `full` network's grants, hostnames already resolved
    /// (`portcullis_oneshot::net_grants`). `None`: outbound anywhere.
    pub grants: Option<NetGrants>,
    /// The system call class named for this jail; required when the config
    /// carries a filter, which jaild resolves again from its own policy.
    pub syscall_profile: Option<String>,
}

/// The device class each capability's devfs action stands for. jaild owns
/// the node patterns; a line not listed here is refused.
const DEVICE_ACTIONS: &[(&str, DeviceClass)] = &[
    ("path 'fresco0' unhide", DeviceClass::Fresco),
    ("path 'input/event*' unhide", DeviceClass::Input),
    ("path 'video*' unhide", DeviceClass::Camera),
];

fn refuse<T>(what: String) -> Result<T, String> {
    Err(what)
}

fn want_bool(key: &str, v: &Value, want: bool) -> Result<(), String> {
    match v {
        Value::Bool(b) if *b == want => Ok(()),
        other => refuse(format!("{key} = {other:?}: jaild sets it to {want} for every classed jail")),
    }
}

/// The single absolute path an `exec.start` names. Arguments come from
/// [`RunAs::args`], never from words in the string, which jail(8) would
/// hand to a shell.
fn entry_path(s: &str) -> Result<String, String> {
    let ok = s.starts_with('/')
        && s.len() <= 1024
        && !s.split('/').any(|c| c == "..")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b));
    if ok { Ok(s.to_string()) } else { refuse(format!("exec.start {s:?} is not one plain absolute path")) }
}

/// The exec.created string the `loopback` capability renders for jail `n`.
fn loopback_created(n: &str) -> String {
    format!("ifconfig -j {n} lo0 inet 127.0.0.1/8 up && ifconfig -j {n} lo0 inet6 ::1/128")
}

/// The exec.prestart string `portcullis_jail::build` renders for the
/// config's devfs actions.
fn devfs_prestart(jc: &JailConfig) -> String {
    let dev = jc.root_path.join("dev");
    jc.devfs_actions.iter()
        .map(|a| format!("devfs -m {} rule apply {}", dev.display(), a.line))
        .collect::<Vec<_>>()
        .join(" && ")
}

fn mount(jc: &JailConfig, m: &portcullis_jail::MountSpec, app: bool) -> Result<MountSpec, String> {
    let at = || format!("{} -> {}", m.src.display(), m.dst.display());
    if m.fstype != "nullfs" {
        return refuse(format!("mount {}: fstype {} (only nullfs maps)", at(), m.fstype));
    }
    let opts: BTreeSet<&str> = m.opts.iter().map(String::as_str).collect();
    if opts.len() != m.opts.len() {
        return refuse(format!("mount {}: an option named twice", at()));
    }
    let kind = match (opts.contains("ro"), opts.contains("rw")) {
        (true, false) => MountKind::RoNullfs,
        (false, true) => MountKind::RwNullfs,
        _ => return refuse(format!("mount {}: exactly one of ro and rw", at())),
    };
    let noexec = opts.contains("noexec");
    /* nosuid is how jaild mounts every nullfs into an app jail; anywhere
     * else it has no field to map to. */
    if opts.contains("nosuid") && !app {
        return refuse(format!("mount {}: nosuid outside an app jail", at()));
    }
    if let Some(o) = opts.iter().find(|o| !["ro", "rw", "noexec", "nosuid"].contains(o)) {
        return refuse(format!("mount {}: option {o}", at()));
    }
    let Ok(rel) = m.dst.strip_prefix(&jc.root_path) else {
        return refuse(format!("mount {}: destination outside the root {}", at(), jc.root_path.display()));
    };
    let (Some(source), Some(dest)) = (m.src.to_str(), rel.to_str()) else {
        return refuse(format!("mount {}: not UTF-8", at()));
    };
    if dest.is_empty() || !m.src.is_absolute() {
        return refuse(format!("mount {}: over the root itself, or from a relative source", at()));
    }
    Ok(MountSpec { source: source.into(), dest: dest.into(), kind, size_mb: None, owner: None, noexec })
}

/// The jaild request for `jc`, or why it has none.
pub fn to_create_request(jc: &JailConfig, parts: RequestParts) -> Result<CreateJailRequest, String> {
    let app = matches!(parts.class, JailClass::App { .. });
    let may_debug = matches!(parts.class, JailClass::Develop { .. } | JailClass::App { debug: true, .. });

    let (mut hostname, mut hostid, mut hostuuid) = (None, None, None);
    let (mut devfs, mut devfs_ruleset) = (false, 0u32);
    let (mut entry, mut prestart, mut created, mut vnet) = (None, None, None, false);
    let mut sysfil = false;
    for (key, v) in &jc.params {
        match (key.as_str(), v) {
            ("host.hostname", Value::String(s)) => hostname = Some(s.clone()),
            ("host.hostid", Value::Number(n)) => {
                hostid = Some(u32::try_from(*n).map_err(|_| format!("host.hostid {n} is not a u32"))?)
            }
            ("host.hostuuid", Value::String(s)) => hostuuid = Some(s.clone()),
            /* jaild decides: an exec'd jail ends with its entry, an empty
             * one persists. */
            ("persist", Value::Bool(_)) => {}
            ("mount.devfs", Value::Bool(b)) => devfs = *b,
            ("devfs_ruleset", Value::Number(n)) => {
                devfs_ruleset = u32::try_from(*n).map_err(|_| format!("devfs_ruleset {n} is not a u32"))?
            }
            /* jaild always gives the entry exactly the environment asked for,
             * and takes the identity as numbers from RunAs. */
            ("exec.clean", v) => want_bool(key, v, true)?,
            ("exec.system_jail_user", v) => want_bool(key, v, true)?,
            ("exec.jail_user", Value::String(_)) => {}
            ("exec.start", Value::String(s)) => entry = Some(s.clone()),
            ("exec.prestart", Value::String(s)) => prestart = Some(s.clone()),
            ("exec.created", Value::String(s)) => created = Some(s.clone()),
            ("vnet", Value::Symbolic(s)) if s == "new" => vnet = true,
            ("allow.raw_sockets" | "allow.mount" | "allow.chflags" | "allow.set_hostname", v) => {
                want_bool(key, v, false)?
            }
            ("allow.unprivileged_proc_debug", v) => want_bool(key, v, may_debug)?,
            ("enforce_statfs", Value::Number(2)) => {}
            ("children.max", Value::Number(0)) => {}
            ("sysfil.mode" | "sysfil.syscalls" | "sysfil.ioctls" | "sysfil.sockopts" | "sysfil.families", _) => {
                sysfil = true
            }
            (k, v) => return refuse(format!("{k} = {v:?} has no jaild counterpart")),
        }
    }
    if sysfil && parts.syscall_profile.is_none() {
        return refuse("the config filters system calls and no class names the filter".into());
    }

    /* Devices: each action is one of jaild's classes, and the prestart that
     * would have applied them is exactly the one build() renders. */
    let mut devices = Vec::new();
    for a in &jc.devfs_actions {
        match DEVICE_ACTIONS.iter().find(|(line, _)| *line == a.line) {
            Some((_, d)) if !devices.contains(d) => devices.push(*d),
            Some(_) => {}
            None => return refuse(format!("devfs action {:?} is no device class", a.line)),
        }
    }
    match prestart {
        None if devices.is_empty() => {}
        Some(p) if !devices.is_empty() && p == devfs_prestart(jc) => {}
        Some(p) => return refuse(format!("exec.prestart {p:?} is not the device grants")),
        None => return refuse("device grants with no prestart to say how".into()),
    }
    if !devfs {
        devfs_ruleset = 0;
        if !devices.is_empty() {
            return refuse("device grants with no devfs".into());
        }
    }

    /* Network. */
    if jc.route_fib.is_some() && jc.needs_routed_net.is_none() {
        return refuse("a route domain without a routed network".into());
    }
    if parts.grants.is_some() && jc.needs_routed_net.is_none() {
        return refuse("network grants without a routed network".into());
    }
    let network = match (&jc.needs_routed_net, created) {
        (Some(mac), None) if vnet => NetworkConfig::Routed { mac: mac.clone(), grants: parts.grants, fib: jc.route_fib },
        (None, Some(c)) if vnet => {
            let n = c.strip_prefix("ifconfig -j ").and_then(|r| r.split(' ').next()).unwrap_or("");
            if c != loopback_created(n) {
                return refuse(format!("exec.created {c:?} is not the loopback capability's"));
            }
            NetworkConfig::Loopback
        }
        (None, None) if vnet => NetworkConfig::Isolated,
        (None, None) => NetworkConfig::Disable,
        (_, Some(c)) => return refuse(format!("exec.created {c:?} has no jaild counterpart")),
        (Some(_), None) => return refuse("a routed network without vnet".into()),
    };

    let mounts = jc.mounts.iter().map(|m| mount(jc, m, app)).collect::<Result<Vec<_>, _>>()?;

    let exec = match parts.run {
        Some(r) => {
            let Some(e) = entry else { return refuse("an exec with no exec.start".into()) };
            let path = entry_path(&e)?;
            let name = Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or(&path).to_string();
            let mut argv = vec![name];
            argv.extend(r.args);
            Some(ExecSpec { cwd: r.cwd,
                path, argv, env: r.env, uid: r.uid, gid: r.gid, groups: r.groups,
                stdio: r.stdio, listen: Vec::new(), ready: false,
            })
        }
        /* An empty jail: jaild runs nothing in it. */
        None => None,
    };

    let Some(path) = jc.root_path.to_str() else {
        return refuse(format!("root {} is not UTF-8", jc.root_path.display()));
    };
    Ok(CreateJailRequest {
        name: jc.name.clone(),
        path: path.into(),
        children_max: 0,
        mounts,
        devfs_ruleset,
        network,
        exec,
        hostname,
        hostid,
        hostuuid,
        allow_mlock: false,
        allow_tessera_keys: false,
        syscall_profile: parts.syscall_profile,
        devfs_grant: None,
        devfs_grants: vec![],
        fuse: None,
        rctl: None,
        class: Some(parts.class),
        devices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Every app manifest in the tree.
    fn manifests() -> Vec<(PathBuf, portcullis_toml::Manifest)> {
        let mut out = Vec::new();
        let mut dirs = vec![repo()];
        while let Some(d) = dirs.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name();
                let name = name.to_string_lossy();
                if p.is_dir() && !name.starts_with('.') && !["target", "freebsd-src", "sysroot", "node_modules", "scratch"].contains(&name.as_ref()) {
                    dirs.push(p);
                } else if name == "atrium.toml" {
                    let text = std::fs::read_to_string(&p).expect("read manifest");
                    if let Ok((m, _)) = portcullis_toml::parse_and_validate(&text) {
                        out.push((p, m));
                    }
                }
            }
        }
        out
    }

    fn opts(name: &str) -> portcullis_jail::BuildOpts {
        portcullis_jail::BuildOpts {
            root_path: PathBuf::from(format!("/var/lib/atrium/jails/{name}")),
            host_sockets: PathBuf::from("/atrium/sockets"),
            user_home: PathBuf::from("/usr/home/alice"),
            user_name: "app2001".into(),
            devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
            instance: None,
            persist: true,
            host_identity: portcullis_identity::derive(&[7; 32], "x"),
            sysfil: None,
        }
    }

    fn app_parts(app_id: &str) -> RequestParts {
        RequestParts {
            class: JailClass::App { owner: "alice".into(), app_id: app_id.into(), debug: false },
            run: Some(RunAs { uid: 2001, gid: 2001, groups: vec![], args: vec![], env: vec![], stdio: true, cwd: None }),
            grants: None,
            syscall_profile: None,
        }
    }

    /// Every shipped manifest maps, each jail.conf part has its typed
    /// counterpart, and jaild's own validator accepts the result.
    #[test]
    fn every_shipped_manifest_maps_and_passes_jaild() {
        let policy = jaild_policy::Policy::load(repo().join("etc/jaild.policy.toml")).expect("policy");
        let all = manifests();
        assert!(all.len() >= 15, "only {} manifests found", all.len());
        let mut refused = Vec::new();
        for (at, m) in &all {
            // The name and root portcullisd gives an app's jail.
            let jc = portcullis_jail::build(m, &opts(&m.app.id))
                .unwrap_or_else(|e| panic!("{}: build: {e}", at.display()));
            let req = to_create_request(&jc, app_parts(&m.app.id))
                .unwrap_or_else(|e| panic!("{}: {e}", at.display()));
            assert_eq!(req.mounts.len(), jc.mounts.len(), "{}", at.display());
            assert_eq!(req.devices.len(), jc.devfs_actions.len(), "{}", at.display());
            assert_eq!(req.hostname.as_deref(), Some(m.app.id.as_str()));
            assert_eq!(req.devfs_ruleset, portcullis_jail::APP_DEVFS_RULESET);
            let exec = req.exec.as_ref().expect("an exec");
            assert_eq!(exec.path, format!("/{}", m.entry()), "{}", at.display());
            match (&m.capabilities.network, &req.network) {
                (Some(s), NetworkConfig::Routed { .. }) if s.mode() == portcullis_toml::NetworkCap::Full => {}
                (Some(s), NetworkConfig::Loopback) if s.mode() == portcullis_toml::NetworkCap::Loopback => {}
                (_, NetworkConfig::Isolated) => {}
                (n, got) => panic!("{}: network {n:?} became {got:?}", at.display()),
            }
            if let Err(e) = jaild::validator::validate_create(&req, &policy) {
                refused.push(format!("{}: {e}", m.app.id));
            }
        }
        assert!(refused.is_empty(), "jaild refuses:\n{}", refused.join("\n"));
    }

    fn sheets() -> JailConfig {
        let (_, m) = manifests().into_iter()
            .find(|(p, _)| p.ends_with("atrium-sheets/atrium.toml")).expect("the Sheets manifest");
        portcullis_jail::build(&m, &opts("org.atrium.sheets")).expect("build")
    }

    #[test]
    fn what_has_no_counterpart_is_refused() {
        let parts = || app_parts("org.atrium.sheets");
        assert!(to_create_request(&sheets(), parts()).is_ok());

        let mut jc = sheets();
        jc.set("exec.prestart", Value::String("touch /tmp/pwned".into()));
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("exec.prestart"));

        let mut jc = sheets();
        jc.set("exec.created", Value::String("sh -c id".into()));
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("exec.created"));

        let mut jc = sheets();
        jc.set("exec.poststop", Value::String("true".into()));
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("no jaild counterpart"));

        let mut jc = sheets();
        jc.set("exec.start", Value::String("/bin/sh -c id".into()));
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("exec.start"));

        let mut jc = sheets();
        jc.set("exec.clean", Value::Bool(false));
        assert!(to_create_request(&jc, parts()).is_err());

        let mut jc = sheets();
        jc.set("allow.unprivileged_proc_debug", Value::Bool(true));
        assert!(to_create_request(&jc, parts()).is_err());
        let mut p = parts();
        p.class = JailClass::App { owner: "alice".into(), app_id: "org.atrium.sheets".into(), debug: true };
        assert!(to_create_request(&jc, p).is_ok());

        let mut jc = sheets();
        jc.set("allow.mount", Value::Bool(true));
        assert!(to_create_request(&jc, parts()).is_err());
        let mut jc = sheets();
        jc.set("enforce_statfs", Value::Number(1));
        assert!(to_create_request(&jc, parts()).is_err());
        let mut jc = sheets();
        jc.set("children.max", Value::Number(4));
        assert!(to_create_request(&jc, parts()).is_err());

        let mut jc = sheets();
        jc.add_devfs_action("path 'bpf*' unhide");
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("devfs action"));

        let mut jc = sheets();
        jc.set("sysfil.mode", Value::Number(2));
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("filter"));

        let mut jc = sheets();
        jc.add_mount(Path::new("/etc"), Path::new("/etc"), "nullfs", &["ro"]);
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("outside the root"));
        let mut jc = sheets();
        let root = jc.root_path.clone();
        jc.add_mount(Path::new("/usr/local/lib"), &root.join("x"), "nullfs", &["ro", "suiddir"]);
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("option"));
        let mut jc = sheets();
        jc.add_mount(Path::new("/usr/local/lib"), &root.join("x"), "nullfs", &["ro", "rw"]);
        assert!(to_create_request(&jc, parts()).is_err());
        let mut jc = sheets();
        jc.add_mount(Path::new("/dev"), &root.join("x"), "devfs", &["rw"]);
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("fstype"));
        let mut jc = sheets();
        jc.add_mount(Path::new("/usr/local/lib"), &root.join("x"), "nullfs", &["ro", "noexec"]);
        let r = to_create_request(&jc, parts()).unwrap();
        assert!(r.mounts.last().unwrap().noexec);

        let mut jc = sheets();
        jc.route_fib = Some(3);
        assert!(to_create_request(&jc, parts()).unwrap_err().contains("route domain"));

        // An exec needs an entry; an empty jail runs nothing.
        let mut jc = sheets();
        jc.params.retain(|(k, _)| k != "exec.start");
        assert!(to_create_request(&jc, parts()).is_err());
        let mut p = parts();
        p.class = JailClass::Terminal { owner: "alice".into() };
        p.run = None;
        assert!(to_create_request(&jc, p).unwrap().exec.is_none());
    }

    #[test]
    fn arguments_come_from_the_caller_not_the_string() {
        let mut p = app_parts("org.atrium.sheets");
        p.run.as_mut().unwrap().args = vec!["--persona".into(), "work; rm -rf /".into()];
        let r = to_create_request(&sheets(), p).unwrap();
        let e = r.exec.unwrap();
        assert_eq!(e.argv[1..], ["--persona".to_string(), "work; rm -rf /".to_string()]);
        assert_eq!(e.argv[0], Path::new(&e.path).file_name().unwrap().to_str().unwrap());
    }

    #[test]
    fn the_loopback_and_device_strings_map_to_their_kinds() {
        let mut jc = sheets();
        let n = jc.name.clone();
        jc.set("vnet", Value::Symbolic("new".into()));
        jc.set("exec.created", Value::String(loopback_created(&n)));
        assert!(matches!(to_create_request(&jc, app_parts("x")).unwrap().network, NetworkConfig::Loopback));
        jc.set("exec.created", Value::String(loopback_created(&n) + " && id"));
        assert!(to_create_request(&jc, app_parts("x")).is_err());

        let mut jc = sheets();
        jc.devfs_actions.clear();
        jc.params.retain(|(k, _)| k != "exec.prestart");
        jc.add_devfs_action("path 'video*' unhide");
        jc.add_devfs_action("path 'input/event*' unhide");
        // the action without the prestart build renders for it
        assert!(to_create_request(&jc, app_parts("x")).is_err());
        jc.set("exec.prestart", Value::String(devfs_prestart(&jc)));
        assert_eq!(to_create_request(&jc, app_parts("x")).unwrap().devices, [DeviceClass::Camera, DeviceClass::Input]);
        jc.set("mount.devfs", Value::Bool(false));
        assert!(to_create_request(&jc, app_parts("x")).is_err());
    }

    /// Every capability socket portcullis-jail can mount into an app is one
    /// jaild's policy lets an app jail have.
    #[test]
    fn every_capability_socket_is_in_jailds_app_list() {
        let policy = jaild_policy::Policy::load(repo().join("etc/jaild.policy.toml")).expect("policy");
        let src = repo().join("portcullis/portcullis-jail/src");
        let mut names = Vec::new();
        for f in ["capabilities.rs", "lib.rs"] {
            let text = std::fs::read_to_string(src.join(f)).expect("source");
            for part in text.split("apply_socket(\"").skip(1) {
                let n = part.split('"').next().unwrap_or("");
                names.push(n.strip_suffix(".sock").unwrap_or(n).to_string());
            }
            // The Navigator broker's array, mounted in a loop.
            if let Some(arr) = text.split("for s in [").nth(1).and_then(|r| r.split(']').next()) {
                names.extend(arr.split(',').map(|s| s.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty()));
            }
        }
        assert!(names.len() > 30, "found only {} socket names", names.len());
        let missing: Vec<_> = names.iter().filter(|n| !n.is_empty() && !policy.apps.sockets.contains(n)).collect();
        assert!(missing.is_empty(), "not in jaild.policy.toml [apps] sockets: {missing:?}");
    }
}
