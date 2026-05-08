//! Accept-loop + request dispatcher. Same shape as jaild's:
//! single-threaded blocking accept, peer-uid root-only,
//! length-prefixed JSON.

use std::io::BufReader;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use log::{error, info, warn};

use crate::ffi;
use crate::plugin::plugin_for;
use crate::policy::Policy;
use crate::protocol::{
    self, BackendInfo, DestroyRequest, ProvisionRequest, Request, Response,
    VolumeKind, VolumeRecord as ProtoVolumeRecord,
};
use crate::state::{State, VolumeRecord};
use crate::VolumesError;

pub fn serve(
    listener:   &UnixListener,
    policy:     &Policy,
    state_path: &Path,
) -> Result<(), VolumesError> {
    let mut state = State::load(state_path)?;
    info!("atrium-volumes: ready ({} volume(s) known, {} backend(s) configured)",
        state.volumes.len(), policy.backends.len());

    for inbound in listener.incoming() {
        let stream = match inbound {
            Ok(s) => s,
            Err(e) => {
                error!("accept: {e}");
                return Err(VolumesError::Io(e));
            }
        };
        if let Err(e) = handle_connection(stream, policy, &mut state, state_path) {
            warn!("connection closed with error: {e}");
        }
    }
    Ok(())
}

fn handle_connection(
    stream:     UnixStream,
    policy:     &Policy,
    state:      &mut State,
    state_path: &Path,
) -> Result<(), VolumesError> {
    let peer_uid = ffi::getpeereid(stream.as_raw_fd()).unwrap_or(u32::MAX);
    if peer_uid != 0 {
        warn!("non-root peer uid={peer_uid}; refusing");
        let body = serde_json::to_vec(&Response::Error {
            detail: "non-root peer".into(),
        })?;
        let mut s = stream;
        let _ = protocol::write_frame(&mut s, &body);
        return Ok(());
    }

    let socket_fd = stream.as_raw_fd();
    let mut reader = BufReader::new(stream.try_clone()?);

    loop {
        let body = match protocol::read_frame(&mut reader)? {
            Some(b) => b,
            None    => return Ok(()),
        };
        let req: Request = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                send(socket_fd, &Response::Error {
                    detail: format!("malformed: {e}"),
                })?;
                continue;
            }
        };

        let resp = dispatch(req, policy, state, state_path);
        send(socket_fd, &resp)?;
    }
}

fn dispatch(
    req:        Request,
    policy:     &Policy,
    state:      &mut State,
    state_path: &Path,
) -> Response {
    match req {
        Request::Ping => Response::Ok,

        Request::ListBackends => Response::Backends {
            backends: policy.backends.iter().map(|b| BackendInfo {
                name:    b.name.clone(),
                kind:    format!("{:?}", b.kind).to_lowercase(),
                default: b.default,
                features: plugin_for(b.kind)
                    .map(|p| p.features().iter().map(|s| (*s).into()).collect())
                    .unwrap_or_default(),
            }).collect(),
        },

        Request::Provision(p) => match handle_provision(&p, policy, state, state_path) {
            Ok(resp) => resp,
            Err(e)   => err_to_resp(e),
        },

        Request::Destroy(d) => match handle_destroy(&d, policy, state, state_path) {
            Ok(resp) => resp,
            Err(e)   => err_to_resp(e),
        },

        Request::Status(s) => Response::Status {
            volumes: state.volumes.iter()
                .filter(|v| match &s.jail_name {
                    Some(j) => &v.jail_name == j,
                    None    => true,
                })
                .map(|v| ProtoVolumeRecord {
                    jail_name:        v.jail_name.clone(),
                    volume_name:      v.volume_name.clone(),
                    backend:          v.backend.clone(),
                    backend_kind:     v.backend_kind.clone(),
                    host_path:        v.host_path.clone(),
                    mount_at:         v.mount_at.clone(),
                    allocated_at_unix: v.allocated_at_unix,
                })
                .collect(),
        },
    }
}

fn handle_provision(
    req:        &ProvisionRequest,
    policy:     &Policy,
    state:      &mut State,
    state_path: &Path,
) -> Result<Response, VolumesError> {
    /* Tmpfs short-circuits — no persistent state, no plugin. */
    if let VolumeKind::Tmpfs = req.volume.kind {
        let host_path = format!("tmpfs::{}/{}", req.jail_name, req.volume.name);
        return Ok(Response::Provisioned { host_path });
    }

    /* Idempotent: existing record returns AlreadyProvisioned. */
    if let Some(existing) = state.find(&req.jail_name, &req.volume.name) {
        return Ok(Response::AlreadyProvisioned {
            host_path: existing.host_path.clone(),
        });
    }

    /* Resolve backend by name (or default). */
    let backend = match policy.resolve(req.volume.backend.as_deref()) {
        Some(b) => b,
        None => {
            return Ok(Response::BackendUnavailable {
                name: req.volume.backend.clone()
                    .unwrap_or_else(|| "<default>".into()),
                configured: policy.backend_names(),
            });
        }
    };

    let plugin = match plugin_for(backend.kind) {
        Some(p) => p,
        None => {
            return Err(VolumesError::PolicyViolation {
                rule:   "backend.kind_not_implemented_v0",
                detail: format!("backend kind {:?} is not implemented in V0", backend.kind),
            });
        }
    };

    let host_path = plugin.provision(backend, &req.jail_name, &req.volume)?;

    state.add(VolumeRecord {
        jail_name:        req.jail_name.clone(),
        volume_name:      req.volume.name.clone(),
        backend:          backend.name.clone(),
        backend_kind:     plugin.kind().to_string(),
        host_path:        host_path.clone(),
        mount_at:         req.volume.mount_at.clone(),
        allocated_at_unix: now_unix(),
        mode:             req.volume.mode,
        owner_uid:        req.volume.owner_uid,
        owner_gid:        req.volume.owner_gid,
        size_max:         req.volume.size_max,
    });
    if let Err(e) = state.save(state_path) {
        warn!("state save after provision: {e}");
    }

    info!("provisioned {}/{} on backend {} ({}) → {}",
        req.jail_name, req.volume.name, backend.name, plugin.kind(), host_path);
    Ok(Response::Provisioned { host_path })
}

fn handle_destroy(
    req:        &DestroyRequest,
    policy:     &Policy,
    state:      &mut State,
    state_path: &Path,
) -> Result<Response, VolumesError> {
    if !req.really_yes {
        return Err(VolumesError::PolicyViolation {
            rule:   "destroy.requires_really_yes",
            detail: "Destroy requires `really_yes: true` to confirm data deletion".into(),
        });
    }

    let record_idx = state.volumes.iter()
        .position(|v| v.jail_name == req.jail_name && v.volume_name == req.volume);
    let idx = match record_idx {
        Some(i) => i,
        None    => return Ok(Response::Destroyed),  // idempotent: already gone
    };

    let host_path    = state.volumes[idx].host_path.clone();
    let backend_name = state.volumes[idx].backend.clone();

    let backend = policy.resolve(Some(&backend_name)).ok_or_else(|| {
        VolumesError::PolicyViolation {
            rule:   "destroy.backend_no_longer_configured",
            detail: format!(
                "volume's recorded backend {backend_name:?} is no longer in policy"),
        }
    })?;
    let plugin = plugin_for(backend.kind).ok_or_else(|| {
        VolumesError::PolicyViolation {
            rule:   "destroy.backend_not_implemented",
            detail: format!("backend kind {:?} not implemented", backend.kind),
        }
    })?;

    plugin.destroy(backend, &host_path)?;
    state.remove(&req.jail_name, &req.volume);
    if let Err(e) = state.save(state_path) {
        warn!("state save after destroy: {e}");
    }
    info!("destroyed {}/{} (was {})", req.jail_name, req.volume, host_path);
    Ok(Response::Destroyed)
}

fn err_to_resp(e: VolumesError) -> Response {
    match e {
        VolumesError::PolicyViolation { rule, detail } => Response::PolicyDenied {
            rule: rule.into(), detail,
        },
        VolumesError::BackendUnavailable { name, configured } => {
            Response::BackendUnavailable { name, configured }
        }
        VolumesError::BackendDoesNotSupport { feature, .. } => {
            Response::BackendDoesNotSupport { feature: feature.into() }
        }
        other => Response::Error { detail: format!("{other}") },
    }
}

fn send(fd: i32, resp: &Response) -> Result<(), VolumesError> {
    let body = serde_json::to_vec(resp)?;
    /* Direct write on the socket fd via a duped owned stream;
     * we don't need the cmsg mechanics jaild has, since
     * atrium-volumes never passes fds. */
    let mut tmp = ffi::dup_to_stream(fd);
    protocol::write_frame(&mut tmp, &body)?;
    Ok(())
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Bind the listener at `socket_path` (mode 0600). Same shape
/// as jaild's helper.
pub fn bind(socket_path: &Path) -> Result<UnixListener, VolumesError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;
    Ok(listener)
}
