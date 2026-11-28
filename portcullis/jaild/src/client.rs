//! Synchronous Rust client for `jaild`'s wire protocol.
//!
//! Speaks the length-prefixed JSON the broker accepts (see
//! `jaild::protocol`). On a `CreateJail` response that carries a
//! procdesc fd via `SCM_RIGHTS`, the fd is returned alongside the
//! parsed `Response` so the caller can `EVFILT_PROCDESC` on it.
//!
//! ★ Lives in jaild's own crate, next to the protocol it speaks, because two
//! crates need it and one depends on the other: portcullisd, and
//! portcullis-oneshot (which portcullisd itself depends on). portcullisd
//! re-exports it as `portcullisd::jaild_client`, so there is one client, not
//! two to drift apart.
//!
//! Usage:
//! ```no_run
//! use jaild::client::Client;
//! use jaild::protocol::{Request, Response};
//! let mut c = Client::connect("/var/run/atrium/jaild.sock").unwrap();
//! let (resp, _fd) = c.send(&Request::Ping).unwrap();
//! assert!(matches!(resp, Response::Ok));
//! ```

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, BorrowedFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::protocol::{Request, Response, MAX_FRAME_BYTES};

/// One open connection to jaild. Cheap to construct; the unix
/// socket is established eagerly.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to a jaild socket. The peer must be jaild on
    /// the other end (root-owned mode-0600 socket); jaild
    /// `getpeereid`s us and refuses any non-root caller.
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self { stream })
    }

    /// Wrap an already-connected fd. Used at boot when the
    /// socket fd is inherited from `atrium-jaild` via the
    /// rc-script start path (eventual: `ATRIUM_JAILD_FD` env
    /// var). For the V0 client the explicit `connect()` path
    /// is the one in use.
    #[allow(dead_code)]
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Send a request, return the response. Any procdesc fd
    /// attached via SCM_RIGHTS is returned in the second slot;
    /// caller takes ownership and is responsible for closing.
    pub fn send(&mut self, req: &Request) -> io::Result<(Response, Option<i32>)> {
        let (resp, fds) = self.send_recv_fds(req, 1)?;
        Ok((resp, fds.into_iter().next()))
    }

    /// Like [`send`](Self::send) but returns up to `max_fds` SCM_RIGHTS fds
    /// in order. ExecInJail uses `max_fds = 2` for `[procdesc, pty_master]`.
    pub fn send_recv_fds(&mut self, req: &Request, max_fds: usize) -> io::Result<(Response, Vec<i32>)> {
        let body = serde_json::to_vec(req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("serialize request: {e}")))?;
        write_frame(&mut self.stream, &body)?;
        self.stream.flush()?;

        read_response(&mut self.stream, max_fds)
    }
}

impl Client {
    /// Send a request with DESCRIPTORS riding on it (SCM_RIGHTS in the same
    /// `sendmsg` as the frame, in order), then read the response and up to
    /// `max_recv` descriptors back. `ExecSpec::stdio` is the one user: the
    /// caller's stdin/stdout/stderr, and the reply's procdesc.
    ///
    /// ★ One `sendmsg` for frame and descriptors, so jaild's framing receives
    /// them WITH this request's bytes — it assigns descriptors to the frame
    /// whose bytes they arrived with, and a separate send could not promise
    /// that.
    pub fn send_with_fds(&mut self, req: &Request, fds: &[BorrowedFd<'_>], max_recv: usize)
        -> io::Result<(Response, Vec<i32>)>
    {
        let body = serde_json::to_vec(req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("serialize request: {e}")))?;
        if body.len() as u64 > MAX_FRAME_BYTES as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("outbound frame too large: {}", body.len())));
        }
        let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
        crate::ffi::send_frame_with_fds(self.stream.as_raw_fd(), &body, &raw)?;
        read_response(&mut self.stream, max_recv)
    }
}

fn write_frame<W: Write>(mut w: W, body: &[u8]) -> io::Result<()> {
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("outbound frame too large: {}", body.len())));
    }
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(body)?;
    Ok(())
}

/// Descriptors ride on the first byte of the length prefix. Keep them owned
/// until the complete response has been read and decoded.
fn read_response(stream: &mut UnixStream, max_fds: usize) -> io::Result<(Response, Vec<i32>)> {
    let mut prefix = [0u8; 4];
    let received = atrium_fdpass::recv(stream.as_raw_fd(), &mut prefix, max_fds)?;
    stream.read_exact(&mut prefix[received.n..])?;
    let len = u32::from_le_bytes(prefix);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("inbound frame too large: {len}")));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    let response = serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse response: {e}")))?;
    Ok((response, received.fds.into_iter().map(IntoRawFd::into_raw_fd).collect()))
}

/// The jail(8) lane's network (network.md §0): ask jaild — the one allocator —
/// for a point-to-point epair for `jail_name` carrying `mac`. Returns
/// `(epair_b, app_addr, host_addr)`; the caller hands `epair_b` to jail(8) as
/// `vnet.interface` and MUST call [`release_net`] when the jail is gone.
pub fn allocate_net(socket: &Path, jail_name: &str, mac: &str,
                    grants: Option<crate::protocol::NetGrants>)
    -> Result<(String, String, String), String>
{
    allocate_net_in(socket, jail_name, mac, grants, None)
}

/// [`allocate_net`] with the host end in routing table `fib` (a persona's
/// route domain, vpn.md §5.3).
pub fn allocate_net_in(socket: &Path, jail_name: &str, mac: &str,
                       grants: Option<crate::protocol::NetGrants>, fib: Option<u32>)
    -> Result<(String, String, String), String>
{
    let mut c = Client::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    match c.send(&Request::AllocateNet { jail_name: jail_name.into(), mac: mac.into(), grants, fib }) {
        Ok((Response::NetAllocated { epair_b, app_addr, host_addr }, _)) => Ok((epair_b, app_addr, host_addr)),
        Ok((Response::PolicyDenied { rule, detail }, _)) => Err(format!("jaild refused ({rule}): {detail}")),
        Ok((other, _)) => Err(format!("jaild: unexpected reply {other:?}")),
        Err(e) => Err(format!("jaild: {e}")),
    }
}

/// Release what [`allocate_net`] made. Best-effort and idempotent: a failure
/// leaves an epair jaild's startup reconcile will reclaim, never a live jail.
pub fn release_net(socket: &Path, jail_name: &str) {
    if let Ok(mut c) = Client::connect(socket) {
        let _ = c.send(&Request::ReleaseNet { jail_name: jail_name.into() });
    }
}
