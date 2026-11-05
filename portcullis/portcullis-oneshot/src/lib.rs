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
    /// ★ A CPU cap, percent of one CPU (200 = two cores), enforced as rctl
    /// `pcpu:deny` — a THROTTLE, not a kill (see `jaild::protocol::RctlLimits`).
    /// Combined with the manifest's `[resources].cpu`: the tighter wins.
    pub cpu_pct: Option<u32>,
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
pub fn racct_enabled() -> bool { jaild::ffi::racct_enabled() }

/// The limits a one-shot jail is created with: the manifest's `[resources]`
/// (`memory`, `cpu`) and the caller's (`--memory`, `--cpu`), **the tighter of
/// each** — a caller can demand more confinement than the publisher declared,
/// never less (the same rule as the trust gate, §6.5.3).
///
/// ★ Applied by jaild at CreateJail, as rctl rules it removes with the jail
/// (§6.5.2e) — no more `rctl(8)` from this lane. The memory figure is a
/// CEILING: memfed, budgeting the jail's pool, may set it lower and never
/// higher (jaild clamps).
///
/// `Ok(None)` means run uncapped (nothing asked, or nothing enforceable and
/// nothing required — which is reported on stderr, not silent).
pub fn resolve_limits(manifest: Option<&portcullis_toml::ResourcesSection>, memory_mb: Option<u64>,
                      cpu_pct: Option<u32>, require_memory_limit: bool, racct: bool)
    -> Result<Option<jaild::protocol::RctlLimits>, String>
{
    let (limits, warning) = resolve_limits_noted(manifest, memory_mb, cpu_pct, require_memory_limit, racct)?;
    if let Some(w) = warning { eprintln!("{w}") }
    Ok(limits)
}

/// As `resolve_limits`, but the "running UNCAPPED" warning is RETURNED, so
/// the caller decides where it goes. ★ On the `--daemon` lane this process's
/// stderr is portcullisd's log, and a warning printed there never reached
/// the person who asked for the limit (portcullis.md §6.5.2e′ snags):
/// `run_with_stdio` also writes it to the CALLER's stderr (`tell_caller`).
pub fn resolve_limits_noted(manifest: Option<&portcullis_toml::ResourcesSection>, memory_mb: Option<u64>,
                      cpu_pct: Option<u32>, require_memory_limit: bool, racct: bool)
    -> Result<(Option<jaild::protocol::RctlLimits>, Option<String>), String>
{
    let tighter = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)), (x, None) => x, (None, y) => y };
    let declared_mb = match manifest.and_then(|r| r.memory.as_deref()) {
        Some(m) => Some(parse_mib(m).ok_or_else(|| format!("[resources].memory {m:?} is not a size"))?),
        None => None,
    };
    let mem = tighter(declared_mb, memory_mb);
    let cpu = tighter(manifest.and_then(|r| r.cpu).map(u64::from), cpu_pct.map(u64::from)).map(|c| c as u32);
    if require_memory_limit && mem.is_none() {
        return Err("a memory limit is required and none was given (neither --memory nor \
                    the manifest's [resources].memory)".into());
    }
    let limits = jaild::protocol::RctlLimits { memoryuse_mb: mem, pcpu: cpu };
    if limits.is_empty() { return Ok((None, None)) }
    if !racct {
        if require_memory_limit {
            return Err("a memory limit was required but kern.racct.enable is 0; \
                        RACCT is a loader tunable, so this machine cannot enforce one \
                        until it reboots with kern.racct.enable=1".into());
        }
        let w = format!("portcullis: kern.racct.enable is 0 — running UNCAPPED (asked for {}{}); \
                   reboot with kern.racct.enable=1 to enforce, or pass --require-memory-limit \
                   to refuse instead",
                  mem.map(|m| format!("memoryuse {m} MiB")).unwrap_or_default(),
                  cpu.map(|c| format!(" pcpu {c}%")).unwrap_or_default());
        return Ok((None, Some(w)));
    }
    Ok((Some(limits), None))
}

/// Say `msg` where the person who asked will see it: this process's stderr
/// always, and, when the caller's descriptors arrived over SCM_RIGHTS (the
/// `--daemon` lane, where this process's stderr is portcullisd's log), the
/// CALLER's stderr too. Best effort on the caller's side: a caller that
/// closed its stderr does not stop the run.
pub fn tell_caller(msg: &str, caller_stderr: Option<&std::os::fd::OwnedFd>) {
    eprintln!("{msg}");
    if let Some(fd) = caller_stderr {
        use std::io::Write;
        if let Ok(dup) = fd.try_clone() {
            let mut f = std::fs::File::from(dup);
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// `512M`, `2G`, `1024K`, or a bare byte count → MiB (rounded up).
pub fn parse_mib(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() { return None }
    let (digits, mult) = match s.as_bytes()[s.len() - 1].to_ascii_uppercase() {
        b'K' => (&s[..s.len() - 1], 1024u64),
        b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        b'T' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let bytes = digits.trim().parse::<u64>().ok()?.checked_mul(mult)?;
    Some(bytes.div_ceil(1024 * 1024))
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
    // ★ This lane builds the instance root and reads the machine's identity
    // secret in THIS process, so it needs root — and says so first. Without
    // it an unprivileged caller was told "host identity: Permission denied",
    // which reads like a broken install rather than the wrong lane.
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return OneShot::Refused(
            "a one-shot jail is created by root; an unprivileged caller asks \
             portcullisd (`portcullis exec --daemon`, portcullis.md §6.5.2a)".into());
    }
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

    // ★ Limits are resolved here and APPLIED BY JAILD at CreateJail, before
    // the jail exists, so the worker is capped from its first instruction —
    // and jaild removes them with the jail (§6.5.2e). Resolved before
    // anything is built would be tidier, but the refusal must come after the
    // tree is known to be this app's.
    let rctl = match resolve_limits_noted(manifest.resources.as_ref(), spec.memory_mb, spec.cpu_pct,
                                    spec.require_memory_limit, racct_enabled()) {
        Ok((l, warning)) => {
            if let Some(w) = warning { tell_caller(&w, stdio.as_ref().map(|s| &s[2])) }
            l
        }
        Err(why) => { teardown(&jail_path, &jail_name); return OneShot::Refused(why) }
    };

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
        rctl,
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
            owner: None,
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

/// ★★ RECLAIM WHAT A STOPPED DAEMON LEFT BEHIND.
///
/// A one-shot jail is torn down by whoever launched it: jail removed, then
/// its mounts. Stop that process in between — portcullisd stopped, or
/// crashed, mid-teardown — and the wreckage stays: instance-root mounts
/// (nullfs + tmpfs + unionfs), sometimes a process-less jail. Instance tags
/// are usually unique (a pid), so the reclaim-on-reuse path in `run` never
/// meets them again. Measured: six such roots after runs whose harness
/// stopped portcullisd as soon as the last conversion returned.
///
/// Swept at daemon start. An instance root (`app-…--…` under the jails
/// directory) is wreckage when its jail is gone or holds no process AND it is
/// older than `min_age` — the age guard is for the direct lane, which builds
/// roots outside the daemon and may be mid-launch right now. Returns what was
/// reclaimed.
pub fn reclaim_abandoned(min_age: std::time::Duration) -> Vec<String> {
    let mut reclaimed = vec![];
    let Ok(rd) = std::fs::read_dir(JAILS_DIR) else { return reclaimed };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("app-") && name.contains("--")) { continue }
        let old_enough = e.metadata().and_then(|m| m.modified()).ok()
            .and_then(|t| now.duration_since(t).ok()).is_some_and(|age| age >= min_age);
        if !old_enough { continue }
        if let Some(jid) = running_jid(&name) {
            if jailed_process_count(jid) > 0 { continue }
        }
        teardown(&e.path(), &name);
        jaild_remove(&name);
        reclaimed.push(name);
    }
    reclaimed
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
    // (rctl rules are jaild's now: RemoveJail removes every rule for the name.)
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


#[cfg(test)]
mod limit_tests {
    use super::*;

    fn res(memory: Option<&str>, cpu: Option<u32>) -> portcullis_toml::ResourcesSection {
        let mut t = String::new();
        if let Some(m) = memory { t += &format!("memory = {m:?}\n") }
        if let Some(c) = cpu { t += &format!("cpu = {c}\n") }
        toml::from_str(&t).unwrap()
    }

    #[test]
    fn the_tighter_of_manifest_and_caller_wins_each_limit() {
        let m = res(Some("2G"), Some(200));
        let l = resolve_limits(Some(&m), Some(512), Some(400), false, true).unwrap().unwrap();
        assert_eq!((l.memoryuse_mb, l.pcpu), (Some(512), Some(200)));
        let l = resolve_limits(Some(&m), Some(8192), None, false, true).unwrap().unwrap();
        assert_eq!((l.memoryuse_mb, l.pcpu), (Some(2048), Some(200)), "a caller cannot loosen the manifest");
        let l = resolve_limits(None, None, Some(50), false, true).unwrap().unwrap();
        assert_eq!((l.memoryuse_mb, l.pcpu), (None, Some(50)));
        assert_eq!(resolve_limits(None, None, None, false, true).unwrap(), None);
    }

    #[test]
    fn require_memory_limit_refuses_rather_than_runs_uncapped() {
        // Nothing to cap with.
        assert!(resolve_limits(None, None, Some(100), true, true).unwrap_err().contains("none was given"));
        // A cap, but a kernel that cannot enforce it: refused, naming the tunable.
        let e = resolve_limits(None, Some(512), None, true, false).unwrap_err();
        assert!(e.contains("kern.racct.enable"), "{e}");
        // Not required: runs uncapped (and says so on stderr), never sends a
        // limit jaild would have to refuse.
        assert_eq!(resolve_limits(None, Some(512), None, false, false).unwrap(), None);
        // Required and enforceable.
        assert!(resolve_limits(Some(&res(Some("1G"), None)), None, None, true, true).unwrap().is_some());
    }

    /// ★ The `--daemon` lane's snag (§6.5.2e′): the UNCAPPED warning went
    /// only to portcullisd's log. It is returned now, and `tell_caller`
    /// writes it to the caller's own stderr descriptor.
    #[test]
    fn the_uncapped_warning_reaches_the_callers_stderr() {
        let (l, w) = resolve_limits_noted(None, Some(512), Some(50), false, false).unwrap();
        assert_eq!(l, None);
        let w = w.expect("a limit the kernel cannot enforce is reported");
        assert!(w.contains("UNCAPPED") && w.contains("memoryuse 512 MiB") && w.contains("pcpu 50%"), "{w}");
        // Nothing asked: nothing to warn about. Enforceable: no warning.
        assert_eq!(resolve_limits_noted(None, None, None, false, false).unwrap(), (None, None));
        assert!(resolve_limits_noted(None, Some(512), None, false, true).unwrap().1.is_none());

        // The caller's stderr, as the daemon receives it: a descriptor.
        use std::io::Read;
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd: std::os::fd::OwnedFd = ours.into();
        tell_caller(&w, Some(&fd));
        drop(fd);
        let mut got = String::new();
        theirs.read_to_string(&mut got).unwrap();
        assert_eq!(got, format!("{w}\n"));
    }

    #[test]
    fn sizes_are_mib_rounded_up() {
        assert_eq!(parse_mib("512M"), Some(512));
        assert_eq!(parse_mib("2G"), Some(2048));
        assert_eq!(parse_mib("1536K"), Some(2));
        assert_eq!(parse_mib("1"), Some(1));
        assert_eq!(parse_mib("lots"), None);
        assert!(resolve_limits(Some(&res(Some("lots"), None)), None, None, false, true).is_err());
    }
}
