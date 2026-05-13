//! atrium-spv-pcmap — PC-map sidecar format for tier-2
//! software Vulkan shaders.
//!
//! Maps host instruction PCs (offsets into the shader
//! `.so`'s `__text` section) back to the SPIR-V byte
//! offset that produced them. Lets the daemon's crash
//! handler convert "SIGSEGV at native PC 0x7ffe...c4f8"
//! into "shader sha256-abc..., SPIR-V offset 0x214"
//! before logging.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §10.1 — sidecar
//!   format spec
//! - [`docs/spec/tier2-shader-codegen-constraints.md`]
//!   constraint A2 — every IR instruction carries its
//!   source SPIR-V offset for this purpose; PPTK runbook's
//!   biggest hindsight regret was retrofitting this
//!   instead of building it in (constraint G5).
//!
//! # On-disk format (v1)
//!
//! ```text
//! +---------+--------+
//! | 0..8    | "ATRPCMAP" (magic; 8 ASCII bytes)
//! | 8..12   | version (u32, LE) — currently 1
//! | 12..16  | entry_count (u32, LE)
//! | 16+0    | entries[0].host_offset (u32, LE)
//! | 16+4    | entries[0].spirv_offset (u32, LE)
//! | 16+8    | entries[1].host_offset (u32, LE)
//! | ...
//! ```
//!
//! Total size = 16 + 8 × entry_count bytes. Entries are
//! sorted by `host_offset` ascending, which lets the
//! lookup path use binary search.
//!
//! # Crate scope
//!
//! Producer-side: [`Builder`] accumulates entries during
//! codegen and emits a `Vec<u8>` for writing alongside the
//! shader `.so`.
//!
//! Consumer-side: [`PcMap::from_bytes`] parses + validates
//! a sidecar; [`PcMap::lookup`] performs the host-PC →
//! SPIR-V-offset binary search.
//!
//! No runtime dependency on `atrium-spv-ir` (the IR knows
//! the source offset; the codegen passes it here as a raw
//! `u32`). Keeps this crate dependency-free.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Magic bytes at the start of every PC-map sidecar.
pub const MAGIC: &[u8; 8] = b"ATRPCMAP";

/// Current sidecar format version.
///
/// Bumped on incompatible format changes. Producer + daemon
/// must agree on version; mismatches return
/// [`ParseError::VersionMismatch`].
pub const VERSION: u32 = 1;

/// Byte size of the fixed header (magic + version + count).
pub const HEADER_SIZE: usize = 16;

/// Byte size of one [`Entry`] in the on-disk layout.
pub const ENTRY_SIZE: usize = 8;

/// One (host_offset, spirv_offset) mapping.
///
/// `host_offset` is the byte offset into the `.so`'s
/// `__text` section where the instruction lives.
/// `spirv_offset` is the byte offset into the source
/// SPIR-V module that the IR instruction at this host PC
/// came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Native code offset within the shader `.so`'s text section.
    pub host_offset: u32,
    /// Source SPIR-V byte offset that produced this code.
    pub spirv_offset: u32,
}

/// A complete PC-map sidecar.
///
/// Entries are sorted by `host_offset` ascending — both
/// the builder and the parser enforce this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcMap {
    entries: Vec<Entry>,
}

impl PcMap {
    /// Construct from an already-sorted entry list.
    ///
    /// Returns an error if entries aren't sorted by
    /// `host_offset` ascending. (Duplicate host_offsets are
    /// allowed — the lookup path returns the largest
    /// spirv_offset with `host_offset <= query`, which
    /// works even with duplicates.)
    pub fn from_sorted_entries(entries: Vec<Entry>) -> Result<Self, BuildError> {
        for w in entries.windows(2) {
            if w[0].host_offset > w[1].host_offset {
                return Err(BuildError::NotSorted {
                    index: 0, // unused; window contents are the diagnostic
                });
            }
        }
        Ok(Self { entries })
    }

    /// Parse + validate a sidecar from raw bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ParseError::Truncated);
        }
        if &bytes[..8] != MAGIC {
            return Err(ParseError::BadMagic);
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(ParseError::VersionMismatch {
                found: version,
                expected: VERSION,
            });
        }
        let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let expected_size = HEADER_SIZE + count * ENTRY_SIZE;
        if bytes.len() != expected_size {
            return Err(ParseError::SizeMismatch {
                found: bytes.len(),
                expected: expected_size,
            });
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let base = HEADER_SIZE + i * ENTRY_SIZE;
            let host_offset = u32::from_le_bytes(
                bytes[base..base + 4].try_into().unwrap(),
            );
            let spirv_offset = u32::from_le_bytes(
                bytes[base + 4..base + 8].try_into().unwrap(),
            );
            if let Some(prev) = entries.last() {
                let prev: &Entry = prev;
                if host_offset < prev.host_offset {
                    return Err(ParseError::EntriesNotSorted { index: i });
                }
            }
            entries.push(Entry { host_offset, spirv_offset });
        }
        Ok(Self { entries })
    }

    /// All entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Look up the SPIR-V offset that produced the
    /// instruction at host_pc.
    ///
    /// Returns the spirv_offset of the entry with the
    /// largest host_offset ≤ host_pc — i.e. the
    /// instruction the PC is "inside of." Returns `None`
    /// if host_pc is before the first entry (e.g. before
    /// the function prologue).
    pub fn lookup(&self, host_pc: u32) -> Option<u32> {
        if self.entries.is_empty() { return None; }
        // Binary search for the largest entry with
        // host_offset ≤ host_pc.
        let idx = self.entries.partition_point(|e| e.host_offset <= host_pc);
        if idx == 0 { None } else { Some(self.entries[idx - 1].spirv_offset) }
    }

    /// Serialize the sidecar to a byte buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.entries.len() * ENTRY_SIZE);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(&e.host_offset.to_le_bytes());
            out.extend_from_slice(&e.spirv_offset.to_le_bytes());
        }
        out
    }
}

/// Accumulator used during codegen.
///
/// Backends call [`Builder::push`] as they emit each
/// instruction, then [`Builder::finish`] when done to get
/// the serialized sidecar bytes ready to write to disk.
///
/// The builder enforces `host_offset` monotonicity on
/// `push` — codegen emits instructions in increasing
/// host-PC order, so out-of-order pushes are a backend
/// bug. The builder panics on violation in debug builds
/// and silently swallows them in release (the daemon will
/// reject the malformed sidecar at load time anyway).
#[derive(Debug, Default)]
pub struct Builder {
    entries: Vec<Entry>,
}

impl Builder {
    /// Empty builder.
    pub fn new() -> Self { Self::default() }

    /// Record one mapping.
    ///
    /// `host_offset` must be ≥ the last pushed entry's
    /// `host_offset` (monotone non-decreasing).
    pub fn push(&mut self, host_offset: u32, spirv_offset: u32) {
        if let Some(last) = self.entries.last() {
            debug_assert!(
                host_offset >= last.host_offset,
                "Builder::push called out of order: last host_offset = {}, \
                 new = {}; codegen must emit in increasing host-PC order",
                last.host_offset, host_offset,
            );
        }
        self.entries.push(Entry { host_offset, spirv_offset });
    }

    /// Number of entries accumulated so far.
    pub fn len(&self) -> usize { self.entries.len() }

    /// True if no entries have been pushed.
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Consume the builder and produce a [`PcMap`].
    pub fn finish(self) -> PcMap {
        // Builder enforces sort order on push; from_sorted_entries
        // also re-checks but won't fail given the debug-assert
        // above.
        PcMap::from_sorted_entries(self.entries)
            .expect("Builder invariant violated: entries should be sorted")
    }

    /// Convenience: build + serialize in one step.
    pub fn finish_to_bytes(self) -> Vec<u8> {
        self.finish().to_bytes()
    }
}

// ── Errors ──────────────────────────────────────────────────────

/// Errors from constructing a `PcMap` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// Entries weren't sorted by host_offset ascending.
    NotSorted {
        /// Index where the violation was detected.
        index: usize,
    },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::NotSorted { index } => {
                write!(f, "entries not sorted by host_offset (around index {index})")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Errors from parsing a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than [`HEADER_SIZE`] bytes.
    Truncated,
    /// Magic bytes don't match [`MAGIC`].
    BadMagic,
    /// Version field doesn't match [`VERSION`].
    VersionMismatch {
        /// Version found in the file.
        found: u32,
        /// Version this build of the crate expects.
        expected: u32,
    },
    /// Declared entry count doesn't match the file size.
    SizeMismatch {
        /// Actual file size.
        found: usize,
        /// `HEADER_SIZE + count * ENTRY_SIZE`.
        expected: usize,
    },
    /// Entries in the on-disk file aren't sorted.
    EntriesNotSorted {
        /// Index of the first out-of-order entry.
        index: usize,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Truncated => write!(f, "sidecar truncated"),
            ParseError::BadMagic => write!(f, "bad magic; expected 'ATRPCMAP'"),
            ParseError::VersionMismatch { found, expected } => write!(
                f,
                "version mismatch: file is v{found}, this build expects v{expected}",
            ),
            ParseError::SizeMismatch { found, expected } => write!(
                f,
                "size mismatch: file is {found} bytes; expected {expected} from header",
            ),
            ParseError::EntriesNotSorted { index } => write!(
                f,
                "entries not sorted by host_offset (around index {index})",
            ),
        }
    }
}

impl std::error::Error for ParseError {}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty() {
        let map = Builder::new().finish();
        assert_eq!(map.entries().len(), 0);
        let bytes = map.to_bytes();
        assert_eq!(bytes.len(), HEADER_SIZE);
        let parsed = PcMap::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.entries().len(), 0);
    }

    #[test]
    fn round_trip_with_entries() {
        let mut b = Builder::new();
        b.push(0,    0x10);
        b.push(4,    0x14);
        b.push(20,   0x20);
        b.push(100,  0x80);
        let map = b.finish();
        assert_eq!(map.entries().len(), 4);
        let bytes = map.to_bytes();
        assert_eq!(bytes.len(), HEADER_SIZE + 4 * ENTRY_SIZE);
        let parsed = PcMap::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.entries(), map.entries());
    }

    #[test]
    fn lookup_finds_containing_entry() {
        let mut b = Builder::new();
        b.push(0,    0x10);
        b.push(16,   0x20);
        b.push(48,   0x30);
        b.push(100,  0x80);
        let map = b.finish();
        // Each query returns the SPIR-V offset of the
        // largest host_offset ≤ query.
        assert_eq!(map.lookup(0),   Some(0x10));
        assert_eq!(map.lookup(4),   Some(0x10));
        assert_eq!(map.lookup(15),  Some(0x10));
        assert_eq!(map.lookup(16),  Some(0x20));
        assert_eq!(map.lookup(17),  Some(0x20));
        assert_eq!(map.lookup(47),  Some(0x20));
        assert_eq!(map.lookup(48),  Some(0x30));
        assert_eq!(map.lookup(99),  Some(0x30));
        assert_eq!(map.lookup(100), Some(0x80));
        assert_eq!(map.lookup(u32::MAX), Some(0x80));
    }

    #[test]
    fn lookup_below_first_entry_returns_none() {
        let mut b = Builder::new();
        b.push(100, 0x10);
        let map = b.finish();
        assert_eq!(map.lookup(0),  None);
        assert_eq!(map.lookup(99), None);
        assert_eq!(map.lookup(100), Some(0x10));
    }

    #[test]
    fn lookup_on_empty_returns_none() {
        let map = Builder::new().finish();
        assert_eq!(map.lookup(0), None);
        assert_eq!(map.lookup(u32::MAX), None);
    }

    #[test]
    fn parse_rejects_truncated() {
        assert_eq!(PcMap::from_bytes(b""), Err(ParseError::Truncated));
        assert_eq!(PcMap::from_bytes(b"ATRPCMAP"), Err(ParseError::Truncated));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut bytes = vec![0u8; HEADER_SIZE];
        bytes[..8].copy_from_slice(b"NOTAMAGC");
        assert_eq!(PcMap::from_bytes(&bytes), Err(ParseError::BadMagic));
    }

    #[test]
    fn parse_rejects_version_mismatch() {
        let mut bytes = Vec::with_capacity(HEADER_SIZE);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&999u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        match PcMap::from_bytes(&bytes) {
            Err(ParseError::VersionMismatch { found: 999, expected: VERSION }) => (),
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_size_mismatch() {
        // Header claims 1 entry but only header bytes are present.
        let mut bytes = Vec::with_capacity(HEADER_SIZE);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        match PcMap::from_bytes(&bytes) {
            Err(ParseError::SizeMismatch { found: 16, expected: 24 }) => (),
            other => panic!("expected SizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_unsorted_entries() {
        // Hand-craft a file with two entries in wrong order.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        // entry 0: host=100
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&0x10u32.to_le_bytes());
        // entry 1: host=50 (should come first!)
        bytes.extend_from_slice(&50u32.to_le_bytes());
        bytes.extend_from_slice(&0x08u32.to_le_bytes());
        assert_eq!(
            PcMap::from_bytes(&bytes),
            Err(ParseError::EntriesNotSorted { index: 1 }),
        );
    }

    #[test]
    fn duplicate_host_offsets_allowed() {
        // Multiple SPIR-V offsets can map to the same host PC
        // (rare but possible if multiple IR ops collapse to
        // zero native bytes). Lookup returns the last one
        // recorded.
        let mut b = Builder::new();
        b.push(0,  0x10);
        b.push(0,  0x14);
        b.push(0,  0x18);
        b.push(8,  0x20);
        let map = b.finish();
        assert_eq!(map.lookup(0),  Some(0x18));
        assert_eq!(map.lookup(7),  Some(0x18));
        assert_eq!(map.lookup(8),  Some(0x20));
    }

    #[test]
    #[should_panic(expected = "out of order")]
    fn builder_panics_on_out_of_order_push_in_debug() {
        let mut b = Builder::new();
        b.push(100, 0x10);
        b.push(50, 0x08); // ← violates monotonicity
    }

    #[test]
    fn finish_to_bytes_matches_finish_then_to_bytes() {
        let mut b1 = Builder::new();
        b1.push(0,  0x10);
        b1.push(8,  0x14);
        let bytes_a = b1.finish_to_bytes();

        let mut b2 = Builder::new();
        b2.push(0,  0x10);
        b2.push(8,  0x14);
        let bytes_b = b2.finish().to_bytes();

        assert_eq!(bytes_a, bytes_b);
    }
}
