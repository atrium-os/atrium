//! Device/publisher ↔ relay wire protocol.
//!
//! Framing: a 4-byte little-endian length prefix
//! followed by a CBOR-encoded message — the wire shape
//! `tabellarius.md` §3.2 specifies, chosen so a
//! third-party relay or device written in any language
//! can interoperate. (The transport is still plaintext
//! TCP in v0; mutual-auth TLS is the remaining §3.2
//! hardening item.)
//!
//! Connection roles are implicit: a connection that
//! sends [`ClientMsg::Subscribe`] is a *device*
//! connection (it then receives `Push` / `Pong`); a
//! connection that sends [`ClientMsg::Publish`] is a
//! *publisher* connection (it receives
//! `PublishAccepted` / `PublishRejected`).

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// A subscription public key — the address a publisher
/// targets, the interest a device registers.
pub type PushKey = [u8; 32];

/// Messages a client (device or publisher) sends to
/// the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    // ── device side ────────────────────────────────
    /// Register interest in these pubkeys. The relay
    /// will forward any blob addressed to one of them.
    /// Idempotent + additive — re-sending merges.
    Subscribe { keys: Vec<PushKey> },
    /// Drop interest in these pubkeys.
    Unsubscribe { keys: Vec<PushKey> },
    /// Acknowledge a delivered push by id. v0 relay
    /// records the ack but does not retry un-acked
    /// pushes (at-most-once); spec §3.2's at-least-once
    /// retry is a follow-up.
    Ack { id: u64 },
    /// Keep-alive. Relay answers with [`RelayMsg::Pong`].
    Ping,

    // ── publisher side ─────────────────────────────
    /// Submit an encrypted blob addressed to `to_key`.
    /// `ttl_secs` is advisory in v0 (the relay fans out
    /// immediately to currently-connected devices and
    /// does not queue for offline ones yet).
    Publish { to_key: PushKey, blob: Vec<u8>, ttl_secs: u64 },
}

/// Messages the relay sends back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayMsg {
    /// A push for a key this device subscribed to.
    Push { id: u64, to_key: PushKey, ts: u64, blob: Vec<u8> },
    /// Answer to [`ClientMsg::Ping`].
    Pong,
    /// A publisher's `Publish` was routed. `delivered`
    /// is the number of subscribed devices it reached.
    PublishAccepted { id: u64, delivered: u32 },
    /// A publisher's `Publish` was refused.
    PublishRejected { reason: String },
}

/// Maximum frame size the reader will accept — guards
/// against a malformed length prefix demanding a
/// gigabyte allocation. 16 MiB is far above any real
/// push blob.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Write one length-prefixed CBOR frame.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut bytes = Vec::new();
    ciborium::into_writer(msg, &mut bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("cbor encode: {e}")))?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_LEN"));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one length-prefixed CBOR frame.
pub fn read_msg<R: Read, T: serde::de::DeserializeOwned>(r: &mut R)
    -> io::Result<T>
{
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("frame len {len} exceeds MAX_FRAME_LEN")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    ciborium::from_reader(&buf[..])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("cbor decode: {e}")))
}

/// Encode a message into one length-prefixed CBOR
/// frame (the bytes [`write_msg`] would put on the
/// wire). Useful when the caller drives I/O itself —
/// e.g. a poll loop that batches writes.
pub fn encode_frame<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    ciborium::into_writer(msg, &mut body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("cbor encode: {e}")))?;
    if body.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_LEN"));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one CBOR frame payload (the bytes *after*
/// the 4-byte length prefix).
pub fn decode_payload<T: serde::de::DeserializeOwned>(payload: &[u8])
    -> io::Result<T>
{
    ciborium::from_reader(payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("cbor decode: {e}")))
}

/// Incremental, buffering frame parser.
///
/// A poll loop reading from a socket with a read
/// timeout can return mid-frame — a plain `read_exact`
/// would consume + lose those partial bytes and
/// desync the stream. `FrameReader` buffers every byte
/// fed to it and only yields *complete* frames, so a
/// timeout between reads is harmless.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader { buf: Vec::new() }
    }

    /// Append bytes just read off the wire.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame's payload, if one is
    /// fully buffered. `Ok(None)` means "need more
    /// bytes"; `Err` means a malformed length prefix.
    pub fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(
            [self.buf[0], self.buf[1], self.buf[2], self.buf[3]]
        ) as usize;
        if len > MAX_FRAME_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                format!("frame len {len} exceeds MAX_FRAME_LEN")));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_round_trips_through_frame() {
        let cases = vec![
            ClientMsg::Subscribe { keys: vec![[1u8; 32], [2u8; 32]] },
            ClientMsg::Unsubscribe { keys: vec![[9u8; 32]] },
            ClientMsg::Ack { id: 42 },
            ClientMsg::Ping,
            ClientMsg::Publish {
                to_key: [7u8; 32],
                blob: vec![0xde, 0xad, 0xbe, 0xef],
                ttl_secs: 3600,
            },
        ];
        for msg in cases {
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).unwrap();
            let mut cur = std::io::Cursor::new(&buf);
            let back: ClientMsg = read_msg(&mut cur).unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn relay_msg_round_trips_through_frame() {
        let cases = vec![
            RelayMsg::Push {
                id: 1, to_key: [3u8; 32], ts: 1700000000,
                blob: vec![1, 2, 3],
            },
            RelayMsg::Pong,
            RelayMsg::PublishAccepted { id: 5, delivered: 2 },
            RelayMsg::PublishRejected { reason: "no subscribers".into() },
        ];
        for msg in cases {
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).unwrap();
            let mut cur = std::io::Cursor::new(&buf);
            let back: RelayMsg = read_msg(&mut cur).unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn frame_reader_yields_complete_frames_only() {
        let msg = ClientMsg::Ack { id: 7 };
        let frame = encode_frame(&msg).unwrap();

        let mut fr = FrameReader::new();
        // Feed one byte at a time — no frame until the
        // very last byte completes it.
        for (i, b) in frame.iter().enumerate() {
            fr.feed(&[*b]);
            let got = fr.next_frame().unwrap();
            if i + 1 == frame.len() {
                let payload = got.expect("last byte completes the frame");
                let back: ClientMsg = decode_payload(&payload).unwrap();
                assert_eq!(back, msg);
            } else {
                assert!(got.is_none(), "partial frame must not yield");
            }
        }
    }

    #[test]
    fn frame_reader_splits_a_coalesced_two_frame_buffer() {
        let a = encode_frame(&ClientMsg::Ping).unwrap();
        let b = encode_frame(&ClientMsg::Ack { id: 99 }).unwrap();
        let mut both = a.clone();
        both.extend_from_slice(&b);

        let mut fr = FrameReader::new();
        fr.feed(&both);
        let f1: ClientMsg = decode_payload(
            &fr.next_frame().unwrap().unwrap()).unwrap();
        let f2: ClientMsg = decode_payload(
            &fr.next_frame().unwrap().unwrap()).unwrap();
        assert_eq!(f1, ClientMsg::Ping);
        assert_eq!(f2, ClientMsg::Ack { id: 99 });
        assert!(fr.next_frame().unwrap().is_none());
    }

    #[test]
    fn frame_reader_rejects_oversized_prefix() {
        let mut fr = FrameReader::new();
        fr.feed(&(1u32 << 30).to_le_bytes());
        assert!(fr.next_frame().is_err());
    }

    #[test]
    fn oversized_length_prefix_is_rejected() {
        // Hand-craft a frame claiming 1 GiB.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(1u32 << 30).to_le_bytes());
        let mut cur = std::io::Cursor::new(&buf);
        let r: io::Result<ClientMsg> = read_msg(&mut cur);
        assert!(r.is_err(), "oversized frame must be refused");
    }
}
