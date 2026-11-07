//! Bond records and the key store seam (docs/spec/bluetooth.md §4.1, §9).
//!
//! A [`Bond`] is what `vinculumd` keeps per paired device: the keys, how
//! strong they are, and who paired it. A [`KeyStore`] holds them:
//!
//! - [`FileStore`] on stock FreeBSD: one sealed file per bond (AES-256-GCM
//!   under a machine key held apart from the store).
//! - [`arca::ArcaStore`] on Atrium (feature `atrium`): the Arca machine
//!   store, item kind 7, class "after boot" (arca.md §6.4).
//! - [`MemoryStore`] for tests and development runs.
//!
//! `vinculumd` lists and loads every bond once at start and keeps them in
//! memory, since the controller asks for keys without time for a round
//! trip; new bonds are stored at once and removed bonds deleted (§9).
#![forbid(unsafe_code)]

mod file;
mod record;

#[cfg(feature = "atrium")]
pub mod arca;

pub use file::{load_or_create_machine_key, FileStore};
pub use record::{Address, Bond, DecodeError};

/// A stored bond's id, assigned by the store.
pub type Id = [u8; 16];

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A stored item that does not open (wrong key, tampered) or parse.
    Corrupt(&'static str),
    /// The store refused (Arca: no machine store open, or not ours).
    Refused(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Corrupt(w) => write!(f, "corrupt bond: {w}"),
            Error::Refused(w) => write!(f, "key store refused: {w}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

pub trait KeyStore {
    /// Every bond. An item that fails to open is reported, not skipped
    /// silently, so the caller can say which device must be paired again.
    fn load_all(&mut self) -> Result<Vec<(Id, Result<Bond, Error>)>, Error>;
    fn store(&mut self, bond: &Bond) -> Result<Id, Error>;
    fn delete(&mut self, id: &Id) -> Result<(), Error>;
}

/// Bonds in memory only.
#[derive(Default)]
pub struct MemoryStore {
    items: Vec<(Id, Vec<u8>)>,
    next: u128,
}

impl KeyStore for MemoryStore {
    fn load_all(&mut self) -> Result<Vec<(Id, Result<Bond, Error>)>, Error> {
        Ok(self.items.iter().map(|(id, b)| (*id, Bond::decode(b).map_err(|e| Error::Corrupt(e.0)))).collect())
    }
    fn store(&mut self, bond: &Bond) -> Result<Id, Error> {
        self.next += 1;
        let id = self.next.to_le_bytes();
        self.items.push((id, bond.encode()));
        Ok(id)
    }
    fn delete(&mut self, id: &Id) -> Result<(), Error> {
        self.items.retain(|(i, _)| i != id);
        Ok(())
    }
}

/// Finds the bond for a peer seen at `addr`: by the address it paired
/// from, its identity address, or by resolving a resolvable private address
/// with its IRK (Core 6.0 Vol 6 Part B §1.3.2.3).
pub fn find<'a>(bonds: impl IntoIterator<Item = &'a Bond>, addr: &Address) -> Option<&'a Bond> {
    bonds.into_iter().find(|b| b.address == *addr || b.identity.as_ref() == Some(addr) || b.resolves(addr))
}
