//! Serve loop + request dispatcher. Same shape as jaild's: one thread,
//! every connection multiplexed over kqueue (sockmux), peer-uid root-only,
//! length-prefixed JSON.
//!
//! ★ It used to serve each connection to completion before accepting the
//! next, the loop that made jaild starve portcullisd-daemon behind the
//! bootstrap's long-lived connection. Any client holding an atrium-volumes
//! connection open would have done the same here.

use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use sockmux::{LengthPrefixed, Mux};

use log::{info, warn};

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

    let mut mux: Mux<LengthPrefixed> = Mux::new(listener.try_clone()?)?;
    let mut admit = |stream: UnixStream| -> Option<LengthPrefixed> {
        let peer_uid = ffi::getpeereid(stream.as_raw_fd()).unwrap_or(u32::MAX);
        if peer_uid != 0 {
            warn!("non-root peer uid={peer_uid}; refusing");
            if let Ok(body) = serde_json::to_vec(&Response::Error {
                detail: "non-root peer".into(),
            }) {
                let mut s = stream;
                let _ = protocol::write_frame(&mut s, &body);
            }
            return None;
        }
        Some(LengthPrefixed::new(stream, protocol::MAX_FRAME_BYTES))
    };

    loop {
        /* One request per connection per round. */
        for fd in mux.next_round(&mut admit)? {
            let Some(conn) = mux.session_mut(fd) else { continue };
            let served = match conn.take_frame() {
                Ok(Some(body)) => serve_request(conn.stream(), &body, policy,
                    &mut state, state_path),
                Ok(None) => Ok(()),
                Err(e) => Err(VolumesError::Io(e)),
            };
            if let Err(e) = served {
                warn!("connection closed with error: {e}");
                mux.close(fd);
            }
        }
    }
}

/// Decode one request, dispatch it, and send the reply.
fn serve_request(
    stream:     &UnixStream,
    body:       &[u8],
    policy:     &Policy,
    state:      &mut State,
    state_path: &Path,
) -> Result<(), VolumesError> {
    let req: Request = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return send(stream, &Response::Error {
                detail: format!("malformed: {e}"),
            });
        }
    };
    let resp = dispatch(req, policy, state, state_path);
    send(stream, &resp)
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

fn send(stream: &UnixStream, resp: &Response) -> Result<(), VolumesError> {
    let body = serde_json::to_vec(resp)?;
    /* atrium-volumes never passes fds, so a plain framed write. The socket
     * is blocking here (sockmux makes it non-blocking only while reading),
     * bounded by sockmux::SEND_TIMEOUT. */
    protocol::write_frame(stream, &body)?;
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
