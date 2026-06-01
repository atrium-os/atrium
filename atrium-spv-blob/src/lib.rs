//! atrium-spv-blob — flat executable-blob container for
//! tier-2 software Vulkan shaders.
//!
//! A tier-2 backend emits a *position-independent native
//! code blob* (the raw bytes of `.text`, with all branches
//! already PC-relative and patched) plus a small per-stage
//! entry-offset table. The daemon's loader `mmap`s the
//! blob `PROT_EXEC` and jumps straight in — no ELF/Mach-O
//! object, no `cc -shared`, no `dlopen`.
//!
//! # Why
//!
//! Per-phase instrumentation of `atrium-spv-compile`
//! showed `cc` linking is ~99.5% of compile wall-clock
//! (~40 ms; frontend+backend together ~0.3 ms). The
//! object→`cc`→`.so`→`dlopen` path exists only because
//! that was the quickest way to turn backend output into
//! callable code. The bespoke backend already emits
//! self-contained PIC machine code — branches patched
//! in-backend, constants materialised inline, the fragment
//! ABI entirely register/pointer — so it has no external
//! relocations and needs no linker at all. This crate is
//! the container that lets the loader skip straight to
//! `mmap`. See the RUNBOOK "JIT-emit path" design.
//!
//! # On-disk / on-wire format (v1)
//!
//! ```text
//! +-----------+-----------------------------------------+
//! | 0..8      | "ATRMBLOB" (magic; 8 ASCII bytes)       |
//! | 8..12     | version  (u32 LE) — currently 1         |
//! | 12..16    | arch     (u32 LE) — 0 = aarch64         |
//! | 16..20    | flags    (u32 LE) — reserved, 0         |
//! | 20..24    | code_len (u32 LE)                       |
//! | 24..28    | vs_off   (u32 LE; u32::MAX = absent)    |
//! | 28..32    | fs_off   (u32 LE; u32::MAX = absent)    |
//! | 32..36    | cs_off   (u32 LE; u32::MAX = absent)    |
//! | 36..40    | fs_span_off (u32 LE; u32::MAX=absent)   |
//! | 40..48    | reserved (8 bytes, zero)                |
//! | 48..      | code  (code_len bytes)                  |
//! +-----------+-----------------------------------------+
//! ```
//!
//! `HEADER_SIZE` is 48 so the code starts 16-byte aligned.
//! Each present entry offset is a byte offset into `code`
//! and is 4-byte aligned (ARM64 instruction width). A flat
//! AAPCS64 code blob is **OS-agnostic** — only `arch`
//! distinguishes targets, not FreeBSD-vs-Darwin.
//!
//! # Crate scope
//!
//! Producer-side: backends build a [`ShaderBlob`] and call
//! [`ShaderBlob::to_bytes`].
//!
//! Consumer-side: the loader calls [`ShaderBlob::from_bytes`]
//! to parse + validate, then `mmap`s [`ShaderBlob::code`]
//! and resolves entry pointers as `base + offset`.
//!
//! No dependency on `atrium-spv-ir` or any backend — the
//! format is just bytes + offsets.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

/// Magic bytes at the start of every shader blob.
pub const MAGIC: &[u8; 8] = b"ATRMBLOB";

/// Current blob format version. Bumped on incompatible
/// format changes; producer + loader must agree.
///
/// v2 (P2): carved an `fs_span_off` slot out of the reserved
/// header bytes for the batched fragment entry
/// (`atrium_fs_main_span`).  v1 blobs wrote zero there (which
/// would mis-resolve as offset 0), so the version bump forces a
/// recompile.
pub const VERSION: u32 = 2;

/// Byte size of the fixed header. Chosen so `code` starts
/// 16-byte aligned.
pub const HEADER_SIZE: usize = 48;

/// `arch` value for 64-bit ARM (AArch64 / AAPCS64). The
/// only architecture the bespoke backend targets.
pub const ARCH_AARCH64: u32 = 0;

/// Sentinel in an entry-offset slot meaning "this stage's
/// entry point is not present in this blob".
const ENTRY_ABSENT: u32 = u32::MAX;

/// Per-shader-stage entry-point byte offsets into the code
/// blob. A shader module exports exactly one of these in
/// practice, but the table has a slot for each stage so
/// the format doesn't care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryOffsets {
    /// `atrium_vs_main` offset, if this is a vertex shader.
    pub vs: Option<u32>,
    /// `atrium_fs_main` offset, if this is a fragment shader.
    pub fs: Option<u32>,
    /// `atrium_cs_main` offset, if this is a compute shader.
    pub cs: Option<u32>,
    /// `atrium_fs_main_span` offset (P2 batched fragment entry),
    /// if the backend emitted it.  `None` when the fragment
    /// shader has only the scalar `fs` entry.
    pub fs_span: Option<u32>,
}

impl EntryOffsets {
    /// True when no stage entry point is present — a blob
    /// the loader can parse but never call.
    pub fn is_empty(&self) -> bool {
        self.vs.is_none() && self.fs.is_none() && self.cs.is_none()
    }
}

/// A parsed (or about-to-be-serialised) shader blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderBlob {
    /// Target architecture — currently always
    /// [`ARCH_AARCH64`].
    pub arch: u32,
    /// Position-independent native code. Branches are
    /// already PC-relative and patched; there are no
    /// external relocations.
    pub code: Vec<u8>,
    /// Per-stage entry-point offsets into `code`.
    pub entries: EntryOffsets,
}

impl ShaderBlob {
    /// Serialise to the on-disk/on-wire byte layout.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.code.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.arch.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&(self.code.len() as u32).to_le_bytes());
        out.extend_from_slice(
            &self.entries.vs.unwrap_or(ENTRY_ABSENT).to_le_bytes());
        out.extend_from_slice(
            &self.entries.fs.unwrap_or(ENTRY_ABSENT).to_le_bytes());
        out.extend_from_slice(
            &self.entries.cs.unwrap_or(ENTRY_ABSENT).to_le_bytes());
        out.extend_from_slice(
            &self.entries.fs_span.unwrap_or(ENTRY_ABSENT).to_le_bytes());
        out.extend_from_slice(&[0u8; 8]); // reserved
        debug_assert_eq!(out.len(), HEADER_SIZE);
        out.extend_from_slice(&self.code);
        out
    }

    /// Parse + validate a blob from raw bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ParseError::Truncated);
        }
        if &bytes[..8] != MAGIC {
            return Err(ParseError::BadMagic);
        }
        let u32_at = |o: usize| u32::from_le_bytes(
            bytes[o..o + 4].try_into().unwrap());
        let version = u32_at(8);
        if version != VERSION {
            return Err(ParseError::VersionMismatch {
                found: version, expected: VERSION,
            });
        }
        let arch = u32_at(12);
        let code_len = u32_at(20) as usize;
        let expected = HEADER_SIZE + code_len;
        if bytes.len() != expected {
            return Err(ParseError::SizeMismatch {
                found: bytes.len(), expected,
            });
        }
        // Resolve an entry slot: ENTRY_ABSENT → None;
        // otherwise it must be a 4-aligned offset strictly
        // inside the code (an entry points *at* an
        // instruction).
        let entry = |slot: u32, stage: Stage|
            -> Result<Option<u32>, ParseError>
        {
            if slot == ENTRY_ABSENT {
                return Ok(None);
            }
            if slot % 4 != 0 || (slot as usize) >= code_len {
                return Err(ParseError::EntryOutOfRange {
                    stage, offset: slot, code_len: code_len as u32,
                });
            }
            Ok(Some(slot))
        };
        let entries = EntryOffsets {
            vs: entry(u32_at(24), Stage::Vertex)?,
            fs: entry(u32_at(28), Stage::Fragment)?,
            cs: entry(u32_at(32), Stage::Compute)?,
            fs_span: entry(u32_at(36), Stage::Fragment)?,
        };
        Ok(Self {
            arch,
            code: bytes[HEADER_SIZE..].to_vec(),
            entries,
        })
    }
}

/// Shader stage — used only to label an
/// [`ParseError::EntryOutOfRange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Vertex shader (`atrium_vs_main`).
    Vertex,
    /// Fragment shader (`atrium_fs_main`).
    Fragment,
    /// Compute shader (`atrium_cs_main`).
    Compute,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stage::Vertex => write!(f, "vertex"),
            Stage::Fragment => write!(f, "fragment"),
            Stage::Compute => write!(f, "compute"),
        }
    }
}

/// Errors from [`ShaderBlob::from_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than [`HEADER_SIZE`] bytes.
    Truncated,
    /// First 8 bytes are not [`MAGIC`].
    BadMagic,
    /// `version` field doesn't match [`VERSION`].
    VersionMismatch {
        /// Version found in the blob.
        found: u32,
        /// Version this build expects.
        expected: u32,
    },
    /// Total length isn't `HEADER_SIZE + code_len`.
    SizeMismatch {
        /// Actual byte length.
        found: usize,
        /// Length the header's `code_len` implies.
        expected: usize,
    },
    /// An entry offset is mis-aligned or points outside the
    /// code region.
    EntryOutOfRange {
        /// Which stage's entry slot.
        stage: Stage,
        /// The bad offset.
        offset: u32,
        /// The code length it had to be inside of.
        code_len: u32,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Truncated =>
                write!(f, "blob truncated (< {HEADER_SIZE}-byte header)"),
            ParseError::BadMagic =>
                write!(f, "bad magic (not an ATRMBLOB)"),
            ParseError::VersionMismatch { found, expected } =>
                write!(f, "blob version {found}, expected {expected}"),
            ParseError::SizeMismatch { found, expected } =>
                write!(f, "blob size {found}, header implies {expected}"),
            ParseError::EntryOutOfRange { stage, offset, code_len } =>
                write!(f, "{stage} entry offset {offset} out of range \
                           (code_len {code_len}, must be 4-aligned and \
                           < code_len)"),
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(code: Vec<u8>, entries: EntryOffsets) -> ShaderBlob {
        ShaderBlob { arch: ARCH_AARCH64, code, entries }
    }

    #[test]
    fn round_trips() {
        let blob = sample(
            (0u8..64).collect(),
            EntryOffsets { vs: None, fs: Some(0), cs: None, fs_span: None },
        );
        let bytes = blob.to_bytes();
        assert_eq!(bytes.len(), HEADER_SIZE + 64);
        assert_eq!(&bytes[..8], MAGIC);
        let parsed = ShaderBlob::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, blob);
    }

    #[test]
    fn round_trips_all_stages_and_offsets() {
        let blob = sample(
            vec![0u8; 256],
            EntryOffsets { vs: Some(0), fs: Some(64), cs: Some(252), fs_span: None },
        );
        let parsed = ShaderBlob::from_bytes(&blob.to_bytes()).unwrap();
        assert_eq!(parsed, blob);
    }

    #[test]
    fn header_is_16_aligned() {
        assert_eq!(HEADER_SIZE % 16, 0);
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(ShaderBlob::from_bytes(&[0u8; 10]),
                   Err(ParseError::Truncated));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample(vec![0u8; 16],
            EntryOffsets { fs: Some(0), ..Default::default() }).to_bytes();
        bytes[0] = b'X';
        assert_eq!(ShaderBlob::from_bytes(&bytes), Err(ParseError::BadMagic));
    }

    #[test]
    fn rejects_version_mismatch() {
        let mut bytes = sample(vec![0u8; 16],
            EntryOffsets { fs: Some(0), ..Default::default() }).to_bytes();
        bytes[8] = 99;
        assert_eq!(
            ShaderBlob::from_bytes(&bytes),
            Err(ParseError::VersionMismatch { found: 99, expected: VERSION }),
        );
    }

    #[test]
    fn rejects_size_mismatch() {
        let mut bytes = sample(vec![0u8; 16],
            EntryOffsets { fs: Some(0), ..Default::default() }).to_bytes();
        bytes.push(0); // one byte too many
        assert!(matches!(ShaderBlob::from_bytes(&bytes),
                         Err(ParseError::SizeMismatch { .. })));
    }

    #[test]
    fn rejects_entry_past_code() {
        let blob = sample(vec![0u8; 16],
            EntryOffsets { fs: Some(16), ..Default::default() });
        // fs offset == code_len → out of range.
        assert!(matches!(ShaderBlob::from_bytes(&blob.to_bytes()),
                         Err(ParseError::EntryOutOfRange { .. })));
    }

    #[test]
    fn rejects_misaligned_entry() {
        let blob = sample(vec![0u8; 16],
            EntryOffsets { fs: Some(2), ..Default::default() });
        assert!(matches!(ShaderBlob::from_bytes(&blob.to_bytes()),
                         Err(ParseError::EntryOutOfRange { .. })));
    }
}
