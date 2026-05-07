//! Wire protocol — length-prefixed JSON, mirrors jaild's shape.
//!
//! Spec: `docs/spec/atrium-volumes.md` §3.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

use crate::VolumesError;

pub const MAX_FRAME_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Provision(ProvisionRequest),
    Destroy(DestroyRequest),
    Status(StatusRequest),
    ListBackends,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvisionRequest {
    pub jail_name: String,
    pub volume:    VolumeSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeSpec {
    pub name:      String,
    pub kind:      VolumeKind,
    /// Operator-configured backend name; `None` = use the
    /// default-marked backend.
    #[serde(default)]
    pub backend:   Option<String>,
    pub mount_at:  String,
    pub mode:      u32,
    pub owner_uid: u32,
    pub owner_gid: u32,
    #[serde(default)]
    pub size_max:  Option<u64>,
    /// For `kind = Cas`: the CAS root reference. Ignored on
    /// other kinds.
    #[serde(default)]
    pub cas_root:  Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKind {
    Persistent,
    Cas,
    Tmpfs,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DestroyRequest {
    pub jail_name: String,
    pub volume:    String,
    /// Without this set true, atrium-volumes refuses; data is
    /// preserved by default per `storage.md` principle.
    #[serde(default)]
    pub really_yes: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatusRequest {
    /// `None` = all jails.
    #[serde(default)]
    pub jail_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,

    Provisioned        { host_path: String },
    AlreadyProvisioned { host_path: String },
    Destroyed,

    Status   { volumes: Vec<VolumeRecord> },
    Backends { backends: Vec<BackendInfo> },

    BackendUnavailable    { name: String, configured: Vec<String> },
    BackendDoesNotSupport { feature: String },

    PolicyDenied { rule: String, detail: String },
    Error        { detail: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VolumeRecord {
    pub jail_name:        String,
    pub volume_name:      String,
    pub backend:          String,
    pub backend_kind:     String,
    pub host_path:        String,
    pub mount_at:         String,
    pub allocated_at_unix: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BackendInfo {
    pub name:     String,
    pub kind:     String,
    pub default:  bool,
    pub features: Vec<String>,
}

// -----------------------------------------------------------------
// Length-prefixed framing (same as jaild).
// -----------------------------------------------------------------

pub fn read_frame<R: Read>(mut r: R) -> Result<Option<Vec<u8>>, VolumesError> {
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf)? {
        0 => return Ok(None),
        n if n < 4 => r.read_exact(&mut len_buf[n..])?,
        _ => {}
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(VolumesError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} > {MAX_FRAME_BYTES}"),
        )));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn write_frame<W: Write>(mut w: W, body: &[u8]) -> Result<(), VolumesError> {
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(VolumesError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("outbound frame too large: {}", body.len()),
        )));
    }
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_round_trip() {
        let req = Request::Ping;
        let body = serde_json::to_vec(&req).unwrap();
        let mut buf = Vec::new();
        write_frame(&mut buf, &body).unwrap();
        let mut r = std::io::Cursor::new(&buf);
        let got = read_frame(&mut r).unwrap().expect("frame");
        let _: Request = serde_json::from_slice(&got).unwrap();
    }

    #[test]
    fn provision_request_serde() {
        let req = Request::Provision(ProvisionRequest {
            jail_name: "mysqld".into(),
            volume: VolumeSpec {
                name:      "data".into(),
                kind:      VolumeKind::Persistent,
                backend:   Some("fast-db".into()),
                mount_at:  "/var/db/mysql".into(),
                mode:      0o700,
                owner_uid: 88,
                owner_gid: 88,
                size_max:  Some(100 * 1024 * 1024 * 1024),
                cas_root:  None,
            },
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: Request = serde_json::from_slice(&bytes).unwrap();
        match back {
            Request::Provision(p) => {
                assert_eq!(p.jail_name, "mysqld");
                assert_eq!(p.volume.name, "data");
            }
            _ => panic!("wrong variant"),
        }
    }
}
