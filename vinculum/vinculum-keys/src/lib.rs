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

    /// Deletes every stored item not in `keep`: after a store whose reply
    /// was lost, the item may exist although the pairing was reported
    /// failed, and it must not be honoured at the next start.
    fn forget_unknown(&mut self, keep: &[Id]) -> Result<usize, Error> {
        let mut n = 0;
        for (id, _) in self.load_all()? {
            if !keep.contains(&id) {
                self.delete(&id)?;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// Splits loaded bonds into the usable ones and the ids to delete, for
/// records that share an address or identity. Records of one device with
/// the same key are copies (a rename stores the new record before deleting
/// the old one): one is kept, a named one first. Records with different
/// keys can't be told apart, so none is used and that device pairs again.
pub fn split_duplicates(bonds: Vec<(Id, Bond)>) -> (Vec<(Id, Bond)>, Vec<Id>) {
    let same = |a: &Bond, b: &Bond| a.address == b.address || (a.identity.is_some() && a.identity == b.identity);
    let n = bonds.len();
    // Group each record with every record it shares a device with.
    let mut group: Vec<usize> = (0..n).collect();
    for i in 0..n {
        for j in 0..i {
            if same(&bonds[i].1, &bonds[j].1) {
                let (gi, gj) = (group[i], group[j]);
                for g in group.iter_mut() {
                    if *g == gi {
                        *g = gj;
                    }
                }
            }
        }
    }
    let mut keep_idx = vec![false; n];
    for g in 0..n {
        let members: Vec<usize> = (0..n).filter(|&i| group[i] == g).collect();
        let Some(&first) = members.first() else { continue };
        let copies = members.iter().all(|&i| {
            let (a, b) = (&bonds[i].1, &bonds[first].1);
            a.paired_by == b.paired_by && a.ltk == b.ltk && a.classic == b.classic && a.key_size == b.key_size && a.authenticated == b.authenticated && a.secure_connections == b.secure_connections && a.peer_irk == b.peer_irk
        });
        if copies {
            // A crash between permission writes can leave both records. Keep
            // the ungranted one even if the granted copy has a better name.
            let pick = members.iter().copied().min_by_key(|&i| {
                (bonds[i].1.audio_allowed, bonds[i].1.name.trim().is_empty())
            }).unwrap_or(first);
            keep_idx[pick] = true;
        }
    }
    let mut keep = Vec::new();
    let mut drop = Vec::new();
    for ((id, b), k) in bonds.into_iter().zip(keep_idx) {
        if k { keep.push((id, b)) } else { drop.push(id) }
    }
    (keep, drop)
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
