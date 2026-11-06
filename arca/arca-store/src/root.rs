//! The user root key (arca.md §6.1): what Vestibulum hands Arca at unlock.
//!
//! On a machine with a TPM the root key is sealed to it (phase A4). Without
//! one (the dev VM, some laptops) it is derived from the passcode with
//! Argon2id alone at a high cost, which is weaker against a stolen disk;
//! setup says so. Vestibulum owns the passcode, the salt and the
//! parameters; Arca only ever sees the 32-byte result.

use std::io;

use argon2::{Algorithm, Argon2, Params, Version};

use crate::secret::LockedBuf;
use crate::Error;

pub const ROOT_KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 16;

/// The user root key, in locked memory.
#[derive(Debug)]
pub struct RootKey(pub(crate) LockedBuf);

impl RootKey {
    /// Wrap 32 bytes received from Vestibulum. The caller zeroizes `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Result<RootKey, Error> {
        if bytes.len() != ROOT_KEY_LEN {
            return Err(Error::BadInput("root key length"));
        }
        Ok(RootKey(LockedBuf::from_slice(bytes)?))
    }

    /// Derive from a passcode (no-TPM path). `passcode` is the caller's;
    /// the caller keeps it in locked memory and zeroizes it.
    pub fn derive(passcode: &[u8], salt: &[u8; SALT_LEN], kdf: KdfParams) -> Result<RootKey, Error> {
        let params = Params::new(kdf.m_kib, kdf.t_cost, kdf.p_cost, Some(ROOT_KEY_LEN))
            .map_err(|_| Error::BadInput("argon2 params"))?;
        let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = LockedBuf::zeroed(ROOT_KEY_LEN)?;
        a.hash_password_into(passcode, salt, out.as_mut()).map_err(|_| Error::BadInput("argon2"))?;
        Ok(RootKey(out))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }
}

/// Argon2id cost. [`KdfParams::NO_TPM`] is the shipped default for machines
/// without a TPM; tests use [`KdfParams::TEST`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl KdfParams {
    /// 256 MiB, 3 passes: well above OWASP's Argon2id floor (19 MiB, 2
    /// passes), around a second per login on the dev VM, and memory-hard
    /// enough that GPU guessing is expensive. It is computed by the login
    /// process (ostiarius) at every unlock, so it must fit that jail's
    /// memory. Raised, never lowered, over time: the parameters are stored
    /// per user, so a raise applies at the next passcode change.
    pub const NO_TPM: KdfParams = KdfParams { m_kib: 256 * 1024, t_cost: 3, p_cost: 1 };
    /// Only for tests: cheap, never written by a shipped build.
    pub const TEST: KdfParams = KdfParams { m_kib: 64, t_cost: 1, p_cost: 1 };
}

/// A fresh random salt.
pub fn new_salt() -> io::Result<[u8; SALT_LEN]> {
    let mut s = [0u8; SALT_LEN];
    getrandom::getrandom(&mut s).map_err(io::Error::other)?;
    Ok(s)
}
