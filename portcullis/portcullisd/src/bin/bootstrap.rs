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

use jaild::protocol::{MountKind, MountSpec, Request, Response};
use log::{error, info, warn};
use portcullisd::jaild_client::Client;
use portcullisd::supervisor::Supervisor;
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
        match Supervisor::new(client) {
            Ok(s)  => Driver::Supervise(s),
            Err(e) => {
                error!("kqueue init: {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    let mut held_fds: Vec<i32> = Vec::new();
    let mut launch_failures = 0;

    for m in enabled {
        let mut create_req = m.to_create_request();

        /* Resolve [[volumes]] before launching: ask atrium-volumes
         * to provision each persistent volume, then append the
         * returned host paths as rw_nullfs mounts. tmpfs volumes
         * become Tmpfs mounts directly (no allocator). cas
         * volumes are V1. */
        if !m.volumes.is_empty() {
            match resolve_volumes(&m, &mut volumes) {
                Ok(extra_mounts) => create_req.mounts.extend(extra_mounts),
                Err(e) => {
                    error!("{}: volume resolution: {e}", m.name);
                    launch_failures += 1;
                    continue;
                }
            }
        }

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
                        if let Err(e) = sup.watch(m, fd, r.pid) {
                            error!("watch register failed: {e}");
                            launch_failures += 1;
                        }
                    }
                    (Some(fd), Driver::Once(_)) => held_fds.push(fd),
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
            info!("bootstrap launched {} supervised + {} held; entering kqueue loop",
                sup.watched_count(), held_fds.len());
            if let Err(e) = sup.run() {
                error!("supervisor.run: {e}");
                return ExitCode::FAILURE;
            }
        }
        Driver::Once(_client) => {
            info!("bootstrap done; {} held, {} failed (--once mode)",
                held_fds.len(), launch_failures);
            /* About to drop fds → kernel closes the procdescs →
             * exec'd jails with persist=0 die. */
        }
    }

    let _ = Duration::from_secs(0);  // silence unused-import lint
    if launch_failures == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Walk the manifest's `[[volumes]]` and turn each into a
/// `MountSpec` for the jail. Persistent volumes go through
/// atrium-volumes (Provision returns a host path). Tmpfs
/// volumes are direct (jaild handles the mount; source ignored
/// per atrium-volumes V0). Cas volumes return an error in V0.
fn resolve_volumes(
    m:        &ServiceManifest,
    volumes:  &mut Option<volumes_client::Client>,
) -> Result<Vec<MountSpec>, String> {
    use atrium_volumes::protocol::{ProvisionRequest, Request as VReq, Response as VResp};

    let mut mounts: Vec<MountSpec> = Vec::with_capacity(m.volumes.len());
    for v in &m.volumes {
        match v.kind {
            ManifestVolumeKind::Tmpfs => {
                mounts.push(MountSpec {
                    source: format!("tmpfs::{}/{}", m.name, v.name),
                    dest:   v.mount_at.clone(),
                    kind:   MountKind::Tmpfs,
                });
            }
            ManifestVolumeKind::Cas => {
                return Err(format!(
                    "volume {:?}: kind = \"cas\" is V1 (Tessera CAS API not yet wired)",
                    v.name));
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
                    source: host_path,
                    dest:   v.mount_at.clone(),
                    kind:   MountKind::RwNullfs,
                });
            }
        }
    }
    Ok(mounts)
}
