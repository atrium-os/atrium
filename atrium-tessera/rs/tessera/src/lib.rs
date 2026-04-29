//! Safe Rust wrapper over [`tessera_sys`].
//!
//! Phase 0 exposes only the error type and a SHA-256 convenience.
//! Subsequent phases add codec, manifest, pack, and volume APIs.

use core::fmt;

pub use tessera_sys as sys;

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn is_null(&self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    pub const OK:       Errno = Errno(sys::TESSERA_OK);
    pub const NOTIMPL:  Errno = Errno(sys::TESSERA_ENOTIMPL);
}
