//! portcullis-ipc — wire protocol between `portcullis` CLI and
//! `portcullisd`. Newline-delimited JSON over a Unix-domain socket
//! at `SOCKET_PATH`. Each line is one `Request` from the client or
//! one `Response` from the daemon; framing is "until the next \n".
//!
//! JSON (vs bincode/protobuf) chosen so the protocol is debuggable
//! with `nc -U` and unimportant from a perf perspective — request
//! rate is "human-driven app launches", not anything hot.
//!
//! Versioning: every `Request` and `Response` is tagged with its
//! variant name via `#[serde(tag = "op")]` / `#[serde(tag = "kind")]`,
//! so adding new variants is forward-compatible (older daemons reply
//! `UnknownOp`; older clients ignore unknown response kinds).

use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use portcullis_toml::Capabilities;

/// Default socket path. Created by portcullisd at startup; mode 0600
/// so only the owning user can connect (per-user policy oracle).
pub const SOCKET_PATH: &str = "/var/run/portcullisd.sock";

/// Protocol version — bumped if a wire-incompatible change lands.
/// Clients send their version in `Request::Hello`; mismatched versions
/// get `Response::ProtoMismatch` and the daemon closes the connection.
pub const PROTO_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Handshake; must be the first message on a fresh connection.
    Hello { version: u32 },
    /// Cheap liveness probe.
    Ping,
    /// "Can app `app_id` (with this manifest hash and these caps)
    /// launch right now?" Daemon returns `Authorized` if the policy
    /// already covers everything, `NeedsApproval` with the delta
    /// otherwise.
    Authorize {
        app_id:        String,
        manifest_hash: String,
        requested:     Capabilities,
    },
    /// "Persist a grant for these capabilities." Used by both the
    /// CLI bootstrap (`policy grant`) and, in the future, the
    /// prompt UI after the user clicks Allow.
    Grant {
        app_id:        String,
        manifest_hash: String,
        capabilities:  Capabilities,
    },
    /// Drop a previously persisted grant.
    Revoke { app_id: String },
    /// Re-read policy.toml from disk (for after manual edits).
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Handshake reply; daemon advertises its proto version.
    Hello { version: u32 },
    Pong,
    /// Authorize → all requested caps already granted.
    Authorized,
    /// Authorize → needs user approval. `delta` is human-readable
    /// reason lines (suitable for a prompt UI or CLI message).
    NeedsApproval { delta: Vec<String> },
    /// Grant or Revoke → success.
    Ok,
    /// Generic error reply with a message.
    Error { message: String },
    /// Wire-version mismatch; client should disconnect.
    ProtoMismatch { server_version: u32 },
    /// Daemon doesn't recognize this op (forward-compat).
    UnknownOp,
}

/// Send one request and read one response over a connected stream.
/// Convenience for synchronous client code.
pub fn round_trip(stream: &mut UnixStream, req: &Request) -> io::Result<Response> {
    write_request(stream, req)?;
    read_response(stream)
}

pub fn write_request<W: Write>(w: &mut W, req: &Request) -> io::Result<()> {
    let mut line = serde_json::to_vec(req).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> io::Result<()> {
    let mut line = serde_json::to_vec(resp).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Request> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed"));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                 format!("bad request frame: {e}")))
}

pub fn read_response<R: Read>(r: &mut R) -> io::Result<Response> {
    let mut br = io::BufReader::new(r);
    let mut line = String::new();
    let n = br.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed"));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                 format!("bad response frame: {e}")))
}

use std::io::Read;

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_toml::NetworkCap;

    #[test]
    fn roundtrip_authorize_request() {
        let mut caps = Capabilities::default();
        caps.clipboard = Some(true);
        caps.network   = Some(NetworkCap::Loopback);
        let req = Request::Authorize {
            app_id:        "org.atrium.edit".into(),
            manifest_hash: "sha256:abc".into(),
            requested:     caps,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"authorize\""));
        let back: Request = serde_json::from_str(&s).unwrap();
        match back {
            Request::Authorize { app_id, .. } => assert_eq!(app_id, "org.atrium.edit"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_needs_approval_response() {
        let r = Response::NeedsApproval {
            delta: vec!["Use the fresco graphics service".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back {
            Response::NeedsApproval { delta } => assert_eq!(delta.len(), 1),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn newline_framing_via_pipe() {
        use std::io::Cursor;
        let req1 = Request::Ping;
        let req2 = Request::Reload;
        let mut buf = Vec::new();
        write_request(&mut buf, &req1).unwrap();
        write_request(&mut buf, &req2).unwrap();
        let mut br = io::BufReader::new(Cursor::new(buf));
        let r1 = read_request(&mut br).unwrap();
        let r2 = read_request(&mut br).unwrap();
        matches!(r1, Request::Ping);
        matches!(r2, Request::Reload);
    }
}
