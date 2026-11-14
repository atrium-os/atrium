//! `Connection` — per-process socket + CAS cache + send/recv plumbing.
//!
//! Wraps a `UnixStream`, gives services a higher-level API:
//!
//!  * `send_message` — push one envelope-tagged message.
//!  * `recv_message` — pull the next message; auto-handles CAS-layer
//!    traffic (UPLOAD_BEGIN/DATA/FINISH, ACKs) so the caller only
//!    sees their own opcode_class messages.
//!  * `fill_available` / `has_buffered_message` / `try_recv_message` —
//!    the same, for a server multiplexing many connections over kqueue:
//!    read what is there without blocking, then serve complete messages
//!    from the buffer.
//!  * `upload_blob` / `fetch_blob` — convenience for CAS payload
//!    handoff.
//!  * `set_cache_cap` — cap RAM used by the per-connection blob
//!    cache.
//!  * `send_message_with_fds` / `take_fds` — pass file descriptors
//!    (shared memory, spec §4.2) along with a message.
//!
//! Verify-on-use is unconditional: every cache read re-hashes the
//! bytes and rejects on mismatch.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufWriter, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::cas::{self, Hash};
use crate::classes::CLASS_CORE;
use crate::envelope::{self, flag, Header, MAX_PAYLOAD};

/// Default per-connection blob cache cap. Caller may shrink with
/// `set_cache_cap`. 8 MiB is plenty for icons, manifests, and
/// config-shaped blobs; bulk content should ride shm/fd-pass.
pub const DEFAULT_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Most fds one message may carry.
pub const MAX_FDS_PER_MESSAGE: usize = 16;
/// Most received fds a connection holds before the caller takes them;
/// past this, more are closed on arrival (a peer can't exhaust our fds).
pub const MAX_QUEUED_FDS: usize = 64;

/// What the caller asked recv_message for. Matches the envelope's
/// flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response,
    Event,
}

/// A complete decoded message ready for service-level dispatch.
#[derive(Debug, Clone)]
pub struct Message {
    pub opcode_class: u8,
    pub op:           u16,
    pub flags:        u16,
    pub kind:         MessageKind,
    pub payload:      Vec<u8>,
}

impl Message {
    pub fn has_hash_refs(&self) -> bool {
        self.flags & flag::HAS_HASH_REFS != 0
    }
}

/// Per-blob cache entry. We hold the full bytes (cache hit avoids
/// any re-fetch). For enormous blobs the caller should use shm
/// rendezvous, not the message envelope, so this fits in RAM.
#[derive(Debug)]
struct CacheEntry {
    bytes: Vec<u8>,
    /// Wall-clock of last access; LRU eviction key.
    last_use: Instant,
}

pub struct Connection {
    rd:        UnixStream,
    /// Bytes read from `rd` but not yet decoded into envelopes. Every
    /// receive path decodes from here, so blocking and non-blocking
    /// receives can be mixed on one connection, and a read timeout
    /// mid-message loses nothing.
    inbuf:     Vec<u8>,
    wr:        BufWriter<UnixStream>,
    cache:     HashMap<Hash, CacheEntry>,
    cache_bytes:    usize,
    cache_cap:      usize,
    /// In-flight uploads — outbound state. Key is the blob's hash.
    /// Currently we publish synchronously, so this is mostly empty;
    /// kept for future pipelined uploads.
    _outbound_uploads: HashMap<Hash, ()>,
    /// In-progress incoming uploads — receiver state. Keyed by hash.
    incoming_uploads:  HashMap<Hash, IncomingUpload>,
    /// Fds received with messages, in arrival order, until `take_fds`.
    infds: VecDeque<OwnedFd>,
    /// Service messages that arrived while an upload waited for its ACK
    /// (events from a busy peer): delivered next, in order.
    held: VecDeque<(Header, Vec<u8>)>,
    /// While corked, `send_message` only buffers: a frame of many small
    /// messages goes out in a few writes instead of one per message.
    corked: bool,
}

#[derive(Debug)]
struct IncomingUpload {
    total_size: u64,
    bytes:      Vec<u8>,
}

impl AsRawFd for Connection {
    /// The underlying socket fd, for kqueue / EVFILT_READ
    /// registration when the caller multiplexes events with other
    /// fds.
    fn as_raw_fd(&self) -> RawFd { self.rd.as_raw_fd() }
}

impl Connection {
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let s = UnixStream::connect(path)?;
        Self::wrap(s)
    }

    pub fn wrap(s: UnixStream) -> io::Result<Self> {
        let rd = s.try_clone()?;
        let wr = BufWriter::with_capacity(256 * 1024, s);
        Ok(Self {
            rd, wr,
            inbuf: Vec::new(),
            cache: HashMap::new(),
            cache_bytes: 0,
            cache_cap:   DEFAULT_CACHE_BYTES,
            _outbound_uploads: HashMap::new(),
            incoming_uploads:  HashMap::new(),
            infds: VecDeque::new(),
            held: VecDeque::new(),
            corked: false,
        })
    }

    pub fn set_cache_cap(&mut self, bytes: usize) {
        self.cache_cap = bytes;
        self.evict_until_fits();
    }

    /// Send a complete service-level message. Caller chooses
    /// flags (RESPONSE_EXPECTED, IS_RESPONSE, ASYNC_EVENT, etc.)
    /// and provides the marshalled payload.
    pub fn send_message(
        &mut self,
        opcode_class: u8,
        op: u16,
        flags: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() as u64 > MAX_PAYLOAD as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("payload {} > MAX_PAYLOAD {}",
                    payload.len(), MAX_PAYLOAD),
            ));
        }
        let h = Header::new(opcode_class, op, flags, payload.len() as u32);
        envelope::write_message(&mut self.wr, h, payload)?;
        if self.corked {
            return Ok(());
        }
        self.wr.flush()
    }

    /// Buffer what is sent until [`Connection::uncork`] (or a read, which
    /// sends what is buffered first, so a request never waits on its own
    /// reply).
    pub fn cork(&mut self) {
        self.corked = true;
    }

    /// Send everything buffered, and send each message at once again.
    pub fn uncork(&mut self) -> io::Result<()> {
        self.corked = false;
        self.wr.flush()
    }

    /// Send what is buffered now. Call it before making the socket
    /// non-blocking (the write side shares the descriptor's mode).
    pub fn flush(&mut self) -> io::Result<()> {
        self.wr.flush()
    }

    /// Send a message with `fds` attached (at most
    /// [`MAX_FDS_PER_MESSAGE`]). They arrive with the message's first
    /// byte, so by the time the receiver has the whole message they are
    /// in its queue for [`Connection::take_fds`]. The payload should say
    /// how many there are.
    pub fn send_message_with_fds(
        &mut self,
        opcode_class: u8,
        op: u16,
        flags: u16,
        payload: &[u8],
        fds: &[RawFd],
    ) -> io::Result<()> {
        if fds.is_empty() {
            return self.send_message(opcode_class, op, flags, payload);
        }
        if fds.len() > MAX_FDS_PER_MESSAGE || payload.len() as u64 > MAX_PAYLOAD as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many fds or payload too big"));
        }
        self.wr.flush()?;
        let h = Header::new(opcode_class, op, flags, payload.len() as u32);
        let mut bytes = h.encode().to_vec();
        bytes.extend_from_slice(payload);
        let sent = send_with_fds(self.wr.get_ref(), &bytes, fds)?;
        self.wr.get_mut().write_all(&bytes[sent..])
    }

    /// Take `n` received fds, oldest first; `None` (and none taken) if
    /// fewer than `n` have arrived.
    pub fn take_fds(&mut self, n: usize) -> Option<Vec<OwnedFd>> {
        (self.infds.len() >= n).then(|| self.infds.drain(..n).collect())
    }

    /// Read from the socket into `chunk`, queueing any fds that came along.
    fn read_chunk(&mut self, chunk: &mut [u8]) -> io::Result<usize> {
        if !self.wr.buffer().is_empty() {
            self.wr.flush()?;
        }
        let (n, fds) = recv_with_fds(&self.rd, chunk)?;
        for fd in fds {
            if self.infds.len() < MAX_QUEUED_FDS {
                self.infds.push_back(fd);
            }
        }
        Ok(n)
    }

    /// Pull the next service-level message. Auto-handles
    /// CLASS_CORE traffic in the background (cache loading, ACKs).
    /// Caller-visible messages are everything in their own service
    /// class, plus FETCH_REQUEST (which the caller must answer by
    /// re-uploading) and EVICT_HINT (advisory).
    pub fn recv_message(&mut self) -> io::Result<Message> {
        loop {
            let (h, payload) = self.read_envelope()?;
            if let Some(m) = self.deliver(h, payload)? {
                return Ok(m);
            }
        }
    }

    /// Read whatever the socket has right now into the receive buffer.
    /// For a server multiplexing connections: call it when kqueue reports
    /// the fd readable, with the socket set non-blocking
    /// (`set_nonblocking(true)`). Returns `Ok(false)` once the peer has
    /// closed. Stops early once a complete message is buffered — the rest
    /// stays in the socket, and a level-triggered kqueue reports it again.
    pub fn fill_available(&mut self) -> io::Result<bool> {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if self.has_buffered_message() {
                return Ok(true);
            }
            match self.read_chunk(&mut chunk) {
                Ok(0) => return Ok(false),
                Ok(n) => self.inbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Whether a complete envelope is buffered — or a malformed header,
    /// which `try_recv_message` will report. Does not read the socket.
    pub fn has_buffered_message(&self) -> bool {
        !self.held.is_empty() || !matches!(self.buffered_envelope_len(), Ok(None))
    }

    /// Serve from the receive buffer only: decode buffered envelopes,
    /// handling CLASS_CORE traffic as `recv_message` does, and return the
    /// first caller-visible message. `Ok(None)` when the buffer holds no
    /// complete caller-visible message. Never reads the socket; any reply
    /// the substrate sends (e.g. UPLOAD_ACK) is written normally, so call
    /// it with the socket blocking.
    pub fn try_recv_message(&mut self) -> io::Result<Option<Message>> {
        while let Some((h, payload)) = self.pop_envelope()? {
            if let Some(m) = self.deliver(h, payload)? {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    /// Length (header + payload) of the first buffered envelope, if complete.
    fn buffered_envelope_len(&self) -> io::Result<Option<usize>> {
        if self.inbuf.len() < envelope::HEADER_LEN {
            return Ok(None);
        }
        let h = Header::decode(&self.inbuf[..envelope::HEADER_LEN])?;
        let total = envelope::HEADER_LEN + h.length as usize;
        Ok((self.inbuf.len() >= total).then_some(total))
    }

    fn pop_envelope(&mut self) -> io::Result<Option<(Header, Vec<u8>)>> {
        if let Some(env) = self.held.pop_front() {
            return Ok(Some(env));
        }
        let Some(total) = self.buffered_envelope_len()? else { return Ok(None) };
        let h = Header::decode(&self.inbuf[..envelope::HEADER_LEN])?;
        let payload = self.inbuf[envelope::HEADER_LEN..total].to_vec();
        self.inbuf.drain(..total);
        Ok(Some((h, payload)))
    }

    /// Next envelope, reading (and blocking, per the socket's mode and
    /// read timeout) until one is complete. EOF before a complete envelope
    /// is `UnexpectedEof`, as `read_exact` reported it.
    fn read_envelope(&mut self) -> io::Result<(Header, Vec<u8>)> {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if let Some(env) = self.pop_envelope()? {
                return Ok(env);
            }
            match self.read_chunk(&mut chunk) {
                Ok(0) => return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof, "connection closed")),
                Ok(n) => self.inbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Run CLASS_CORE handling on one envelope; return the message if the
    /// caller should see it.
    fn deliver(&mut self, h: Header, payload: Vec<u8>) -> io::Result<Option<Message>> {
        if h.opcode_class == CLASS_CORE && self.handle_core(h.op, &payload)? {
            return Ok(None); /* swallowed at the substrate layer */
        }
        let kind = if h.flags & flag::IS_RESPONSE != 0 {
            MessageKind::Response
        } else if h.flags & flag::ASYNC_EVENT != 0 {
            MessageKind::Event
        } else {
            MessageKind::Request
        };
        Ok(Some(Message {
            opcode_class: h.opcode_class,
            op:           h.op,
            flags:        h.flags,
            kind,
            payload,
        }))
    }

    /// Handle a CLASS_CORE op. Returns true if the message was
    /// fully handled and the caller should not see it; false if
    /// the caller wanted to see it (e.g., a FETCH_REQUEST they
    /// must answer).
    fn handle_core(&mut self, op: u16, payload: &[u8]) -> io::Result<bool> {
        match op {
            cas::op::UPLOAD_BEGIN => {
                let ub = cas::UploadBegin::decode(payload)
                    .map_err(io_invalid)?;
                if ub.total_size > MAX_PAYLOAD as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "upload total_size exceeds MAX_PAYLOAD",
                    ));
                }
                let mut entry = IncomingUpload {
                    total_size: ub.total_size,
                    bytes: Vec::with_capacity(ub.total_size as usize),
                };
                entry.bytes.extend_from_slice(ub.inline);
                if entry.bytes.len() as u64 == ub.total_size {
                    /* fits inline — finalize immediately */
                    self.finalize_incoming(ub.hash, entry.bytes)?;
                } else {
                    self.incoming_uploads.insert(ub.hash, entry);
                }
                Ok(true)
            }
            cas::op::UPLOAD_DATA => {
                let ud = cas::UploadData::decode(payload)
                    .map_err(io_invalid)?;
                let entry = self.incoming_uploads.get_mut(&ud.hash)
                    .ok_or_else(|| io::Error::new(
                        io::ErrorKind::InvalidData,
                        "UPLOAD_DATA for unknown hash",
                    ))?;
                if ud.offset != entry.bytes.len() as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("UPLOAD_DATA offset {} expected {}",
                            ud.offset, entry.bytes.len()),
                    ));
                }
                entry.bytes.extend_from_slice(ud.data);
                Ok(true)
            }
            cas::op::UPLOAD_FINISH => {
                let claimed = cas::decode_hash_msg(payload)
                    .map_err(io_invalid)?;
                let entry = self.incoming_uploads.remove(&claimed)
                    .ok_or_else(|| io::Error::new(
                        io::ErrorKind::InvalidData,
                        "UPLOAD_FINISH for unknown hash",
                    ))?;
                if entry.bytes.len() as u64 != entry.total_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "UPLOAD_FINISH before all data received",
                    ));
                }
                self.finalize_incoming(claimed, entry.bytes)?;
                Ok(true)
            }
            cas::op::UPLOAD_ACK => {
                /* Sender side bookkeeping; nothing to do for the
                 * synchronous-upload baseline. */
                Ok(true)
            }
            cas::op::EVICT_HINT => {
                let h = cas::decode_hash_msg(payload).map_err(io_invalid)?;
                self.cache_drop(&h);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn finalize_incoming(&mut self, claimed: Hash, bytes: Vec<u8>) -> io::Result<()> {
        if !cas::verify(&bytes, &claimed) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incoming blob hash mismatch — protocol violation or corruption",
            ));
        }
        self.cache_insert(claimed, bytes);
        /* Send ACK so the sender knows we have it. */
        let payload = cas::encode_hash_msg(&claimed);
        self.send_message(CLASS_CORE, cas::op::UPLOAD_ACK, 0, &payload)
    }

    /// Upload `bytes` to the peer. Returns the SHA-256 hash.
    /// Synchronous: blocks until the peer has acknowledged
    /// (UPLOAD_ACK).
    pub fn upload_blob(&mut self, bytes: &[u8]) -> io::Result<Hash> {
        let h = cas::hash(bytes);
        self.cache_insert(h, bytes.to_vec());

        let inline = bytes.len().min(cas::MAX_INLINE_BEGIN);
        let begin = cas::UploadBegin {
            hash: h,
            total_size: bytes.len() as u64,
            inline: &bytes[..inline],
        }.encode();
        self.send_message(CLASS_CORE, cas::op::UPLOAD_BEGIN, 0, &begin)?;

        let mut sent = inline;
        while sent < bytes.len() {
            let n = (bytes.len() - sent).min(64 * 1024);
            let data = cas::UploadData {
                hash: h,
                offset: sent as u64,
                data: &bytes[sent..sent + n],
            }.encode();
            self.send_message(CLASS_CORE, cas::op::UPLOAD_DATA, 0, &data)?;
            sent += n;
        }

        if bytes.len() > cas::MAX_INLINE_BEGIN {
            let fin = cas::encode_hash_msg(&h);
            self.send_message(CLASS_CORE, cas::op::UPLOAD_FINISH, 0, &fin)?;
        }

        /* Wait for ACK. */
        let ack = self.recv_message_until_class(CLASS_CORE, cas::op::UPLOAD_ACK)?;
        if ack != h {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UPLOAD_ACK hash mismatch",
            ));
        }
        Ok(h)
    }

    /// Synchronous helper to consume from the wire until a specific
    /// CLASS_CORE op arrives. Used by the upload state machine.
    fn recv_message_until_class(&mut self, class: u8, op: u16) -> io::Result<Hash> {
        // Messages already held came before this wait: set them aside so
        // the reads below see only new envelopes, then put them first.
        let earlier = std::mem::take(&mut self.held);
        let mut late = VecDeque::new();
        let r = self.recv_until_class_inner(class, op, &mut late);
        self.held = earlier;
        self.held.extend(late);
        r
    }

    fn recv_until_class_inner(&mut self, class: u8, op: u16, late: &mut VecDeque<(Header, Vec<u8>)>) -> io::Result<Hash> {
        loop {
            let (h, payload) = self.read_envelope()?;
            if h.opcode_class == class && h.op == op {
                return cas::decode_hash_msg(&payload).map_err(io_invalid);
            }
            /* CORE traffic is handled now; service messages (a peer's
             * events, which don't wait for our upload) are held and
             * delivered next, in order. */
            if h.opcode_class == CLASS_CORE {
                let _ = self.handle_core(h.op, &payload)?;
            } else {
                late.push_back((h, payload));
            }
        }
    }

    /// Look up a blob in the local cache. Returns None on miss
    /// (caller should send FETCH_REQUEST). Verifies on use.
    pub fn cache_get(&mut self, h: &Hash) -> Option<Vec<u8>> {
        let entry = self.cache.get_mut(h)?;
        if !cas::verify(&entry.bytes, h) {
            /* Tampered or truncated; drop it. */
            self.cache.remove(h);
            return None;
        }
        entry.last_use = Instant::now();
        Some(entry.bytes.clone())
    }

    fn cache_insert(&mut self, h: Hash, bytes: Vec<u8>) {
        let len = bytes.len();
        if len > self.cache_cap {
            /* Single blob bigger than cap — don't pollute the cache. */
            return;
        }
        if let Some(old) = self.cache.remove(&h) {
            self.cache_bytes = self.cache_bytes.saturating_sub(old.bytes.len());
        }
        self.cache.insert(h, CacheEntry {
            bytes,
            last_use: Instant::now(),
        });
        self.cache_bytes += len;
        self.evict_until_fits();
    }

    fn cache_drop(&mut self, h: &Hash) {
        if let Some(e) = self.cache.remove(h) {
            self.cache_bytes = self.cache_bytes.saturating_sub(e.bytes.len());
        }
    }

    fn evict_until_fits(&mut self) {
        while self.cache_bytes > self.cache_cap {
            let victim = self.cache.iter()
                .min_by_key(|(_, v)| v.last_use)
                .map(|(k, _)| *k);
            match victim {
                Some(k) => self.cache_drop(&k),
                None => break,
            }
        }
    }

    /// Set socket read timeout. Useful for clients that want to
    /// poll with a deadline. None = block forever.
    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.rd.set_read_timeout(t)
    }

    /// Toggle non-blocking I/O. Used by clients that want a true
    /// "is there data right now?" poll without setting a sub-tick
    /// timeout (FreeBSD rejects `set_read_timeout(Duration::ZERO)`
    /// with EINVAL — `set_nonblocking(true)` is the portable way).
    pub fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.rd.set_nonblocking(nb)
    }

    /// Diagnostic: how many bytes are currently in the cache.
    pub fn cache_used_bytes(&self) -> usize { self.cache_bytes }
}

fn io_invalid(s: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::thread;

    /// Round-trip a small blob over a socketpair.
    #[test]
    fn upload_roundtrip_small() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Connection::wrap(a).unwrap();
        let mut cb = Connection::wrap(b).unwrap();

        let server = thread::spawn(move || {
            /* B is the receiver — it should auto-handle the upload
             * and place the blob in its cache, then ACK. After ACK
             * the upload_blob call on A returns. */
            /* Drive B's recv loop until cache has the blob. */
            let _ = cb.recv_message_or_timeout(Duration::from_secs(2));
            cb
        });

        let h = ca.upload_blob(b"the answer is 42").unwrap();
        assert_eq!(h, cas::hash(b"the answer is 42"));
        let cb = server.join().unwrap();
        let mut cb = cb;
        assert!(cb.cache_get(&h).is_some());
    }

    /// Events the peer sends while an upload waits for its ACK are kept,
    /// in order, and delivered after it.
    #[test]
    fn events_during_an_upload_wait_are_kept() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Connection::wrap(a).unwrap();
        let mut cb = Connection::wrap(b).unwrap();
        // One event already buffered, one sent while the upload is going.
        cb.send_message(9, 1, 0, b"first").unwrap();
        let server = thread::spawn(move || {
            cb.send_message(9, 2, 0, b"second").unwrap();
            let _ = cb.recv_message_or_timeout(Duration::from_secs(2));
            cb
        });
        let h = ca.upload_blob(b"the answer is 42").unwrap();
        assert_eq!(h, cas::hash(b"the answer is 42"));
        let _cb = server.join().unwrap();

        assert!(ca.has_buffered_message());
        let m = ca.recv_message().unwrap();
        assert_eq!((m.op, m.payload.as_slice()), (1, &b"first"[..]));
        let m = ca.recv_message().unwrap();
        assert_eq!((m.op, m.payload.as_slice()), (2, &b"second"[..]));
    }

    /// Round-trip a blob that requires DATA frames.
    #[test]
    fn upload_roundtrip_large() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Connection::wrap(a).unwrap();
        let mut cb = Connection::wrap(b).unwrap();

        let big = vec![0xa5u8; 256 * 1024]; /* > MAX_INLINE_BEGIN */
        let big_clone = big.clone();
        let server = thread::spawn(move || {
            let _ = cb.recv_message_or_timeout(Duration::from_secs(2));
            cb
        });
        let h = ca.upload_blob(&big).unwrap();
        let mut cb = server.join().unwrap();
        let got = cb.cache_get(&h).unwrap();
        assert_eq!(got, big_clone);
    }

    /// The multiplexing receive path: bytes arrive in pieces, fills never
    /// block, and a message is served only once complete.
    #[test]
    fn nonblocking_receive_serves_only_complete_messages() {
        let (mut raw, b) = UnixStream::pair().unwrap();
        let mut cb = Connection::wrap(b).unwrap();
        cb.set_nonblocking(true).unwrap();

        /* Nothing sent: a fill returns at once, nothing is ready. */
        assert!(cb.fill_available().unwrap());
        assert!(!cb.has_buffered_message());
        assert!(cb.try_recv_message().unwrap().is_none());

        /* Half a header, then the rest plus a second message in one write. */
        let m1 = envelope_bytes(40, 7, b"first");
        let m2 = envelope_bytes(40, 8, b"second");
        raw.write_all(&m1[..4]).unwrap();
        assert!(cb.fill_available().unwrap());
        assert!(!cb.has_buffered_message());

        let mut rest = m1[4..].to_vec();
        rest.extend_from_slice(&m2);
        raw.write_all(&rest).unwrap();
        assert!(cb.fill_available().unwrap());
        assert!(cb.has_buffered_message());
        let got1 = cb.try_recv_message().unwrap().unwrap();
        assert_eq!((got1.op, got1.payload.as_slice()), (7, &b"first"[..]));
        /* fill may have stopped after the first message; the second is in
         * the buffer or still in the socket. */
        let _ = cb.fill_available().unwrap();
        let got2 = cb.try_recv_message().unwrap().unwrap();
        assert_eq!((got2.op, got2.payload.as_slice()), (8, &b"second"[..]));
        assert!(cb.try_recv_message().unwrap().is_none());

        /* Peer closes: fill reports it. */
        drop(raw);
        assert!(!cb.fill_available().unwrap());
    }

    /// A message split by a read timeout is not lost: the blocking path
    /// resumes from the buffered bytes.
    #[test]
    fn blocking_receive_keeps_bytes_across_a_timeout() {
        let (mut raw, b) = UnixStream::pair().unwrap();
        let mut cb = Connection::wrap(b).unwrap();
        let m = envelope_bytes(40, 9, b"split");
        raw.write_all(&m[..6]).unwrap();
        assert!(cb.recv_message_or_timeout(Duration::from_millis(50)).unwrap().is_none());
        raw.write_all(&m[6..]).unwrap();
        let got = cb.recv_message_or_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!((got.op, got.payload.as_slice()), (9, &b"split"[..]));
    }

    fn envelope_bytes(class: u8, op: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = Header::new(class, op, 0, payload.len() as u32).encode().to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn cache_evicts_on_cap() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Connection::wrap(a).unwrap();
        let _cb = Connection::wrap(b).unwrap();
        ca.set_cache_cap(8 * 1024);
        /* Insert two 6-KiB entries; first should evict. */
        let h1 = cas::hash(b"a");
        let h2 = cas::hash(b"b");
        ca.cache_insert(h1, vec![0; 6 * 1024]);
        ca.cache_insert(h2, vec![0; 6 * 1024]);
        assert!(ca.cache.contains_key(&h2));
        assert!(!ca.cache.contains_key(&h1));
    }
}

impl Connection {
    /// Test/poll helper: receive any message but bound the wait by
    /// `timeout`. Returns the message, or None on timeout.
    pub fn recv_message_or_timeout(&mut self, timeout: Duration)
        -> io::Result<Option<Message>>
    {
        self.set_read_timeout(Some(timeout))?;
        let r = self.recv_message();
        let _ = self.set_read_timeout(None);
        match r {
            Ok(m) => Ok(Some(m)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                  || e.kind() == io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// `sendmsg` the start of `bytes` with `fds` attached; returns how many
/// bytes went (at least one, or an error).
fn send_with_fds(s: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let space = unsafe { libc::CMSG_SPACE((fds.len() * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
    // SAFETY: msghdr is plain data; every pointer in it points into
    // `iov`, `bytes` and `cbuf`, which outlive the sendmsg call, and the
    // control message is written inside `cbuf`'s CMSG_SPACE.
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &mut iov;
        m.msg_iovlen = 1;
        m.msg_control = cbuf.as_mut_ptr().cast();
        m.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&m);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN((fds.len() * std::mem::size_of::<RawFd>()) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), fds.len());
        loop {
            let n = libc::sendmsg(s.as_raw_fd(), &m, 0);
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// `recvmsg` into `buf`, returning the bytes read and any fds that came
/// with them (close-on-exec). Fds past [`MAX_FDS_PER_MESSAGE`] in one
/// control message are closed.
fn recv_with_fds(s: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS_PER_MESSAGE * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: as in send_with_fds; the kernel writes at most `space`
    // control bytes and `buf.len()` data bytes, and every fd we read out
    // of a SCM_RIGHTS message is a fresh descriptor we now own.
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &mut iov;
        m.msg_iovlen = 1;
        m.msg_control = cbuf.as_mut_ptr().cast();
        m.msg_controllen = space as _;
        // macOS has no MSG_CMSG_CLOEXEC: close-on-exec is set on each fd
        // below instead (this process doesn't fork in between).
        #[cfg(not(target_vendor = "apple"))]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(target_vendor = "apple")]
        let flags = 0;
        let n = libc::recvmsg(s.as_raw_fd(), &mut m, flags);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&m);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let len = (*c).cmsg_len as usize - (data as usize - c as usize);
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i)));
                    #[cfg(target_vendor = "apple")]
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                    if fds.len() < MAX_FDS_PER_MESSAGE {
                        fds.push(fd);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&m, c);
        }
        Ok((n as usize, fds))
    }
}

#[cfg(test)]
mod fd_tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn fds_ride_along_with_their_message() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut tx = Connection::wrap(a).unwrap();
        let mut rx = Connection::wrap(b).unwrap();
        let mut f = std::fs::File::from(tempfile_fd());
        f.write_all(b"frame").unwrap();
        tx.send_message(9, 1, 0, b"before").unwrap();
        tx.send_message_with_fds(9, 2, 0, &[1], &[f.as_raw_fd(), f.as_raw_fd()]).unwrap();
        tx.send_message(9, 3, 0, b"after").unwrap();
        assert_eq!(rx.recv_message().unwrap().op, 1);
        let m = rx.recv_message().unwrap();
        assert_eq!((m.op, m.payload.as_slice()), (2, &[1u8][..]));
        let got = rx.take_fds(2).expect("two fds");
        assert!(rx.take_fds(1).is_none());
        let mut g = std::fs::File::from(got.into_iter().next().unwrap());
        g.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        g.read_to_string(&mut s).unwrap();
        assert_eq!(s, "frame");
        assert_eq!(rx.recv_message().unwrap().op, 3);
    }

    fn tempfile_fd() -> OwnedFd {
        let p = std::env::temp_dir().join(format!("aqueduct-fd-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        f.into()
    }
}
