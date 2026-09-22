//! A one-shot jail wired to the caller's pipes.
//!
//! ★★ A CRATE, NOT A CLI SUBCOMMAND, because `portcullisd` must be able to
//! do exactly this and a daemon with its own copy would drift from the CLI's
//! the way the trust gate's three copies already had. `run()` is the whole
//! operation; the CLI parses arguments into it and the daemon calls it
//! directly after receiving the caller's descriptors.
//!
//! ★★ A JAIL PER UNIT OF WORK, not per application.
//!
//! Everything else in this CLI launches an *application*: one jail per app id,
//! a persistent overlay, single-instance, living as long as the person is
//! using it. A worker pool is the other shape. The Navigator's backend runs one
//! jailed document worker PER DOCUMENT (atrium-navigator-backend.md §2), talks
//! to it over stdin/stdout, and throws it away when the document closes.
//!
//! Three differences from `launch`, each deliberate:
//!
//!   - **Per-instance name and root.** `<id>__<tag>` and
//!     `/var/lib/atrium/jails/<id>__<tag>`. Without this, two workers collide
//!     on the jail name, and `jail -c` on an existing name does not fail — it
//!     RECONFIGURES the running jail, so two documents would quietly end up
//!     inside one.
//!   - **No persistent overlay.** A worker holds nothing worth keeping, so the
//!     writable upper layer is tmpfs, discarded with the jail. That also keeps
//!     the overlay quota/dedup machinery out of a lane that churns jails.
//!   - **Signatures are REQUIRED**, not merely default-checked. A lane that
//!     launches jails continuously turns "unsigned is allowed when trust is
//!     unconfigured" from a one-off into a standing condition, so this path
//!     asks for `Demand::Required` whatever the machine is set to.
//!
//! ★★ jaild creates the jail and runs the entry (portcullis.md §6.5.4): the
//! caller's three descriptors ride on the CreateJail request over SCM_RIGHTS
//! and the child dup2s them onto 0/1/2, then execve's the entry directly — no
//! jail.conf, no jail(8), no intervening /bin/sh. jaild returns a process
//! descriptor, which is how this crate waits for the worker and learns its real
//! exit status. Root callers are refused: a worker never runs as uid 0.

use std::path::{Path, PathBuf};
use std::process::Command;

use portcullis_jail::{build, BuildOpts};

/// What to run, and as which instance.
pub struct Spec {
    /// App id, or a path to an app tree.
    pub target: String,
    /// Distinct per concurrent run; `None` is single-instance, like `launch`.
    pub instance: Option<String>,
    /// Size of the discarded writable layer.
    pub tmpfs_mb: u32,
    /// ★★★ AN OPT-IN STATIC MEMORY CAP — AND `None` IS THE RIGHT DEFAULT.
    ///
    /// `memoryuse` is RSS, and RSS can only be enforced by KILLING: you
    /// cannot cleanly fail a page fault, so rctl offers `sigkill`/`sigterm`
    /// for it and `deny` only for virtual/swap (atrium-memory-pressure.md
    /// §"the per-jail HARD CAP"). There is no soft version of this knob.
    ///
    /// ★ AND ATRIUM ALREADY HAS THE ADAPTIVE ANSWER. `memfed` water-fills RAM
    /// across jails by weight and pushes each one's `memoryuse` cap
    /// dynamically, **never below current RSS**, so an over-budget jail is
    /// frozen rather than killed — and it acts through the jaild broker, not
    /// by shelling rctl wherever it happens to be convenient. A static number
    /// set here does not merely duplicate that: it FIGHTS it, because a cap
    /// this lane pins can kill a worker memfed would have frozen.
    ///
    /// So the steady-state answer for a worker jail is the federation, not a
    /// constant. This stays as a deliberate safety net for a deployment whose
    /// jails the federation cannot see — see the gap in portcullis.md §6.5.2e
    /// — and it is off unless someone asks for it.
    pub memory_mb: Option<u64>,
    /// ★ Refuse to run rather than run UNCAPPED when the kernel cannot
    /// enforce a limit. Off by default so an existing machine keeps working —
    /// the same shape as `require_signatures` — because a deployment that
    /// cares must be able to demand it, and one that does not must not be
    /// broken by a release.
    pub require_memory_limit: bool,
    /// The user the entry runs as — its uid, gid and HOME are RESOLVED from
    /// the host's passwd, never supplied by the caller. ★ Must not be root:
    /// a one-shot worker never runs as uid 0 inside its jail (refused).
    pub user_name: String,
}

/// Whether the kernel can enforce an rctl rule at all.
///
/// ★ `kern.racct.enable` is a LOADER TUNABLE, not a runtime switch: a machine
/// that did not boot with it cannot be given resource limits without a
/// reboot. So this is a fact to report, never something to "turn on" — and a
/// caller that silently proceeded would be running a worker pool it believes
/// is capped and is not.
pub fn racct_enabled() -> bool {
    std::process::Command::new("sysctl").arg("-n").arg("kern.racct.enable")
        .output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

/// Apply a memoryuse cap to a jail. Returns whether one is now in force.
///
/// ★ `sigkill`, matching jaild's own rule (`jaild/src/ffi.rs`): a worker that
/// exceeds its cap is killed, not merely denied an allocation. A denied
/// allocation inside a parser is an error path that hostile input chose, and
/// the broker already survives a worker dying — it is the failure mode the
/// whole design is built around.
/// ★ NOTE THE LAYERING VIOLATION THIS ACCEPTS. Atrium's privsep design has
/// jaild as the single actor for rctl — a jailed governor cannot rctl another
/// jail, so `SetRctl` is brokered. This lane creates its jails with `jail -c`
/// directly, and jaild refuses to set a rule on a jail it did not create
/// ("rctl.unknown_jail"), so brokering is not available here yet. It becomes
/// available when the lane moves to jaild's `CreateJail` (portcullis.md
/// §6.5.4), which is the same change that removes the intervening `/bin/sh`.
/// Recorded rather than quietly shelled out.
fn apply_memory_cap(jail_name: &str, mb: u64) -> Result<(), String> {
    let rule = format!("jail:{jail_name}:memoryuse:sigkill={mb}M");
    let out = std::process::Command::new("rctl").arg("-a").arg(&rule)
        .output().map_err(|e| format!("rctl: {e}"))?;
    if out.status.success() { return Ok(()) }
    Err(format!("rctl -a {rule}: {}", String::from_utf8_lossy(&out.stderr).trim()))
}

/// Why a one-shot run did not happen. `Exit` is the jailed process's own
/// outcome and is not a failure of this crate.
#[derive(Debug)]
pub enum OneShot {
    /// The jailed entry ran; `ok` is whether it exited 0. `code` is its own
    /// exit status (None if it died on a signal) — jaild reaps the worker
    /// itself, so this is no longer jail(8)'s collapse of every failure to 1.
    Exit { ok: bool, code: Option<i32> },
    Refused(String),
    Failed(String),
}

const APPS_DIR: &str = "/var/lib/atrium/apps";
const JAILS_DIR: &str = "/var/lib/atrium/jails";

/// Run `spec` in a one-shot jail whose stdio is this process's own: jaild is
/// handed this process's 0/1/2, so when the caller spawned us with pipes the
/// jailed process reads and writes them directly.
pub fn run(spec: &Spec) -> OneShot { run_with_stdio(spec, None) }

/// As `run`, but give the jailed entry these descriptors instead of this
/// process's.
///
/// ★ THE DAEMON NEEDS THIS AND THE CLI DOES NOT. When `portcullisd` creates
/// the jail, its own stdio is the daemon log — the client's descriptors
/// arrived over SCM_RIGHTS and are what the jailed process must actually
/// speak on. Handing jaild the daemon's own would send a worker's protocol frames to the system
/// log and leave the broker waiting forever.
pub fn run_with_stdio(spec: &Spec, stdio: Option<[std::os::fd::OwnedFd; 3]>) -> OneShot {
    let target = spec.target.as_str();
    let instance = spec.instance.clone();
    let tmpfs_mb = spec.tmpfs_mb;
    let tree = resolve_tree(target);
    let manifest_path = tree.join("atrium.toml");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => return OneShot::Failed(format!("read {}: {e}", manifest_path.display())),
    };

    // ★ Demand::Required — see the module header. This is the one caller in
    // the CLI that asks for more than the machine's configured policy.
    if let Err(e) = portcullis_trust::Trust::load()
        .verify(&tree, &text, portcullis_trust::Demand::Required)
    {
        return OneShot::Refused(e);
    }

    let manifest = match portcullis_toml::Manifest::from_str(&text) {
        Ok(m) => m,
        Err(e) => return OneShot::Failed(format!("{}: {e:?}", manifest_path.display())),
    };

    // ★ The jaild-lane name: jaild-valid by construction, and the last
    // component of the root — jaild trusts a one-shot's entry only when its
    // root is exactly /var/lib/atrium/jails/<name> (policy instance_root_dir).
    let Some(jail_name) = portcullis_jail::jaild_instance_name(
        &manifest.app.id, instance.as_deref()) else {
        return OneShot::Refused(format!(
            "app id {:?} with instance {:?} makes a jail name over jaild's 64-byte limit",
            manifest.app.id, instance));
    };
    let jail_path = PathBuf::from(JAILS_DIR).join(&jail_name);

    // ★★ A WORKER NEVER RUNS AS ROOT IN ITS JAIL (portcullis.md §6.5.4). The
    // entry runs as whoever called, and a root caller used to mean a uid-0
    // worker — exactly what turned the /dev exposure of §9.1b into a read of
    // the host's disk. Refused here with the reason; jaild's uid policy would
    // refuse it anyway, with a less useful message.
    let Some(pw) = passwd_entry(&spec.user_name) else {
        return OneShot::Failed(format!("no passwd entry for {:?}", spec.user_name));
    };
    if pw.uid == 0 {
        return OneShot::Refused(
            "one-shot workers never run as root inside their jail; call from an \
             unprivileged user (portcullis.md §6.5.4)".into());
    }

    // ★ The app's synthetic host identity (portcullis.md §9.1c): per APP, not
    // per instance — every worker of one app is the same "machine" to it, and
    // the tag stays out of what the worker can read about itself.
    let host_identity = match portcullis_identity::for_app(&manifest.app.id) {
        Ok(id) => id,
        Err(e) => return OneShot::Failed(format!("host identity: {e}")),
    };
    let opts = BuildOpts {
        root_path: jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from(&pw.home),
        user_name: spec.user_name.clone(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        instance: instance.clone(),
        // ★ A unit of work, so the jail dies with its processes — see
        // BuildOpts::persist. (jaild's exec path creates with persist=0.)
        persist: false,
        host_identity: host_identity.clone(),
    };
    let jc = match build(&manifest, &opts) {
        Ok(jc) => jc,
        Err(e) => return OneShot::Failed(format!("build: {e}")),
    };
    // ★★ What this lane cannot express is REFUSED, never dropped. jaild's
    // CreateJail carries nullfs mounts and a devfs ruleset, but no per-jail
    // devfs grants; a
    // capability needing more would otherwise run without it and report
    // success.
    let mounts = match jaild_mounts(&jc, &jail_path) {
        Ok(m) => m,
        Err(e) => return OneShot::Refused(e),
    };

    // ★★ A LIVE JAIL WITH THIS NAME IS A REFUSAL, NOT SOMETHING TO CLEAN UP.
    //
    // The pre-teardown below exists for the leftovers of a run that DIED —
    // killed between its mount and its cleanup — because stacking a fresh
    // nullfs on an abandoned pile is how a mount stack becomes unrecoverable
    // without a reboot. It cannot tell that from a jail that is still
    // running, and measured against a live one it killed it: two runs with
    // the same tag ended with the first reader's process taking SIGTERM and
    // the second failing anyway. Both lost, for a reason neither could act on.
    //
    // So liveness is checked first, and a duplicate tag is refused with the
    // name a caller needs to fix it. Distinctness is the caller's to own —
    // this just stops it being silently violated.
    if let Some(jid) = running_jid(&jail_name) {
        // ★★ AN EMPTY JAIL IS A HUSK, NOT AN INSTANCE. Processes inside
        // decide: some means a live instance and a genuine duplicate; none
        // means wreckage this run should clear.
        if jailed_process_count(jid) > 0 {
            return OneShot::Refused(format!(
                "jail {jail_name} is already running (jid {jid}); \
                 instance tags must be distinct while they overlap"));
        }
        eprintln!("portcullis: reclaiming abandoned jail {jail_name} (jid {jid}, no processes)");
    }

    // Now safe: anything left under this name belongs to a run that is gone.
    teardown(&jail_path, &jail_name);

    if let Err(e) = std::fs::create_dir_all(&jail_path) {
        return OneShot::Failed(format!("mkdir {}: {e}", jail_path.display()));
    }
    if let Err(e) = mount_layers(&tree, &jail_path, &jail_name, tmpfs_mb) {
        teardown(&jail_path, &jail_name);
        return OneShot::Failed(e.to_string());
    }
    // A networked worker resolves names through the host's resolvers (reached
    // over NAT). Written into the discarded tmpfs layer, never the signed tree.
    let net_mode = manifest.capabilities.network.as_ref().map(|n| n.mode());
    if matches!(net_mode, Some(portcullis_toml::NetworkCap::Full)) {
        let etc = jail_path.join("etc");
        if let Err(e) = std::fs::create_dir_all(&etc)
            .and_then(|_| std::fs::copy("/etc/resolv.conf", etc.join("resolv.conf")).map(|_| ()))
        {
            teardown(&jail_path, &jail_name);
            return OneShot::Failed(format!("resolv.conf for a networked worker: {e}"));
        }
    }
    // ★ File mountpoints (sockets) must exist as FILES before the mount; jaild
    // creates only directories. Shared with the application path.
    if let Err(e) = portcullis_mounts::ensure_mountpoints(&jc) {
        teardown(&jail_path, &jail_name);
        return OneShot::Failed(e);
    }

    // ★ An opt-in static cap, by NAME, before the jail exists — rctl accepts
    // a rule ahead of its jail, so the worker is capped from its first
    // instruction. See Spec::memory_mb for why this is off by default.
    if let Some(mb) = spec.memory_mb {
        if racct_enabled() {
            if let Err(e) = apply_memory_cap(&jail_name, mb) {
                teardown(&jail_path, &jail_name);
                return OneShot::Failed(format!("memory cap: {e}"));
            }
        } else if spec.require_memory_limit {
            teardown(&jail_path, &jail_name);
            return OneShot::Refused(
                "a memory limit was required but kern.racct.enable is 0; \
                 RACCT is a loader tunable, so this machine cannot enforce one \
                 until it reboots with kern.racct.enable=1".into());
        }
    }

    // ★★ jaild creates the jail and execs the entry itself (portcullis.md
    // §6.5.4): pdfork + jail_set(CREATE|ATTACH) + a real execve of the entry,
    // with the caller's stdio dup2'd onto 0/1/2. No jail.conf, no jail(8), no
    // intervening /bin/sh — and no `-q` to remember, because nothing but the
    // worker ever writes to the caller's pipe. The devfs ruleset is checked
    // loaded by jaild (§9.1b), and the exit status is the worker's own rather
    // than jail(8)'s collapse of every failure to 1.
    let network = match jaild_network(manifest.capabilities.network.as_ref(), &manifest.app.id, &host_identity) {
        Ok(n) => n,
        Err(e) => { teardown(&jail_path, &jail_name); return OneShot::Refused(format!("network: {e}")); }
    };
    let entry = format!("/{}", manifest.entry());
    let req = jaild::protocol::Request::CreateJail(jaild::protocol::CreateJailRequest {
        name:          jail_name.clone(),
        path:          jail_path.to_string_lossy().into_owned(),
        children_max:  0,
        mounts,
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        // ★ Isolated, not Disable: an own empty vnet, so the worker cannot
        // list the host's interfaces or read its real MAC (§9.1c).
        network,
        exec: Some(jaild::protocol::ExecSpec {
            path:  entry.clone(),
            argv:  vec![entry],
            env:   clean_env(&pw),
            uid:   pw.uid,
            gid:   pw.gid,
            stdio: true,
        }),
        hostname: Some(manifest.app.id.clone()),
        hostid:   Some(host_identity.hostid),
        hostuuid: Some(host_identity.hostuuid.clone()),
    });
    let outcome = run_via_jaild(&req, stdio);

    jaild_remove(&jail_name);
    teardown(&jail_path, &jail_name);
    outcome
}

const JAILD_SOCK: &str = "/var/run/atrium/jaild.sock";

/// The caller's user, as the jail will run it.
struct PwEntry { uid: u32, gid: u32, home: String }

fn passwd_entry(user: &str) -> Option<PwEntry> {
    let cuser = std::ffi::CString::new(user).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe {
        libc::getpwnam_r(cuser.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result)
    };
    if r != 0 || result.is_null() { return None }
    let home = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }.to_string_lossy().into_owned();
    Some(PwEntry { uid: pwd.pw_uid, gid: pwd.pw_gid, home })
}

/// The environment jail(8)'s `exec.clean` gave the entry — HOME, USER,
/// LOGNAME, SHELL and a system PATH — and nothing inherited from the caller.
fn clean_env(pw: &PwEntry) -> Vec<jaild::protocol::EnvPair> {
    let user = passwd_name(pw.uid).unwrap_or_default();
    [("HOME", pw.home.clone()), ("USER", user.clone()), ("LOGNAME", user),
     ("SHELL", "/bin/sh".into()),
     ("PATH", "/sbin:/bin:/usr/sbin:/usr/bin:/usr/local/sbin:/usr/local/bin".into())]
        .into_iter()
        .map(|(k, v)| jaild::protocol::EnvPair { key: k.into(), value: v })
        .collect()
}

fn passwd_name(uid: u32) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if r != 0 || result.is_null() { return None }
    Some(unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
}

/// Translate the build's capability mounts into jaild's. ★ Anything jaild's
/// CreateJail cannot carry is an error for the caller to refuse — device
/// grants (per-mount devfs rules) and any network — never dropped: a
/// capability that silently does not apply is a worker running without what
/// its manifest says it has.
fn jaild_mounts(jc: &portcullis_jail::JailConfig, root: &Path)
    -> Result<Vec<jaild::protocol::MountSpec>, String>
{
    use jaild::protocol::{MountKind, MountSpec};
    if !jc.devfs_actions.is_empty() {
        return Err("device capabilities are not supported on the one-shot lane yet \
                    (jaild cannot apply per-jail devfs grants)".into());
    }
    jc.mounts.iter().map(|m| {
        if m.fstype != "nullfs" {
            return Err(format!("{} mount at {} is not supported on the one-shot lane",
                               m.fstype, m.dst.display()));
        }
        let rel = m.dst.strip_prefix(root).map_err(|_| format!(
            "mount destination {} is outside the jail root", m.dst.display()))?;
        Ok(MountSpec {
            source:  m.src.to_string_lossy().into_owned(),
            dest:    format!("/{}", rel.display()),
            kind:    if m.opts.iter().any(|o| o == "ro") { MountKind::RoNullfs } else { MountKind::RwNullfs },
            size_mb: None,
        })
    }).collect()
}

/// Turn a manifest's `network` into the structured grants jaild renders
/// (network.md §0.1). `Ok(None)` for `network = "full"` (outbound anywhere).
///
/// ★ Hostnames are resolved HERE, once, at launch, to IPv4 /32s — jaild never
/// sees a name. A name that does not resolve REFUSES the launch rather than
/// running the app with a silently shorter allow-list.
pub fn net_grants(spec: &portcullis_toml::NetworkSpec, app_id: &str)
    -> Result<Option<jaild::protocol::NetGrants>, String>
{
    use jaild::protocol::{NetDest, NetGrants, NetPeer};
    let Some(g) = spec.grants() else { return Ok(None) };
    let mut out = NetGrants {
        app_key: portcullis_identity::consent_key(app_id),
        inbound: g.inbound.clone(),
        peers: g.peers.iter().map(|p| NetPeer {
            app_key: portcullis_identity::consent_key(&p.app_id), port: p.port }).collect(),
        ..Default::default()
    };
    match &g.outbound {
        portcullis_toml::Outbound::Any => out.outbound_any = true,
        portcullis_toml::Outbound::List(list) => for d in list {
            let udp = d.proto == portcullis_toml::Proto::Udp;
            let cidrs: Vec<String> = if d.host.contains('/') {
                vec![d.host.clone()]
            } else if d.host.parse::<std::net::Ipv4Addr>().is_ok() {
                vec![format!("{}/32", d.host)]
            } else {
                use std::net::ToSocketAddrs;
                let v4: Vec<String> = (d.host.as_str(), 0).to_socket_addrs()
                    .map_err(|e| format!("outbound {:?}: cannot resolve: {e}", d.host))?
                    .filter_map(|a| match a.ip() { std::net::IpAddr::V4(ip) => Some(format!("{ip}/32")), _ => None })
                    .collect();
                if v4.is_empty() { return Err(format!("outbound {:?}: no IPv4 address", d.host)) }
                v4
            };
            for cidr in cidrs { out.outbound.push(NetDest { cidr, port: d.port, udp }) }
        },
    }
    Ok(Some(out))
}

/// The jail's network (network.md §0). Every worker gets its OWN stack:
/// `Isolated` (only a down lo0) without the capability, `Routed` with it — a
/// point-to-point epair carrying the app's derived MAC, never the real NIC's.
fn jaild_network(spec: Option<&portcullis_toml::NetworkSpec>, app_id: &str,
                 id: &portcullis_identity::HostIdentity)
    -> Result<jaild::protocol::NetworkConfig, String>
{
    Ok(match spec.map(|s| s.mode()) {
        Some(portcullis_toml::NetworkCap::Full) => jaild::protocol::NetworkConfig::Routed {
            mac: id.mac.clone(),
            grants: net_grants(spec.expect("mode came from it"), app_id)?,
        },
        Some(portcullis_toml::NetworkCap::Loopback) =>
            jaild::protocol::NetworkConfig::Loopback,
        _ => jaild::protocol::NetworkConfig::Isolated,
    })
}

/// Ask jaild to create the jail and run the entry on `stdio` (this process's
/// own when `None`), then wait for it.
///
/// ★★ Our copies of the caller's descriptors are dropped the moment the
/// request is sent. The worker holds its own now; if this process kept the
/// stdout write end, the broker reading it would never see EOF when the worker
/// exits — it would wait on us instead.
fn run_via_jaild(req: &jaild::protocol::Request, stdio: Option<[std::os::fd::OwnedFd; 3]>) -> OneShot {
    use jaild::protocol::Response;
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    let mut client = match jaild::client::Client::connect(JAILD_SOCK) {
        Ok(c) => c,
        Err(e) => return OneShot::Failed(format!(
            "connect {JAILD_SOCK}: {e} — one-shot jails are created by atrium-jaild")),
    };
    let sent = match &stdio {
        Some([i, o, e]) => client.send_with_fds(req, &[i.as_fd(), o.as_fd(), e.as_fd()], 1),
        None => client.send_with_fds(req, &[std::io::stdin().as_fd(),
                                            std::io::stdout().as_fd(),
                                            std::io::stderr().as_fd()], 1),
    };
    drop(stdio);
    let (resp, got) = match sent {
        Ok(x) => x,
        Err(e) => return OneShot::Failed(format!("jaild: {e}")),
    };
    // Owned at once, so every path below closes what jaild handed over.
    let mut got: Vec<OwnedFd> = got.into_iter()
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect();
    match resp {
        Response::JailCreated(r) if r.procdesc_attached => {
            let Some(pd) = got.pop() else {
                return OneShot::Failed("jaild said a procdesc was attached; none arrived".into());
            };
            match wait_procdesc(&pd) {
                Ok(status) => {
                    let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
                    OneShot::Exit { ok: code == Some(0), code }
                }
                Err(e) => OneShot::Failed(format!("wait for worker: {e}")),
            }
        }
        Response::PolicyDenied { rule, detail } =>
            OneShot::Refused(format!("jaild refused ({rule}): {detail}")),
        Response::SyscallFailed { name, errno, msg } =>
            OneShot::Failed(format!("jaild: {name} failed (errno {errno}): {msg}")),
        other => OneShot::Failed(format!("jaild: unexpected reply {other:?}")),
    }
}

/// Block until the process behind `pd` exits; its wait(2)-style status.
///
/// ★ No race with an early exit: registering EVFILT_PROCDESC on a process
/// that has already exited reports NOTE_EXIT at once, with the status
/// (sys_procdesc.c, "initial test after registration").
#[cfg(target_os = "freebsd")]
fn wait_procdesc(pd: &std::os::fd::OwnedFd) -> std::io::Result<i32> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(std::io::Error::last_os_error()) }
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = pd.as_raw_fd() as usize;
    ev.filter = libc::EVFILT_PROCDESC;
    ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
    ev.fflags = libc::NOTE_EXIT;
    loop {
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, &mut out, 1, std::ptr::null()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted { continue }
            return Err(e);
        }
        if n == 1 && out.fflags & libc::NOTE_EXIT != 0 {
            return Ok(out.data as i32);
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn wait_procdesc(_pd: &std::os::fd::OwnedFd) -> std::io::Result<i32> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "procdesc: FreeBSD only"))
}

/// Drop jaild's record of the jail. The jail itself is already gone (it was
/// created persist=0 and its only process exited); this keeps jaild's state
/// from accumulating one record per worker. Best-effort: a failure leaves a
/// stale record, not a live jail.
fn jaild_remove(name: &str) {
    if let Ok(mut c) = jaild::client::Client::connect(JAILD_SOCK) {
        let _ = c.send(&jaild::protocol::Request::RemoveJail { jid: None, name: Some(name.into()) });
    }
}

/// The jid of a live jail with this name, if any. `jls` is the kernel's own
/// answer; tracking liveness in a file would be a second source of truth that
/// a killed process could leave wrong.
fn running_jid(name: &str) -> Option<u32> {
    let out = Command::new("jls").arg("-j").arg(name).arg("jid").output().ok()?;
    if !out.status.success() { return None }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// How many processes are inside a jail. Zero means the jail object outlived
/// whatever it was created for.
fn jailed_process_count(jid: u32) -> usize {
    match Command::new("ps").arg("-J").arg(jid.to_string()).arg("-o").arg("pid=").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).lines()
            .filter(|l| !l.trim().is_empty()).count(),
        // ★ Unknown reads as OCCUPIED. If the process table cannot be read,
        // the safe assumption is that something is in there — tearing down a
        // jail that might hold a live reader is the worse mistake.
        Err(_) => 1,
    }
}

fn resolve_tree(target: &str) -> PathBuf {
    if target.contains('/') || target.starts_with('.') {
        PathBuf::from(target)
    } else {
        PathBuf::from(APPS_DIR).join(target)
    }
}

/// Where a one-shot jail's writable layer lives while it runs.
fn upper_dir(jail_name: &str) -> PathBuf {
    PathBuf::from("/var/run/portcullis-exec").join(jail_name)
}

/// Read-only app tree, with a tmpfs upper layer UNIONED over it.
///
/// ★★ THE UNION IS THE POINT, AND THE FIRST VERSION GOT IT WRONG. Mounting
/// tmpfs directly onto the jail root does not layer over the tree — it MASKS
/// it, so the jail booted with an empty root and `exec /bin/sh` failed with
/// "No such file or directory" while every mount had succeeded. A stacking
/// filesystem and a second mount at the same point look identical in
/// `mount -p`; only running it tells you which one you built.
///
/// ★ The upper layer is tmpfs rather than a persistent overlay because a
/// worker has no state worth keeping between documents — and because a lane
/// that creates and destroys jails continuously must not accumulate on-disk
/// overlays that something else then has to garbage-collect. It is mounted
/// OUTSIDE the jail root so the union has two distinct directories to join.
fn mount_layers(tree: &Path, jail_path: &Path, jail_name: &str, tmpfs_mb: u32)
    -> std::io::Result<()>
{
    let upper = upper_dir(jail_name);
    std::fs::create_dir_all(&upper)?;
    sh("mount", &["-t", "nullfs", "-o", "ro",
                   &tree.to_string_lossy(), &jail_path.to_string_lossy()])?;
    let size = format!("size={tmpfs_mb}m");
    sh("mount", &["-t", "tmpfs", "-o", &size, "tmpfs", &upper.to_string_lossy()])?;
    sh("mount", &["-t", "unionfs", &upper.to_string_lossy(),
                   &jail_path.to_string_lossy()])
}

/// Run a host command, failing loudly. Named `sh` rather than `run` since
/// `run` is this crate's own operation.
fn sh(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

/// Stop the jail and unwind its mounts.
///
/// ★ TWO ROOTS. The writable layer is mounted OUTSIDE the jail root, so a
/// teardown that swept only the root left one tmpfs per run alive in
/// /var/run — invisible to anyone looking at the jail, and accumulating
/// exactly as fast as the pool churns.
///
/// ★ Force, unlike the application path: by this point the jail is gone and
/// nothing should hold these mounts, so forcing costs nothing and guarantees
/// the next run does not stack on a survivor.
fn teardown(jail_path: &Path, jail_name: &str) {
    let _ = Command::new("jail").arg("-r").arg(jail_name)
        .stderr(std::process::Stdio::null()).status();
    // ★★ A dying jail pins its root: unmounting before it is gone stacks the
    // pile (portcullis_mounts::wait_jail_gone).
    portcullis_mounts::wait_jail_gone(jail_name, portcullis_mounts::JAIL_GONE_TIMEOUT);
    let upper = upper_dir(jail_name);
    // ★ Remove the rctl rule with the jail. Rules are keyed by NAME, and the
    // name is reused by the next worker with that instance tag: a rule left
    // behind would silently apply someone else's cap to it, and rules
    // accumulate in the kernel with nothing to show for them.
    let _ = Command::new("rctl").arg("-r").arg(format!("jail:{jail_name}:memoryuse:"))
        .stderr(std::process::Stdio::null()).status();
    let left = portcullis_mounts::converge(
        &[jail_path, upper.as_path()], portcullis_mounts::Force::Yes);
    portcullis_mounts::warn_survivors("portcullis", &left);
    let _ = std::fs::remove_dir(&upper);
    // ★ The root too, on EVERY exit — only the normal path used to remove it,
    // so each refused or failed run left an empty directory behind.
    // `remove_dir` removes only an empty directory, so a root that still has a
    // mount (or anything else) in it is left for the warning above to explain.
    let _ = std::fs::remove_dir(jail_path);
}

