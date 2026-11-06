//! What a session's recording carries, as the BROKER knows it.
//!
//! ★★ A WORKER IS NEVER ASKED WHAT ITS DOCUMENT CONTAINS when the answer
//! would grant it something. It is jailed because it may be compromised,
//! and a compromised worker asked "does your document name this image?" or
//! "does it link here?" answers yes. So the broker keeps the recording's
//! bytes — content-addressed, the bytes it was handed and stored, not
//! anything a worker said — and answers from them.
//!
//! And it answers WITHOUT PARSING them (`navigatord` never parses a
//! document): an image address is `blake3:` and 64 hex digits, which a byte
//! scan finds wherever the recording names it; a link is a byte search for
//! its spelling. Both are supersets of what the current document holds — a
//! string anywhere in the recording, including a transition that has not
//! run — and that is the point: they bound a compromised worker to what the
//! recording already says, so it cannot invent an address or a URL. The
//! precise question, "does the CURRENT document carry it", is the worker's,
//! and an honest worker answers it too (see `navigatord`).

use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct Carried {
    bytes: Vec<u8>,
    images: BTreeSet<String>,
}

impl Carried {
    pub fn new(recording: &[u8]) -> Self {
        Carried { images: image_addresses(recording), bytes: recording.to_vec() }
    }

    /// The recording names this image address (`blake3:<hex>`).
    pub fn image(&self, address: &str) -> bool { self.images.contains(address) }

    /// ★ Add bytes delivered apart from the recording — a transitions
    /// artifact (legacy-web §5.4.5.7) — to what the session carries. An
    /// inserted effect's links and images are the document's, and the bound
    /// must know them; it still names only bytes the BROKER was handed.
    pub fn extend(&mut self, more: &[u8]) {
        self.images.extend(image_addresses(more));
        self.bytes.push(0);
        self.bytes.extend_from_slice(more);
    }

    /// Every image address the recording names.
    pub fn images(&self) -> &BTreeSet<String> { &self.images }

    /// The recording contains these bytes, anywhere.
    pub fn contains(&self, needle: &[u8]) -> bool {
        !needle.is_empty() && memchr::memmem::find(&self.bytes, needle).is_some()
    }

    pub fn len(&self) -> usize { self.bytes.len() }
    /// The recording's bytes, as the broker holds them.
    pub fn bytes(&self) -> &[u8] { &self.bytes }
    pub fn is_empty(&self) -> bool { self.bytes.is_empty() }
}

/// Every `blake3:` followed by exactly 64 lowercase hex digits (and not a
/// 65th), by scanning — the shape `image::address_hex` accepts.
pub fn image_addresses(b: &[u8]) -> BTreeSet<String> {
    const P: &[u8] = b"blake3:";
    let hex = |c: u8| matches!(c, b'0'..=b'9' | b'a'..=b'f');
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i + P.len() + 64 <= b.len() {
        if &b[i..i + P.len()] == P {
            let h = &b[i + P.len()..i + P.len() + 64];
            let after = b.get(i + P.len() + 64).copied();
            if h.iter().all(|&c| hex(c)) && !after.is_some_and(hex) {
                out.insert(String::from_utf8_lossy(&b[i..i + P.len() + 64]).into_owned());
                i += P.len() + 64;
                continue;
            }
        }
        i += 1;
    }
    out
}
