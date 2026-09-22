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
//! The `unsafe` is confined to the small `raw` module below — the
//! `recvmsg(2)` that extracts ancillary fds.
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
use std::os::unix::io::{AsRawFd, BorrowedFd};
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

        let (frame, fds) = recvmsg_frame(self.stream.as_raw_fd(), max_fds)?;
        let resp: Response = serde_json::from_slice(&frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("parse response: {e}")))?;
        Ok((resp, fds))
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
        let (frame, got) = recvmsg_frame(self.stream.as_raw_fd(), max_recv)?;
        let resp: Response = serde_json::from_slice(&frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                format!("parse response: {e}")))?;
        Ok((resp, got))
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

/// Read one length-prefixed frame plus optional SCM_RIGHTS fd
/// in a single `recvmsg(2)`. The body and the cmsg arrive
/// atomically because jaild sends them in the same `sendmsg`.
fn recvmsg_frame(socket_fd: i32, max_fds: usize) -> io::Result<(Vec<u8>, Vec<i32>)> {
    /* Read the 4-byte length prefix in a recvmsg call so any
     * SCM_RIGHTS cmsg (which is associated with the FIRST
     * recv on the socket since the server's sendmsg) arrives
     * here. Subsequent body reads are plain `read`. */
    let mut len_buf = [0u8; 4];
    let fds = recvmsg_with_fds(socket_fd, &mut len_buf, max_fds)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("inbound frame too large: {len}")));
    }
    let mut body = vec![0u8; len as usize];
    /* Read the body via plain reads on a duplicate of the
     * stream — it's already a unix socket, regular read works. */
    let mut tmp = unsafe_socket_reader(socket_fd);
    tmp.read_exact(&mut body)?;
    Ok((body, fds))
}

/* The unsafe is localised to two small helpers below: the cmsg
 * extraction (which has to use libc::recvmsg directly) and a
 * dup-based reader to satisfy `read_exact` after we already used
 * recvmsg for the first 4 bytes.
 *
 * These are intentionally narrow — the Read/Write API surface
 * stays safe. */
mod raw {
    #![allow(unsafe_code)]
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::os::unix::io::FromRawFd;

    /// recvmsg on `socket_fd` to read up to `out.len()` bytes AND up to
    /// `max_fds` SCM_RIGHTS fds attached. Returns every fd in the cmsg's
    /// SCM_RIGHTS array (CreateJail sends 1 = [procdesc]; ExecInJail sends
    /// 2 = [procdesc, pty_master]). `out` is filled exactly.
    pub fn recvmsg_with_fds(socket_fd: i32, out: &mut [u8], max_fds: usize) -> io::Result<Vec<i32>> {
        let mut iov = libc::iovec {
            iov_base: out.as_mut_ptr() as *mut _,
            iov_len:  out.len(),
        };

        let intsz = std::mem::size_of::<libc::c_int>();
        // SAFETY: CMSG_SPACE is a libc inline; safe. Size for max_fds ints.
        let cmsg_space = unsafe { libc::CMSG_SPACE((max_fds * intsz) as u32) };
        let mut cmsg_buf: Vec<u8> = vec![0u8; cmsg_space as usize];

        let mut msg = libc::msghdr {
            msg_name:        std::ptr::null_mut(),
            msg_namelen:     0,
            msg_iov:         &mut iov,
            msg_iovlen:      1,
            msg_control:     cmsg_buf.as_mut_ptr() as *mut _,
            msg_controllen:  cmsg_space as _,
            msg_flags:       0,
        };

        // SAFETY: msg fields all initialised; iov + cmsg buffers outlive
        // the recvmsg call. Output bytes go into `out` (caller-owned); fds
        // in the cmsg are read out as values — no aliasing.
        // ★ CLOSE-ON-EXEC FROM RECEIPT. The fds here are procdescs (and a pty
        // master). A caller like portcullisd serves many one-shots at once and
        // spawns mount/umount/jls for each; a procdesc received without this
        // is inherited by every one of those children, and a zombie is not
        // reaped until its LAST descriptor closes — so a worker's exit would
        // wait on whatever unrelated command happened to fork meanwhile.
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
        let flags = 0;
        let n = unsafe { libc::recvmsg(socket_fd, &mut msg, flags) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if (n as usize) < out.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof,
                format!("short recvmsg: got {} of {}", n, out.len())));
        }

        // Collect every fd across SCM_RIGHTS cmsgs (one cmsg can carry an
        // array of fds; count = payload_len / sizeof(int)).
        let mut fds: Vec<i32> = Vec::new();
        // SAFETY: cmsg_buf is valid + correctly sized; CMSG_FIRSTHDR returns
        // null on empty cmsg, which we handle.
        unsafe {
            let cmsg_len0 = libc::CMSG_LEN(0) as usize;
            let mut cmsg_ptr = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg_ptr.is_null() {
                let cmsg = &*cmsg_ptr;
                if cmsg.cmsg_level == libc::SOL_SOCKET
                    && cmsg.cmsg_type == libc::SCM_RIGHTS
                {
                    let payload = (cmsg.cmsg_len as usize).saturating_sub(cmsg_len0);
                    let count = payload / intsz;
                    let data = libc::CMSG_DATA(cmsg_ptr) as *const libc::c_int;
                    for i in 0..count {
                        fds.push(data.add(i).read_unaligned());
                    }
                }
                cmsg_ptr = libc::CMSG_NXTHDR(&msg, cmsg_ptr);
            }
        }
        Ok(fds)
    }

    /// Duplicate `socket_fd` into a `UnixStream` for later reads.
    /// Caller must NOT close the original; both fds reference the
    /// same socket and either close ends both sides.
    pub fn dup_to_stream(socket_fd: i32) -> UnixStream {
        // SAFETY: `dup` returns a fresh fd pointing at the same
        // open file table entry. Wrapping it in UnixStream gives
        // safe access; std drops the dup'd fd when the stream
        // drops. The original fd remains valid in the caller.
        unsafe {
            let dup = libc::dup(socket_fd);
            assert!(dup >= 0, "dup failed");
            UnixStream::from_raw_fd(dup)
        }
    }
}

fn recvmsg_with_fds(socket_fd: i32, out: &mut [u8], max_fds: usize) -> io::Result<Vec<i32>> {
    raw::recvmsg_with_fds(socket_fd, out, max_fds)
}

fn unsafe_socket_reader(socket_fd: i32) -> std::os::unix::net::UnixStream {
    raw::dup_to_stream(socket_fd)
}

/// The jail(8) lane's network (network.md §0): ask jaild — the one allocator —
/// for a point-to-point epair for `jail_name` carrying `mac`. Returns
/// `(epair_b, app_addr, host_addr)`; the caller hands `epair_b` to jail(8) as
/// `vnet.interface` and MUST call [`release_net`] when the jail is gone.
pub fn allocate_net(socket: &Path, jail_name: &str, mac: &str,
                    grants: Option<crate::protocol::NetGrants>)
    -> Result<(String, String, String), String>
{
    let mut c = Client::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    match c.send(&Request::AllocateNet { jail_name: jail_name.into(), mac: mac.into(), grants }) {
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
