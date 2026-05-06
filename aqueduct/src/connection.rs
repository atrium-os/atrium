//! `Connection` — per-process socket + CAS cache + send/recv plumbing.
//!
//! Wraps a `UnixStream`, gives services a higher-level API:
//!
//!  * `send_message` — push one envelope-tagged message.
//!  * `recv_message` — pull the next message; auto-handles CAS-layer
//!    traffic (UPLOAD_BEGIN/DATA/FINISH, ACKs) so the caller only
//!    sees their own opcode_class messages.
//!  * `upload_blob` / `fetch_blob` — convenience for CAS payload
//!    handoff.
//!  * `set_cache_cap` — cap RAM used by the per-connection blob
//!    cache.
//!
//! Verify-on-use is unconditional: every cache read re-hashes the
//! bytes and rejects on mismatch.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::fd::{AsRawFd, RawFd};
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
    rd:        BufReader<UnixStream>,
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
    fn as_raw_fd(&self) -> RawFd { self.rd.get_ref().as_raw_fd() }
}

impl Connection {
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let s = UnixStream::connect(path)?;
        Self::wrap(s)
    }

    pub fn wrap(s: UnixStream) -> io::Result<Self> {
        let rd = BufReader::new(s.try_clone()?);
        let wr = BufWriter::new(s);
        Ok(Self {
            rd, wr,
            cache: HashMap::new(),
            cache_bytes: 0,
            cache_cap:   DEFAULT_CACHE_BYTES,
            _outbound_uploads: HashMap::new(),
            incoming_uploads:  HashMap::new(),
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
        self.wr.flush()
    }

    /// Pull the next service-level message. Auto-handles
    /// CLASS_CORE traffic in the background (cache loading, ACKs).
    /// Caller-visible messages are everything in their own service
    /// class, plus FETCH_REQUEST (which the caller must answer by
    /// re-uploading) and EVICT_HINT (advisory).
    pub fn recv_message(&mut self) -> io::Result<Message> {
        loop {
            let (h, payload) = envelope::read_message_alloc(&mut self.rd)?;
            if h.opcode_class == CLASS_CORE
                && self.handle_core(h.op, &payload)?
            {
                continue; /* swallowed at the substrate layer */
            }
            let kind = if h.flags & flag::IS_RESPONSE != 0 {
                MessageKind::Response
            } else if h.flags & flag::ASYNC_EVENT != 0 {
                MessageKind::Event
            } else {
                MessageKind::Request
            };
            return Ok(Message {
                opcode_class: h.opcode_class,
                op:           h.op,
                flags:        h.flags,
                kind,
                payload,
            });
        }
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
        loop {
            let (h, payload) = envelope::read_message_alloc(&mut self.rd)?;
            if h.opcode_class == class && h.op == op {
                return cas::decode_hash_msg(&payload).map_err(io_invalid);
            }
            /* Anything else: process via handle_core or buffer.
             * For the first pass, only let CORE through; reject
             * service-class messages that arrive mid-upload. */
            if h.opcode_class == CLASS_CORE {
                let _ = self.handle_core(h.op, &payload)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected service message (class {}) mid-upload",
                        h.opcode_class),
                ));
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
        self.rd.get_ref().set_read_timeout(t)
    }

    /// Toggle non-blocking I/O. Used by clients that want a true
    /// "is there data right now?" poll without setting a sub-tick
    /// timeout (FreeBSD rejects `set_read_timeout(Duration::ZERO)`
    /// with EINVAL — `set_nonblocking(true)` is the portable way).
    pub fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.rd.get_ref().set_nonblocking(nb)
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
