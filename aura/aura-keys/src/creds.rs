//! Where Wi-Fi credentials live: Arca's machine store (arca.md §6.4,
//! wifi.md decision 7), kind `WifiCredential` (8), class "after boot". The
//! account string is `wifi <ssid hex>`; the value is the passphrase (or the
//! 64-hex-digit PSK). One more item, `aura mac-key`, holds the key that
//! per-network MAC addresses are derived from, so they stay the same across
//! reboots. An EAP-TLS network's client key is a `TlsClientKey` (P-256)
//! minted in Arca under the network's account plus " tls"; Arca signs with
//! it and never releases it (wifi.md §7.4 E3). aura-keys (uid `_aura-keys`, named in arcad's
//! `ARCA_SERVICES` as `aura-keys`) is the only reader and writer.
//!
//! `Memory` keeps credentials in this process only: for the sim gate before
//! arcad runs there, never for a real machine.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

pub type Secret = Zeroizing<Vec<u8>>;

/// Items by account string.
pub trait Store {
    fn remember(&mut self, acc: &str, secret: &[u8]) -> io::Result<()>;
    fn forget(&mut self, acc: &str) -> io::Result<bool>;
    fn release(&mut self, acc: &str) -> io::Result<Option<Secret>>;
    /// What the user typed into Arca's Wi-Fi passphrase field, by the
    /// handle Settings was given (curia.md §4): once, and only from Arca.
    fn take_typed(&mut self, _handle: &[u8; 16]) -> io::Result<Secret> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no Arca"))
    }
    /// A new EAP-TLS key (P-256) that never leaves Arca: (id, SEC1 point).
    fn mint_tls(&mut self, _acc: &str) -> io::Result<([u8; 16], Vec<u8>)> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no Arca"))
    }
    /// The SEC1 point of an EAP-TLS key.
    fn tls_public(&mut self, _id: &[u8; 16]) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no Arca"))
    }
    /// A DER ECDSA signature over a SHA-256 digest with an EAP-TLS key.
    fn tls_sign(&mut self, _id: &[u8; 16], _digest: &[u8; 32]) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no Arca"))
    }
}

pub fn account(ssid: &[u8]) -> String {
    format!("wifi {}", aura_ipc::hex(ssid))
}

/// Whose 802.1X profile: a Wi-Fi network, or the machine's one wired
/// profile (wifi.md §7.5), used on whichever Ethernet port asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Net {
    Wifi(Vec<u8>),
    Wired,
}

impl Net {
    /// From a ctl line: `wired=1`, or `ssid=..` (1 to 32 bytes); never both.
    pub fn from_line(l: &aura_ipc::Line) -> Option<Net> {
        match (l.get("wired"), l.get("ssid")) {
            (Some("1"), None) => Some(Net::Wired),
            (None, Some(_)) => l.hex("ssid").filter(|s| !s.is_empty() && s.len() <= 32).map(Net::Wifi),
            _ => None,
        }
    }

    pub fn account(&self) -> String {
        match self {
            Net::Wifi(ssid) => account(ssid),
            Net::Wired => "wired".into(),
        }
    }

    /// The account of an EAP-TLS profile's client key.
    pub fn tls_account(&self) -> String {
        format!("{} tls", self.account())
    }
}

pub const MAC_KEY: &str = "aura mac-key";

#[derive(Default)]
pub struct Memory(Vec<(String, Secret)>);

impl Store for Memory {
    fn remember(&mut self, acc: &str, secret: &[u8]) -> io::Result<()> {
        self.0.retain(|(s, _)| s != acc);
        self.0.push((acc.to_string(), Zeroizing::new(secret.to_vec())));
        Ok(())
    }
    fn forget(&mut self, acc: &str) -> io::Result<bool> {
        let n = self.0.len();
        self.0.retain(|(s, _)| s != acc);
        Ok(self.0.len() != n)
    }
    fn release(&mut self, acc: &str) -> io::Result<Option<Secret>> {
        Ok(self.0.iter().find(|(s, _)| s == acc).map(|(_, v)| v.clone()))
    }
}

const OP_MINT: u16 = 0x01;
const OP_PUBKEY: u16 = 0x02;
const OP_SIGN_DIGEST: u16 = 0x0F;
const KIND_TLS_KEY: u16 = 0x105;
/// An uncompressed P-256 point.
const POINT_LEN: usize = 65;
const OP_STORE: u16 = 0x04;
const OP_RELEASE: u16 = 0x05;
const OP_LIST: u16 = 0x06;
const OP_DELETE: u16 = 0x07;
const OP_TAKE_WIFI: u16 = 0x0A;
const KIND_WIFI: u16 = 8;
const CLASS_AFTER_BOOT: u8 = 1;
const MAX_FRAME: usize = 64 * 1024;

/// Arca's machine store, over arcad's socket. Connects per operation: joins
/// are rare and arcad may restart in between.
pub struct Arca {
    path: std::path::PathBuf,
}

impl Arca {
    pub fn new(path: &Path) -> Arca {
        Arca { path: path.into() }
    }

    fn call(&self, op: u16, body: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
        let mut s = UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        let n = 2 + body.len();
        let mut req = Zeroizing::new(Vec::with_capacity(4 + n));
        req.extend_from_slice(&(n as u32).to_le_bytes());
        req.extend_from_slice(&op.to_le_bytes());
        req.extend_from_slice(body);
        s.write_all(&req)?;
        let mut len = [0u8; 4];
        s.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len) as usize;
        if n == 0 || n > MAX_FRAME {
            return Err(io::Error::other("arcad reply length"));
        }
        let mut reply = Zeroizing::new(vec![0u8; n]);
        s.read_exact(&mut reply)?;
        match reply[0] {
            0 => Ok(Zeroizing::new(reply[1..].to_vec())),
            1 => Err(io::Error::new(io::ErrorKind::PermissionDenied, "arcad: denied")),
            2 => Err(io::Error::other("arcad: machine store locked")),
            x => Err(io::Error::other(format!("arcad: status {x}"))),
        }
    }

    /// (id, account) of our Wi-Fi items.
    fn list(&self) -> io::Result<Vec<([u8; 16], String)>> {
        let b = self.call(OP_LIST, &[])?;
        let short = || io::Error::other("short arcad LIST reply");
        let mut p = &b[..];
        let mut take = |n: usize| -> io::Result<Vec<u8>> {
            if p.len() < n {
                return Err(short());
            }
            let (a, r) = p.split_at(n);
            p = r;
            Ok(a.to_vec())
        };
        let n = u32::from_le_bytes(take(4)?.try_into().map_err(|_| short())?);
        let mut out = Vec::new();
        for _ in 0..n.min(4096) {
            let id: [u8; 16] = take(16)?.try_into().map_err(|_| short())?;
            let kind = u16::from_le_bytes(take(2)?.try_into().map_err(|_| short())?);
            take(1 + 4)?;
            let alen = u32::from_le_bytes(take(4)?.try_into().map_err(|_| short())?) as usize;
            if alen > 256 {
                return Err(short());
            }
            let acc = String::from_utf8_lossy(&take(alen)?).into_owned();
            if kind == KIND_WIFI || kind == KIND_TLS_KEY {
                out.push((id, acc));
            }
        }
        Ok(out)
    }
}

/// `bytes value` out of a RELEASE or TAKE_WIFI reply.
fn value(mut v: Zeroizing<Vec<u8>>) -> io::Result<Secret> {
    if v.len() < 4 {
        return Err(io::Error::other("short arcad reply"));
    }
    let n = u32::from_le_bytes([v[0], v[1], v[2], v[3]]) as usize;
    if v.len() != 4 + n {
        v.zeroize();
        return Err(io::Error::other("bad arcad reply"));
    }
    Ok(Zeroizing::new(v[4..].to_vec()))
}

impl Store for Arca {
    fn remember(&mut self, acc: &str, secret: &[u8]) -> io::Result<()> {
        self.forget(acc)?;
        let mut body = Zeroizing::new(Vec::new());
        body.extend_from_slice(&KIND_WIFI.to_le_bytes());
        body.push(CLASS_AFTER_BOOT);
        body.extend_from_slice(&(acc.len() as u32).to_le_bytes());
        body.extend_from_slice(acc.as_bytes());
        body.extend_from_slice(&(secret.len() as u32).to_le_bytes());
        body.extend_from_slice(secret);
        self.call(OP_STORE, &body).map(|_| ())
    }

    fn forget(&mut self, acc: &str) -> io::Result<bool> {
        let mut any = false;
        for (id, a) in self.list()? {
            if a == *acc {
                self.call(OP_DELETE, &id)?;
                any = true;
            }
        }
        Ok(any)
    }

    fn release(&mut self, acc: &str) -> io::Result<Option<Secret>> {
        let Some((id, _)) = self.list()?.into_iter().find(|(_, a)| *a == acc) else {
            return Ok(None);
        };
        value(self.call(OP_RELEASE, &id)?).map(Some)
    }

    fn take_typed(&mut self, handle: &[u8; 16]) -> io::Result<Secret> {
        value(self.call(OP_TAKE_WIFI, handle)?)
    }

    fn mint_tls(&mut self, acc: &str) -> io::Result<([u8; 16], Vec<u8>)> {
        self.forget(acc)?;
        let mut body = Vec::new();
        body.extend_from_slice(&KIND_TLS_KEY.to_le_bytes());
        body.push(CLASS_AFTER_BOOT);
        body.extend_from_slice(&(acc.len() as u32).to_le_bytes());
        body.extend_from_slice(acc.as_bytes());
        let r = self.call(OP_MINT, &body)?;
        if r.len() != 16 + POINT_LEN || r[16] != 4 {
            return Err(io::Error::other("bad arcad MINT reply"));
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&r[..16]);
        Ok((id, r[16..].to_vec()))
    }

    fn tls_public(&mut self, id: &[u8; 16]) -> io::Result<Vec<u8>> {
        let r = self.call(OP_PUBKEY, id)?;
        if r.len() != POINT_LEN || r[0] != 4 {
            return Err(io::Error::other("bad arcad PUBKEY reply"));
        }
        Ok(r.to_vec())
    }

    fn tls_sign(&mut self, id: &[u8; 16], digest: &[u8; 32]) -> io::Result<Vec<u8>> {
        let mut body = id.to_vec();
        body.extend_from_slice(digest);
        let r = self.call(OP_SIGN_DIGEST, &body)?;
        // A DER ECDSA-P256 signature is at most 72 bytes.
        if r.is_empty() || r.len() > 72 || r[0] != 0x30 {
            return Err(io::Error::other("bad arcad signature"));
        }
        Ok(r.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_ipc::Line;

    #[test]
    fn wired_and_wifi_accounts_differ() {
        let w = Net::from_line(&Line::new("x").with("wired", 1)).unwrap();
        assert_eq!((w.account(), w.tls_account()), ("wired".to_string(), "wired tls".to_string()));
        let n = Net::from_line(&Line::new("x").with_hex("ssid", b"corp")).unwrap();
        assert_eq!(n.account(), "wifi 636f7270");
        assert!(Net::from_line(&Line::new("x").with("wired", 1).with_hex("ssid", b"corp")).is_none(), "one or the other");
        assert!(Net::from_line(&Line::new("x").with("wired", 0)).is_none());
        assert!(Net::from_line(&Line::new("x").with("ssid", "-")).is_none(), "an empty SSID");
        assert!(Net::from_line(&Line::new("x").with_hex("ssid", &[1; 33])).is_none());
    }
}
