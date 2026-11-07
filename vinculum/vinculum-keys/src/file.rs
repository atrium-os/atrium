//! The stock-FreeBSD key store (spec §4.1): `/var/db/vinculum/keys`, one
//! file per bond, mode 0600, each sealed with AES-256-GCM under a machine
//! key kept in a separate file (root-only on a real install; the rc.d
//! script hands it to vinculumd, which never writes it).
//!
//! A file is `nonce[12] | ciphertext | tag[16]`, with the bond's id and a
//! format label as associated data, so a file renamed onto another id, or
//! moved between stores, does not open. Writes go to a temporary file,
//! fsync, then rename, so a crash leaves the old bond or the new one.
//!
//! What it protects: copies of the store directory (backups, a disk read
//! offline without the key file). Not root on the running machine, which
//! can read both; the Atrium build uses Arca's TPM-sealed machine store.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand_core::{OsRng, RngCore};
use zeroize::Zeroize;

use crate::{Bond, Error, Id, KeyStore};

const AAD_LABEL: &[u8] = b"vinculum-bond-v1";
/// More files than this in the directory is not a store Vinculum wrote.
const MAX_BONDS: usize = 1024;
const MAX_FILE: u64 = 4096;

pub struct FileStore {
    dir: PathBuf,
    cipher: Aes256Gcm,
}

/// Reads the 32-byte machine key at `path`, or makes one (0600, written
/// once with `create_new`) if there is none. A file of the wrong size is an
/// error, never replaced: losing the key must fail closed.
pub fn load_or_create_machine_key(path: &Path) -> Result<[u8; 32], Error> {
    match File::open(path) {
        Ok(mut f) => {
            let mut k = [0u8; 32];
            let mut extra = [0u8; 1];
            f.read_exact(&mut k).map_err(|_| Error::Corrupt("machine key shorter than 32 bytes"))?;
            if f.read(&mut extra)? != 0 {
                k.zeroize();
                return Err(Error::Corrupt("machine key longer than 32 bytes"));
            }
            Ok(k)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut k = [0u8; 32];
            OsRng.fill_bytes(&mut k);
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            f.write_all(&k)?;
            f.sync_all()?;
            Ok(k)
        }
        Err(e) => Err(e.into()),
    }
}

impl FileStore {
    /// Opens (making if needed, mode 0700) the store in `dir`. The key is
    /// zeroed by the caller after this returns.
    pub fn open(dir: &Path, key: &[u8; 32]) -> Result<FileStore, Error> {
        match fs::create_dir(dir) {
            Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let cipher = Aes256Gcm::new_from_slice(key).expect("32-byte key");
        Ok(FileStore { dir: dir.into(), cipher })
    }

    fn path(&self, id: &Id) -> PathBuf {
        self.dir.join(hex(id))
    }

    fn aad(id: &Id) -> Vec<u8> {
        [AAD_LABEL, &id[..]].concat()
    }

    fn open_file(&self, id: &Id, path: &Path) -> Result<Bond, Error> {
        let f = File::open(path)?;
        if f.metadata()?.len() > MAX_FILE {
            return Err(Error::Corrupt("file too large"));
        }
        let mut sealed = Vec::new();
        f.take(MAX_FILE).read_to_end(&mut sealed)?;
        if sealed.len() < 12 + 16 {
            return Err(Error::Corrupt("file too short"));
        }
        let (nonce, ct) = sealed.split_at(12);
        let aad = Self::aad(id);
        let mut plain = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: &aad })
            .map_err(|_| Error::Corrupt("does not open with this machine key"))?;
        let r = Bond::decode(&plain).map_err(|e| Error::Corrupt(e.0));
        plain.zeroize();
        r
    }
}

impl KeyStore for FileStore {
    fn load_all(&mut self) -> Result<Vec<(Id, Result<Bond, Error>)>, Error> {
        let mut out = Vec::new();
        for ent in fs::read_dir(&self.dir)? {
            let ent = ent?;
            let name = ent.file_name();
            let Some(id) = name.to_str().and_then(unhex) else {
                continue; // temporaries and anything not ours
            };
            if out.len() >= MAX_BONDS {
                return Err(Error::Corrupt("too many bond files"));
            }
            out.push((id, self.open_file(&id, &ent.path())));
        }
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }

    fn store(&mut self, bond: &Bond) -> Result<Id, Error> {
        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let mut plain = bond.encode();
        let aad = Self::aad(&id);
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: &aad });
        plain.zeroize();
        let ct = ct.map_err(|_| Error::Corrupt("seal failed"))?;
        let tmp = self.dir.join(format!(".{}.tmp", hex(&id)));
        {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(&nonce)?;
            f.write_all(&ct)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, self.path(&id))?;
        if let Ok(d) = File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(id)
    }

    fn delete(&mut self, id: &Id) -> Result<(), Error> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn hex(id: &Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Id> {
    if s.len() != 32 || !s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}
