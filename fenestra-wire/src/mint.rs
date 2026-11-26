//! The mint (fenestra.md §11.2): what a viewer needs to reach and prove a
//! session, `addr A host_key HEX id HEX k_sess HEX`.
//!
//! It travels as one line ([`DevMint::line`], [`DevMint::parse_line`]):
//! from `fenestra-session` to `fenestrad`, and back inside the SSH channel
//! as `FENESTRA_MINT <line>` printed by `fenestra-shell mint`. For gates and
//! for a host simulator it can also go through a directory instead: the
//! viewer leaves `viewer.pub` (its public key, hex) and the host writes
//! `mint` (the same fields, one per line). Those files are made fresh
//! (`O_EXCL`), mode 0600, and read with a bound.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;

use crate::{SessionSecret, WireError};

const MAX_FILE: u64 = 4096;
/// Longest mint line (a host key is well under 128 bytes).
pub const MAX_LINE: usize = 1024;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, WireError> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(WireError::Protocol("not hex"));
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| WireError::Protocol("not hex"))).collect()
}

fn write_new(path: &Path, text: &str) -> Result<(), WireError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

fn read_bounded(path: &Path) -> Result<String, WireError> {
    let f = std::fs::File::open(path)?;
    let mut s = String::new();
    f.take(MAX_FILE + 1).read_to_string(&mut s)?;
    if s.len() as u64 > MAX_FILE {
        return Err(WireError::Protocol("mint file too large"));
    }
    Ok(s)
}

pub fn write_viewer_key(dir: &Path, public: &[u8]) -> Result<(), WireError> {
    write_new(&dir.join("viewer.pub"), &format!("{}\n", hex(public)))
}

pub fn read_viewer_key(dir: &Path) -> Result<Vec<u8>, WireError> {
    unhex(&read_bounded(&dir.join("viewer.pub"))?)
}

#[derive(Debug, Clone)]
pub struct DevMint {
    pub addr: SocketAddr,
    pub host_key: Vec<u8>,
    pub secret: SessionSecret,
}

impl DevMint {
    pub fn write(&self, dir: &Path) -> Result<(), WireError> {
        let text = format!(
            "addr {}\nhost_key {}\nid {}\nk_sess {}\n",
            self.addr,
            hex(&self.host_key),
            hex(&self.secret.id),
            hex(&self.secret.k_sess)
        );
        // Written under a temporary name and renamed, so a reader never
        // sees half of it.
        let tmp = dir.join("mint.tmp");
        write_new(&tmp, &text)?;
        std::fs::rename(&tmp, dir.join("mint"))?;
        Ok(())
    }

    pub fn read(dir: &Path) -> Result<DevMint, WireError> {
        let text = read_bounded(&dir.join("mint"))?;
        DevMint::parse(text.lines().filter_map(|l| l.split_once(' ')))
    }

    /// The mint as one line, fields separated by spaces.
    pub fn line(&self) -> String {
        format!("addr {} host_key {} id {} k_sess {}", self.addr, hex(&self.host_key), hex(&self.secret.id), hex(&self.secret.k_sess))
    }

    /// A mint from [`DevMint::line`]'s form: every field once, nothing else.
    pub fn parse_line(line: &str) -> Result<DevMint, WireError> {
        if line.len() > MAX_LINE {
            return Err(WireError::Protocol("mint line too long"));
        }
        let w: Vec<&str> = line.split_ascii_whitespace().collect();
        if w.len() != 8 {
            return Err(WireError::Protocol("not a mint line"));
        }
        DevMint::parse(w.chunks(2).map(|p| (p[0], p[1])))
    }

    fn parse<'a>(fields: impl Iterator<Item = (&'a str, &'a str)>) -> Result<DevMint, WireError> {
        let (mut addr, mut host_key, mut id, mut k) = (None, None, None, None);
        for (key, val) in fields {
            match key {
                "addr" => once(&mut addr, val.parse::<SocketAddr>().map_err(|_| WireError::Protocol("mint address"))?)?,
                "host_key" => once(&mut host_key, unhex(val)?)?,
                "id" => once(&mut id, unhex(val)?)?,
                "k_sess" => once(&mut k, unhex(val)?)?,
                _ => return Err(WireError::Protocol("unknown mint line")),
            }
        }
        let bad = || WireError::Protocol("incomplete mint");
        let id: [u8; 16] = id.ok_or_else(bad)?.try_into().map_err(|_| bad())?;
        let k_sess: [u8; 32] = k.ok_or_else(bad)?.try_into().map_err(|_| bad())?;
        Ok(DevMint { addr: addr.ok_or_else(bad)?, host_key: host_key.ok_or_else(bad)?, secret: SessionSecret { id, k_sess } })
    }
}

fn once<T>(slot: &mut Option<T>, v: T) -> Result<(), WireError> {
    if slot.replace(v).is_some() {
        return Err(WireError::Protocol("a mint field twice"));
    }
    Ok(())
}

/// A fresh session secret.
pub fn fresh_secret() -> Result<SessionSecret, WireError> {
    let rng = ring::rand::SystemRandom::new();
    let mut s = SessionSecret { id: [0; 16], k_sess: [0; 32] };
    ring::rand::SecureRandom::fill(&rng, &mut s.id).map_err(|_| WireError::Protocol("no randomness"))?;
    ring::rand::SecureRandom::fill(&rng, &mut s.k_sess).map_err(|_| WireError::Protocol("no randomness"))?;
    Ok(s)
}
