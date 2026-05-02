//! CAS layer (opcode_class = CLASS_CORE).
//!
//! Built-in opcodes that every atrium-rpc speaker implements. Hashes
//! are SHA-256 (32 bytes), matching Tessera so the same hash means
//! the same content across the storage layer and the IPC fabric.
//!
//! Critical semantic: hashes are **advisory pointers**, not
//! capability tokens. A receiver only learns content X if a sender
//! explicitly serves it (via UPLOAD_BEGIN/DATA/FINISH). On every
//! cache use, the receiver verifies `sha256(bytes) == claimed_hash`.
//!
//! This file defines the wire shapes; the per-process cache and the
//! upload/fetch state machine live in `connection.rs`.

use sha2::{Digest, Sha256};

/// SHA-256 hash size, matching the Tessera CAS hash type.
pub const HASH_LEN: usize = 32;
pub type Hash = [u8; HASH_LEN];

/// CAS-layer opcodes. All sit under opcode_class = CLASS_CORE (= 0).
pub mod op {
    pub const UPLOAD_BEGIN:   u16 = 0x01;
    pub const UPLOAD_DATA:    u16 = 0x02;
    pub const UPLOAD_FINISH:  u16 = 0x03;
    pub const UPLOAD_ACK:     u16 = 0x04;
    pub const FETCH_REQUEST:  u16 = 0x05;
    pub const FETCH_BEGIN:    u16 = 0x06;
    pub const TESSERA_PROBE:  u16 = 0x07;
    pub const EVICT_HINT:     u16 = 0x08;
    pub const NEGOTIATE_CAPS: u16 = 0xFF;
}

/// Convenience: SHA-256 over a byte slice.
pub fn hash(data: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut a = [0u8; HASH_LEN];
    a.copy_from_slice(&out);
    a
}

/// Verify-on-use. Returns true iff `sha256(data) == claimed`. Cheap;
/// run on every cache hit before trusting the bytes.
pub fn verify(data: &[u8], claimed: &Hash) -> bool {
    hash(data).as_slice() == claimed.as_slice()
}

// ── UPLOAD_BEGIN payload ───────────────────────────────────────────
//
//   offset  size  field
//     0     32    hash               (claimed sha256 of full blob)
//    32      8    total_size  (u64)  (bytes in the blob)
//    40      *    inline_data        (first chunk; may be empty)
//
// inline_data lets very small blobs (≤ ~MAX_INLINE) ride entirely
// inside one UPLOAD_BEGIN — no DATA/FINISH frames needed; the receiver
// recognises end-of-blob when inline_data.len() == total_size.

/// Practical inline cap. Senders may use larger but receivers must
/// accept up to this. Past this, switch to DATA frames.
pub const MAX_INLINE_BEGIN: usize = 4096;

#[derive(Debug, Clone)]
pub struct UploadBegin<'a> {
    pub hash: Hash,
    pub total_size: u64,
    pub inline: &'a [u8],
}

impl<'a> UploadBegin<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(40 + self.inline.len());
        out.extend_from_slice(&self.hash);
        out.extend_from_slice(&self.total_size.to_le_bytes());
        out.extend_from_slice(self.inline);
        out
    }
    pub fn decode(b: &'a [u8]) -> Result<Self, &'static str> {
        if b.len() < 40 { return Err("UPLOAD_BEGIN too short"); }
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(&b[..32]);
        let total_size = u64::from_le_bytes(b[32..40].try_into().unwrap());
        Ok(Self { hash, total_size, inline: &b[40..] })
    }
}

// ── UPLOAD_DATA payload ────────────────────────────────────────────
//
//   offset  size  field
//     0     32    hash
//    32      8    offset (u64)
//    40      *    data

#[derive(Debug, Clone)]
pub struct UploadData<'a> {
    pub hash: Hash,
    pub offset: u64,
    pub data: &'a [u8],
}

impl<'a> UploadData<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(40 + self.data.len());
        out.extend_from_slice(&self.hash);
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(self.data);
        out
    }
    pub fn decode(b: &'a [u8]) -> Result<Self, &'static str> {
        if b.len() < 40 { return Err("UPLOAD_DATA too short"); }
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(&b[..32]);
        let offset = u64::from_le_bytes(b[32..40].try_into().unwrap());
        Ok(Self { hash, offset, data: &b[40..] })
    }
}

// ── UPLOAD_FINISH / UPLOAD_ACK payloads ─────────────────────────────
//
// Both are just the hash. FINISH says "I'm done, verify and ack."
// ACK says "I have it." The hash echoes so the sender can correlate
// pipelined uploads.

pub fn encode_hash_msg(h: &Hash) -> Vec<u8> { h.to_vec() }
pub fn decode_hash_msg(b: &[u8]) -> Result<Hash, &'static str> {
    if b.len() != HASH_LEN { return Err("hash msg wrong length"); }
    let mut out = [0u8; HASH_LEN];
    out.copy_from_slice(b);
    Ok(out)
}

// ── FETCH_REQUEST / TESSERA_PROBE / EVICT_HINT ──────────────────────
//
// All carry just a hash; semantics differ.
//   FETCH_REQUEST   — receiver asking sender to (re)upload a blob it
//                     no longer has cached.
//   TESSERA_PROBE   — sender hint that the blob is in Tessera CAS
//                     and the receiver may have a faster path to it.
//                     Advisory; receiver may ignore.
//   EVICT_HINT      — sender promising not to refer to this hash
//                     again; receiver may safely drop its cache.

// ── NEGOTIATE_CAPS ──────────────────────────────────────────────────
//
//   offset  size  field
//     0      1    envelope_version_max   (sender's max version)
//     1      1    n_classes              (count of supported classes)
//     2      *    class_ids              (n_classes bytes)

#[derive(Debug, Clone)]
pub struct NegotiateCaps {
    pub envelope_version_max: u8,
    pub classes: Vec<u8>,
}

impl NegotiateCaps {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.classes.len());
        out.push(self.envelope_version_max);
        out.push(self.classes.len() as u8);
        out.extend_from_slice(&self.classes);
        out
    }
    pub fn decode(b: &[u8]) -> Result<Self, &'static str> {
        if b.len() < 2 { return Err("NEGOTIATE_CAPS too short"); }
        let v = b[0];
        let n = b[1] as usize;
        if b.len() < 2 + n { return Err("NEGOTIATE_CAPS truncated"); }
        Ok(Self {
            envelope_version_max: v,
            classes: b[2..2 + n].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_basic() {
        let h = hash(b"hello");
        // Known sha256("hello") prefix to sanity-check we're using
        // sha2 correctly.
        assert_eq!(h[0], 0x2c);
        assert_eq!(h[1], 0xf2);
        assert!(verify(b"hello", &h));
        assert!(!verify(b"world", &h));
    }

    #[test]
    fn upload_begin_roundtrip() {
        let h = hash(b"payload");
        let inline = b"payload";
        let ub = UploadBegin { hash: h, total_size: 7, inline };
        let bytes = ub.encode();
        let ub2 = UploadBegin::decode(&bytes).unwrap();
        assert_eq!(ub2.hash, h);
        assert_eq!(ub2.total_size, 7);
        assert_eq!(ub2.inline, inline);
    }

    #[test]
    fn upload_data_roundtrip() {
        let h = hash(b"chunked");
        let data = b"chunkbytes";
        let ud = UploadData { hash: h, offset: 4096, data };
        let bytes = ud.encode();
        let ud2 = UploadData::decode(&bytes).unwrap();
        assert_eq!(ud2.hash, h);
        assert_eq!(ud2.offset, 4096);
        assert_eq!(ud2.data, data);
    }

    #[test]
    fn negotiate_caps_roundtrip() {
        let nc = NegotiateCaps {
            envelope_version_max: 1,
            classes: vec![0, 2, 3, 4],
        };
        let b = nc.encode();
        let nc2 = NegotiateCaps::decode(&b).unwrap();
        assert_eq!(nc2.envelope_version_max, 1);
        assert_eq!(nc2.classes, vec![0, 2, 3, 4]);
    }
}
