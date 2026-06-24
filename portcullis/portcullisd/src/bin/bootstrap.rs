//! atrium-portcullisd-bootstrap — system-service launcher +
//! supervisor.
//!
//! Reads `/etc/atrium/services.d/*.toml`, connects to jaild,
//! sends a `CreateJail` request for each enabled manifest in
//! lexicographic order. For exec'd services it then enters a
//! kqueue loop with `EVFILT_PROCDESC` registrations; on
//! `NOTE_EXIT` it consults the manifest's `[supervision].restart`
//! policy and either re-launches (with cooldown + burst cap) or
//! retires the service.
//!
//! Persistent-jail manifests (no `[exec]` block) are launched
//! once and not supervised — they live in jaild's state file
//! until removed.
//!
//! Modes:
//!   default            launch + kqueue-supervise forever
//!   --once             launch + exit (V1 behaviour; the kernel
//!                      closes our procdesc fds → exec'd jails
//!                      die)
//!   --dry-run          parse + log + exit, no jaild calls
//!
//! Usage:
//!     atrium-portcullisd-bootstrap [--socket /var/run/atrium/jaild.sock]
//!                                  [--services-dir /etc/atrium/services.d]
//!                                  [--once] [--dry-run]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jaild::protocol::{EnvPair, ExecSpec, MountKind, MountSpec, Request, Response};
use log::{error, info, warn};
use portcullisd::host_mount;
use portcullisd::init_phase::{self, InitOutcome};
use portcullisd::jaild_client::Client;
use portcullisd::supervisor::{self, Supervisor};
use portcullisd::system_services::{self, LoadOutcome, ManifestVolumeKind, ServiceManifest};
use portcullisd::volumes_client;

const DEFAULT_SOCKET: &str = "/var/run/atrium/jaild.sock";
const DEFAULT_SVCDIR: &str = "/etc/atrium/services.d";
const DEFAULT_VOLUMES_SOCKET: &str = "/var/run/atrium/atrium-volumes.sock";

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-portcullisd-bootstrap
        [--socket <path>]            jaild socket
                                     (default: {DEFAULT_SOCKET})
        [--volumes-socket <path>]    atrium-volumes socket
                                     (default: {DEFAULT_VOLUMES_SOCKET};
                                     pass empty to skip [[volumes]]
                                     resolution — manifests with
                                     persistent volumes will then
                                     fail to launch)
        [--services-dir <path>]      (default: {DEFAULT_SVCDIR})
        [--once]                     launch + exit (no supervision;
                                     exec'd jails die when fd closes)
        [--dry-run]                  parse + log manifests, don't
                                     send anything to jaild
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    /* Catch SIGTERM / SIGINT so the supervisor can run its
     * graceful-shutdown path (close procdescs, unmount each
     * service's mounts, RemoveJail) before we exit. Without
     * this, kqueue blocks forever and a service-stop SIGKILLs
     * us, leaking host-namespace mounts. The handler installs
     * before we open any sockets so we don't miss an early
     * SIGTERM during init. */
    if let Err(e) = supervisor::install_shutdown_handlers() {
        error!("install signal handlers: {e}");
        return ExitCode::FAILURE;
    }

    let mut socket_path     = PathBuf::from(DEFAULT_SOCKET);
    let mut volumes_socket  = Some(PathBuf::from(DEFAULT_VOLUMES_SOCKET));
    let mut services_dir    = PathBuf::from(DEFAULT_SVCDIR);
    let mut once            = false;
    let mut dry_run         = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket"          => match args.next() {
                Some(v) => socket_path = v.into(), None => return usage(),
            },
            "--volumes-socket"  => match args.next() {
                Some(v) => {
                    volumes_socket = if v.is_empty() {
                        None
                    } else {
                        Some(v.into())
                    };
                },
                None => return usage(),
            },
            "--services-dir"    => match args.next() {
                Some(v) => services_dir = v.into(), None => return usage(),
            },
            "--once"            => once    = true,
            "--dry-run"         => dry_run = true,
            "--help" | "-h"     => return usage(),
            other => { eprintln!("unknown arg: {other}"); return ExitCode::from(2); }
        }
    }

    let LoadOutcome { manifests, errors } = match system_services::load_dir(&services_dir) {
        Ok(o)  => o,
        Err(e) => {
            error!("read services dir {}: {e}", services_dir.display());
            return ExitCode::FAILURE;
        }
    };
    for (path, err) in &errors {
        warn!("manifest {} skipped: {err}", path.display());
    }

    let enabled: Vec<_> = manifests.into_iter().filter(|m| m.enabled).collect();
    info!("loaded {} enabled service manifest(s) from {}",
        enabled.len(), services_dir.display());

    if dry_run {
        for m in &enabled {
            info!("dry-run: {}: name={} path={} mounts={} exec={}",
                m.name, m.name, m.path, m.mounts.len(),
                m.exec.is_some());
        }
        return ExitCode::SUCCESS;
    }

    let client = match Client::connect(&socket_path) {
        Ok(c) => c,
        Err(e) => {
            error!("connect jaild socket {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };

    /* Optional atrium-volumes connection. None = manifests with
     * `[[volumes]]` of `kind = "persistent"` will fail; tmpfs
     * still works (no allocator needed). */
    let mut volumes = match &volumes_socket {
        Some(path) => match volumes_client::Client::connect(path) {
            Ok(c) => {
                info!("connected to atrium-volumes at {}", path.display());
                Some(c)
            }
            Err(e) => {
                error!("connect atrium-volumes socket {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    /* The supervisor takes ownership of the jaild connection and
     * reuses it for relaunches — opening a second connection on a
     * single-threaded jaild deadlocks. In --once mode we use
     * the connection directly here and don't supervise. */
    enum Driver { Once(Client), Supervise(Supervisor) }
    let mut driver = if once {
        Driver::Once(client)
    } else {
        match Supervisor::new(client, socket_path.clone()) {
            Ok(s)  => Driver::Supervise(s),
            Err(e) => {
                error!("kqueue init: {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    /* In --once mode each exec'd jail's procdesc fd is held here
     * until process exit; on drop the kernel closes it and the
     * persist=0 jail dies. We also remember the jail's mount
     * destinations + name so we can RemoveJail and unmount the
     * leaked host-namespace mounts before exiting. */
    struct HeldJail { fd: i32, name: String, jail_path: String, mount_dests: Vec<(String, MountKind)> }
    let mut held: Vec<HeldJail> = Vec::new();
    let mut launch_failures = 0;

    'manifest: for m in enabled {
        let mut create_req = m.to_create_request();

        /* Resolve [[volumes]] before launching: ask atrium-volumes
         * to provision each persistent volume, then append the
         * returned host paths as rw_nullfs mounts. tmpfs volumes
         * become Tmpfs mounts directly (no allocator). cas
         * volumes are V1. */
        let mut persistent_host_paths: Vec<(String, String)> = Vec::new();
        if !m.volumes.is_empty() {
            match resolve_volumes(&m, &mut volumes) {
                Ok((extra_mounts, host_paths)) => {
                    create_req.mounts.extend(extra_mounts);
                    persistent_host_paths = host_paths;
                }
                Err(e) => {
                    error!("{}: volume resolution: {e}", m.name);
                    launch_failures += 1;
                    continue;
                }
            }
        }

        /* Capability-derived mounts. Per `docs/spec/aqueduct.md`
         * §6.1, the capability boundary is the filesystem: jails
         * granted [capabilities] attach_mount = true get the
         * portcullisd socket nullfs-mounted into their chroot,
         * jails without the grant simply don't see the directory.
         * Source path comes from jaild's policy ro_paths
         * allow-list (operator-curated). */
        for mount in capability_mounts(&m) {
            create_req.mounts.push(mount);
        }

        /* First-run init for any volume with [[volumes.init]] and
         * no sentinel. Init runs in a one-shot jail with the same
         * mounts as the real service; bootstrap blocks here until
         * each init completes (kqueue+EVFILT_PROCDESC). On any
         * failure, the manifest's real launch is skipped — better
         * to leave the service down than to start it against a
         * half-initialized volume. */
        for v in &m.volumes {
            let Some(init_exec_m) = &v.init else { continue; };
            let Some((_, host_path)) = persistent_host_paths.iter()
                .find(|(name, _)| name == &v.name) else {
                error!("{}: volume {:?} has [init] but no host path \
                        (kind != persistent?)", m.name, v.name);
                launch_failures += 1;
                continue 'manifest;
            };
            let init_exec = ExecSpec {
                path: init_exec_m.path.clone(),
                argv: init_exec_m.argv.clone(),
                uid:  init_exec_m.uid,
                gid:  init_exec_m.gid,
                env:  init_exec_m.env.iter().map(|p| EnvPair {
                    key:   p.key.clone(),
                    value: p.value.clone(),
                }).collect(),
            };
            let client = match &mut driver {
                Driver::Once(c)      => c,
                Driver::Supervise(s) => s.client_mut(),
            };
            let init_result = init_phase::run_init(
                client,
                &m.name,
                &v.name,
                host_path,
                &m.path,
                create_req.mounts.clone(),
                init_exec,
            );
            match init_result {
                Ok(InitOutcome::Ran)         => { /* sentinel was just written */ }
                Ok(InitOutcome::AlreadyDone) => { /* nothing to do */ }
                Err(e) => {
                    error!("{}: init phase for volume {}: {e}", m.name, v.name);
                    launch_failures += 1;
                    continue 'manifest;
                }
            }
        }

        /* Save the resolved request — Supervise mode keeps it for
         * relaunch (manifest.to_create_request() alone drops
         * [[volumes]] mounts); Once mode keeps the dests for
         * teardown unmount. */
        let saved_req = create_req.clone();
        let mount_dests: Vec<(String, MountKind)> = saved_req.mounts.iter()
            .map(|x| (x.dest.clone(), x.kind))
            .collect();
        let jail_name_for_held = saved_req.name.clone();
        let jail_path_for_held = saved_req.path.clone();

        let req = Request::CreateJail(create_req);
        let send_result = match &mut driver {
            Driver::Once(c)       => c.send(&req),
            Driver::Supervise(s)  => s.client_mut().send(&req),
        };
        match send_result {
            Ok((Response::JailCreated(r), fd)) => {
                info!("launched {}: jid={} pid={} procdesc_attached={}",
                    m.name, r.jid, r.pid, r.procdesc_attached);
                match (fd, &mut driver) {
                    (Some(fd), Driver::Supervise(sup)) => {
                        if let Err(e) = sup.watch(m, fd, r.pid, saved_req) {
                            error!("watch register failed: {e}");
                            launch_failures += 1;
                        }
                    }
                    (Some(fd), Driver::Once(_)) => held.push(HeldJail {
                        fd, name: jail_name_for_held, jail_path: jail_path_for_held, mount_dests,
                    }),
                    (None, _) => { /* persistent jail, no fd */ }
                }
            }
            Ok((Response::PolicyDenied { rule, detail }, _)) => {
                error!("policy denied {}: {rule}: {detail}", m.name);
                launch_failures += 1;
            }
            Ok((Response::SyscallFailed { name, errno, msg }, _)) => {
                error!("syscall {} failed for {}: errno={errno} {msg}",
                    name, m.name);
                launch_failures += 1;
            }
            Ok((other, _)) => {
                error!("unexpected response for {}: {other:?}", m.name);
                launch_failures += 1;
            }
            Err(e) => {
                error!("rpc to jaild for {}: {e}", m.name);
                launch_failures += 1;
            }
        }
    }

    match driver {
        Driver::Supervise(mut sup) => {
            /* Drop the persistent jaild connection now that the
             * launch loop is done. While the supervisor is
             * blocked on kqueue, no one in this process holds
             * a connection — leaving jaild free for the
             * portcullisd daemon to forward in-jail
             * AttachMount/DetachMount requests. The supervisor
             * reconnects lazily on its next operation
             * (handle_exit's RemoveJail or a relaunch). */
            sup.release_client();
            info!("bootstrap launched {} supervised + {} held; entering kqueue loop",
                sup.watched_count(), held.len());
            if let Err(e) = sup.run() {
                error!("supervisor.run: {e}");
                return ExitCode::FAILURE;
            }
        }
        Driver::Once(mut client) => {
            info!("bootstrap done; {} held, {} failed (--once mode); cleaning up",
                held.len(), launch_failures);
            /* For each held jail (in reverse launch order, so a
             * later jail's mounts are unmounted before an earlier
             * jail's — relevant if they stack on the same target
             * via init_phase + real-service):
             *   1. close(fd) — kernel kills jailed proc + reaps jail
             *   2. RemoveJail-by-name — clears jaild's state.json
             *      entry + any lo0 alias.
             *   3. unmount each mount dest from the host namespace.
             * Errors at any step are logged but not fatal — best-
             * effort cleanup is still better than dropping fds
             * with no cleanup at all. */
            while let Some(h) = held.pop() {
                if let Err(e) = host_mount::close_fd(h.fd) {
                    warn!("close procdesc for {}: {e}", h.name);
                }
                let rm_req = Request::RemoveJail {
                    jid:  None,
                    name: Some(h.name.clone()),
                };
                if let Err(e) = client.send(&rm_req) {
                    warn!("RemoveJail({}) on cleanup: {e}", h.name);
                }
                for (dest, kind) in &h.mount_dests {
                    if let Err(e) = host_mount::unmount_jail_dest(&h.jail_path, dest) {
                        warn!("cleanup unmount {kind:?} {dest}: {e}");
                    }
                }
            }
        }
    }

    let _ = Duration::from_secs(0);  // silence unused-import lint
    if launch_failures == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Walk the manifest's `[capabilities]` block and emit the
/// per-capability mounts to add to the CreateJailRequest.
///
/// Currently the only capability that materializes as a mount is
/// `attach_mount = true` → ro_nullfs mount of the portcullisd
/// daemon's per-capability directory. Future caps (clipboard,
/// notify, audio, ...) follow the same shape: each has a
/// `/var/run/atrium/caps/<name>/` directory on the host (curated
/// by jaild policy ro_paths) and shows up in the jail at
/// `/atrium/sockets/<name>/`. Per `docs/spec/aqueduct.md` §6.1.
fn capability_mounts(m: &ServiceManifest) -> Vec<MountSpec> {
    let mut out = Vec::new();
    if m.capabilities.attach_mount {
        out.push(MountSpec {
            source: "/var/run/atrium/caps/portcullisd/".into(),
            dest:   "/atrium/sockets/portcullisd/".into(),
            kind:   MountKind::RoNullfs,
        });
    }
    out
}

/// Walk the manifest's `[[volumes]]` and turn each into a
/// `MountSpec` for the jail. Persistent volumes go through
/// atrium-volumes (Provision returns a host path). Tmpfs
/// volumes are direct (jaild handles the mount; source ignored
/// per atrium-volumes V0).
fn resolve_volumes(
    m:        &ServiceManifest,
    volumes:  &mut Option<volumes_client::Client>,
) -> Result<(Vec<MountSpec>, Vec<(String, String)>), String> {
    use atrium_volumes::protocol::{ProvisionRequest, Request as VReq, Response as VResp};

    let mut mounts:     Vec<MountSpec>          = Vec::with_capacity(m.volumes.len());
    let mut host_paths: Vec<(String, String)>   = Vec::new();
    for v in &m.volumes {
        match v.kind {
            ManifestVolumeKind::Tmpfs => {
                mounts.push(MountSpec {
                    source: format!("tmpfs::{}/{}", m.name, v.name),
                    dest:   v.mount_at.clone(),
                    kind:   MountKind::Tmpfs,
                });
            }
            ManifestVolumeKind::Persistent => {
                let client = volumes.as_mut().ok_or_else(|| format!(
                    "volume {:?}: kind = \"persistent\" but bootstrap was started \
                     with --volumes-socket \"\" (allocator unavailable)",
                    v.name))?;
                let req = VReq::Provision(ProvisionRequest {
                    jail_name: m.name.clone(),
                    volume:    v.to_volume_spec(),
                });
                let host_path = match client.send(&req) {
                    Ok(VResp::Provisioned { host_path })
                    | Ok(VResp::AlreadyProvisioned { host_path }) => host_path,
                    Ok(VResp::PolicyDenied { rule, detail }) => {
                        return Err(format!("volume {:?}: policy denied: {rule}: {detail}",
                            v.name));
                    }
                    Ok(VResp::BackendUnavailable { name, configured }) => {
                        return Err(format!(
                            "volume {:?}: backend {name:?} not configured (have: {configured:?})",
                            v.name));
                    }
                    Ok(other) => {
                        return Err(format!("volume {:?}: unexpected response: {other:?}",
                            v.name));
                    }
                    Err(e) => return Err(format!("volume {:?}: rpc: {e}", v.name)),
                };
                info!("{}: provisioned volume {} → {}", m.name, v.name, host_path);
                mounts.push(MountSpec {
                    source: host_path.clone(),
                    dest:   v.mount_at.clone(),
                    kind:   MountKind::RwNullfs,
                });
                host_paths.push((v.name.clone(), host_path));
            }
        }
    }
    Ok((mounts, host_paths))
}
