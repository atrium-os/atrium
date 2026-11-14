//! Arca's at-rest store (docs/spec/arca.md §6).
//!
//! - [`secret`]: locked, non-dumpable, zeroized memory for clear secrets.
//! - [`root`]: the user root key Vestibulum hands over at unlock.
//! - [`model`]: classes, kinds, recipients, item metadata.
//! - [`store`]: the key hierarchy, recipient groups, lock eviction.
//!
//! The store releases an item only to the recipient it was bound to at
//! creation, compared exactly; deciding *who* is asking is arcad's job
//! (arca.md §4.1), and arcad passes in what the system verified.

pub mod codec;
pub mod model;
pub mod root;
pub mod secret;
pub mod store;

pub use model::{flags, Class, ItemId, ItemMeta, Kind, NewItem, Provenance, Recipient, Scheme, Shape, ShareProto};
pub use root::{KdfParams, RootKey};
pub use store::{Access, LockState, Store, UserVerified};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A file is malformed. Names the check, never any content.
    Corrupt(&'static str),
    /// An AEAD open failed: wrong key, wrong binding or tampering.
    Unsealed,
    WrongRootKey,
    /// The class key isn't available in the current lock state.
    Locked,
    /// A verify-each-use item needs a fresh Vestibulum check.
    NeedsVerification,
    /// No such item for this asker (also: it exists but isn't theirs).
    Denied,
    BadRecipient,
    BadInput(&'static str),
    Exists,
    Full,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}
