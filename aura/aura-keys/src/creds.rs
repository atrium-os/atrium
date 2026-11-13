//! Where Wi-Fi credentials live: Arca's machine store (arca.md §6.4,
//! wifi.md decision 7), kind `WifiCredential` (8), class "after boot". The
//! account string is `wifi <ssid hex>`; the value is the passphrase (or the
//! 64-hex-digit PSK). aura-keys (uid `_aura-keys`, named in arcad's
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

pub trait Store {
    fn remember(&mut self, ssid: &[u8], secret: &[u8]) -> io::Result<()>;
    fn forget(&mut self, ssid: &[u8]) -> io::Result<bool>;
    fn release(&mut self, ssid: &[u8]) -> io::Result<Option<Secret>>;
}

fn account(ssid: &[u8]) -> String {
    format!("wifi {}", aura_ipc::hex(ssid))
}

#[derive(Default)]
pub struct Memory(Vec<(Vec<u8>, Secret)>);

impl Store for Memory {
    fn remember(&mut self, ssid: &[u8], secret: &[u8]) -> io::Result<()> {
        self.0.retain(|(s, _)| s != ssid);
        self.0.push((ssid.to_vec(), Zeroizing::new(secret.to_vec())));
        Ok(())
    }
    fn forget(&mut self, ssid: &[u8]) -> io::Result<bool> {
        let n = self.0.len();
        self.0.retain(|(s, _)| s != ssid);
        Ok(self.0.len() != n)
    }
    fn release(&mut self, ssid: &[u8]) -> io::Result<Option<Secret>> {
        Ok(self.0.iter().find(|(s, _)| s == ssid).map(|(_, v)| v.clone()))
    }
}

const OP_STORE: u16 = 0x04;
const OP_RELEASE: u16 = 0x05;
const OP_LIST: u16 = 0x06;
const OP_DELETE: u16 = 0x07;
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
            if kind == KIND_WIFI {
                out.push((id, acc));
            }
        }
        Ok(out)
    }
}

impl Store for Arca {
    fn remember(&mut self, ssid: &[u8], secret: &[u8]) -> io::Result<()> {
        self.forget(ssid)?;
        let acc = account(ssid);
        let mut body = Zeroizing::new(Vec::new());
        body.extend_from_slice(&KIND_WIFI.to_le_bytes());
        body.push(CLASS_AFTER_BOOT);
        body.extend_from_slice(&(acc.len() as u32).to_le_bytes());
        body.extend_from_slice(acc.as_bytes());
        body.extend_from_slice(&(secret.len() as u32).to_le_bytes());
        body.extend_from_slice(secret);
        self.call(OP_STORE, &body).map(|_| ())
    }

    fn forget(&mut self, ssid: &[u8]) -> io::Result<bool> {
        let acc = account(ssid);
        let mut any = false;
        for (id, a) in self.list()? {
            if a == acc {
                self.call(OP_DELETE, &id)?;
                any = true;
            }
        }
        Ok(any)
    }

    fn release(&mut self, ssid: &[u8]) -> io::Result<Option<Secret>> {
        let acc = account(ssid);
        let Some((id, _)) = self.list()?.into_iter().find(|(_, a)| *a == acc) else {
            return Ok(None);
        };
        let mut v = self.call(OP_RELEASE, &id)?;
        if v.len() < 4 {
            return Err(io::Error::other("short RELEASE reply"));
        }
        let n = u32::from_le_bytes([v[0], v[1], v[2], v[3]]) as usize;
        if v.len() != 4 + n {
            v.zeroize();
            return Err(io::Error::other("bad RELEASE reply"));
        }
        Ok(Some(Zeroizing::new(v[4..].to_vec())))
    }
}
