//! The bond record and its stored encoding.
//!
//! Encoding, version 1 (all integers little-endian):
//!
//! ```text
//! u8 version (1) | u8 flags (bit 0 SC, bit 1 authenticated, bit 2 has IRK,
//!                  bit 3 has identity) | u8 key_size
//! u8 addr_type | addr[6]                      address paired from
//! u8 id_type | id[6]                          identity address (zero if none)
//! ltk[16] | irk[16]                           irk zero if none
//! u32 paired_by (uid)
//! u8 name_len | name (UTF-8, at most 64 bytes)
//! ```

use vinculum_smp::crypto::ah;
use zeroize::Zeroize;

pub const MAX_NAME: usize = 64;
const VERSION: u8 = 1;
const FIXED: usize = 3 + 7 + 7 + 16 + 16 + 4 + 1;

/// A device address: type (0 public, 1 random) and 6 bytes little-endian,
/// as SMP and the controller carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address {
    pub kind: u8,
    pub bytes: [u8; 6],
}

impl Address {
    /// From the most-significant-first form `vinculum_host::Peer` prints.
    pub fn from_msb(msb: [u8; 6], random: bool) -> Address {
        let mut b = msb;
        b.reverse();
        Address { kind: random as u8, bytes: b }
    }

    /// A resolvable private address: random, top two bits 0b01.
    pub fn is_resolvable(&self) -> bool {
        self.kind == 1 && self.bytes[5] >> 6 == 0b01
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.bytes;
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[5], b[4], b[3], b[2], b[1], b[0])?;
        if self.kind == 1 {
            write!(f, " (random)")?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Bond {
    pub address: Address,
    pub identity: Option<Address>,
    pub ltk: [u8; 16],
    pub key_size: u8,
    pub secure_connections: bool,
    pub authenticated: bool,
    pub peer_irk: Option<[u8; 16]>,
    /// The uid of the user who paired it (only they or an administrator
    /// may remove it, §9).
    pub paired_by: u32,
    pub name: String,
}

impl std::fmt::Debug for Bond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bond")
            .field("address", &self.address)
            .field("identity", &self.identity)
            .field("ltk", &"<redacted>")
            .field("key_size", &self.key_size)
            .field("secure_connections", &self.secure_connections)
            .field("authenticated", &self.authenticated)
            .field("peer_irk", &self.peer_irk.map(|_| "<redacted>"))
            .field("paired_by", &self.paired_by)
            .field("name", &self.name)
            .finish()
    }
}

impl Drop for Bond {
    fn drop(&mut self) {
        self.ltk.zeroize();
        if let Some(k) = self.peer_irk.as_mut() {
            k.zeroize();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl Bond {
    /// The record for a finished pairing with the peer at `address`.
    pub fn from_pairing(p: &vinculum_smp::pairing::Bond, address: Address, paired_by: u32, name: &str) -> Bond {
        let mut name = name.to_string();
        while name.len() > MAX_NAME {
            name.pop();
        }
        Bond {
            address,
            identity: p.peer_identity.map(|(kind, bytes)| Address { kind, bytes }),
            ltk: p.ltk,
            key_size: p.key_size,
            secure_connections: p.secure_connections,
            authenticated: p.authenticated,
            peer_irk: p.peer_irk,
            paired_by,
            name,
        }
    }

    /// Whether `addr` is a resolvable private address made from this bond's
    /// IRK.
    pub fn resolves(&self, addr: &Address) -> bool {
        let Some(irk) = &self.peer_irk else { return false };
        if !addr.is_resolvable() {
            return false;
        }
        let prand: [u8; 3] = addr.bytes[3..6].try_into().expect("3");
        ah(irk, &prand) == addr.bytes[..3]
    }

    /// The stored form. It holds keys: the caller seals it and zeroes it.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FIXED + self.name.len());
        let flags = self.secure_connections as u8
            | (self.authenticated as u8) << 1
            | (self.peer_irk.is_some() as u8) << 2
            | (self.identity.is_some() as u8) << 3;
        v.extend_from_slice(&[VERSION, flags, self.key_size, self.address.kind]);
        v.extend_from_slice(&self.address.bytes);
        let id = self.identity.unwrap_or(Address { kind: 0, bytes: [0; 6] });
        v.push(id.kind);
        v.extend_from_slice(&id.bytes);
        v.extend_from_slice(&self.ltk);
        v.extend_from_slice(&self.peer_irk.unwrap_or([0; 16]));
        v.extend_from_slice(&self.paired_by.to_le_bytes());
        let name = &self.name.as_bytes()[..self.name.len().min(MAX_NAME)];
        v.push(name.len() as u8);
        v.extend_from_slice(name);
        v
    }

    pub fn decode(b: &[u8]) -> Result<Bond, DecodeError> {
        if b.len() < FIXED {
            return Err(DecodeError("short record"));
        }
        if b[0] != VERSION {
            return Err(DecodeError("unknown record version"));
        }
        let flags = b[1];
        if flags & !0x0F != 0 {
            return Err(DecodeError("unknown flags"));
        }
        let addr = |o: usize| -> Result<Address, DecodeError> {
            if b[o] > 1 {
                return Err(DecodeError("bad address type"));
            }
            Ok(Address { kind: b[o], bytes: b[o + 1..o + 7].try_into().expect("6") })
        };
        let address = addr(3)?;
        let identity = if flags & 8 != 0 { Some(addr(10)?) } else { None };
        let ltk: [u8; 16] = b[17..33].try_into().expect("16");
        let irk: [u8; 16] = b[33..49].try_into().expect("16");
        let paired_by = u32::from_le_bytes(b[49..53].try_into().expect("4"));
        let n = b[53] as usize;
        if n > MAX_NAME || b.len() != FIXED + n {
            return Err(DecodeError("bad name length"));
        }
        let name = std::str::from_utf8(&b[FIXED..]).map_err(|_| DecodeError("name not UTF-8"))?.to_string();
        let key_size = b[2];
        if !(7..=16).contains(&key_size) {
            return Err(DecodeError("bad key size"));
        }
        Ok(Bond {
            address,
            identity,
            ltk,
            key_size,
            secure_connections: flags & 1 != 0,
            authenticated: flags & 2 != 0,
            peer_irk: (flags & 4 != 0).then_some(irk),
            paired_by,
            name,
        })
    }
}
