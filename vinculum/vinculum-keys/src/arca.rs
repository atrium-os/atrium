//! The Atrium key store: Arca's machine store (arca.md §6.4).
//!
//! `vinculumd` (uid `_vinculum`, named in arcad's `ARCA_SERVICES`) talks to
//! `/atrium/sockets/arca/arcad.sock` with arcad's wire: a request is
//! `u32 LE length | u16 LE op | body` (length counts op and body), a reply
//! `u32 LE length | u8 status | body`. Bonds are bearer items of kind 7
//! (BluetoothKey), class 1 (after boot); the account string is a label
//! (`bond <address>`), never key material.
//!
//! LOCKED (no machine store open) fails closed: no bonds, no pairing (§9).
//! This module speaks the wire itself, so the crate keeps no dependency on
//! Atrium's crates and the stock-FreeBSD build is unaffected.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use zeroize::Zeroize;

use crate::{Bond, Error, Id, KeyStore};

pub const SOCKET: &str = "/atrium/sockets/arca/arcad.sock";

const OP_STORE: u16 = 0x04;
const OP_RELEASE: u16 = 0x05;
const OP_LIST: u16 = 0x06;
const OP_DELETE: u16 = 0x07;

const KIND_BLUETOOTH_KEY: u16 = 7;
const CLASS_AFTER_BOOT: u8 = 1;

const OK: u8 = 0;
const DENIED: u8 = 1;
const LOCKED: u8 = 2;

const MAX_FRAME: usize = 64 * 1024;
const MAX_ACCOUNT: usize = 256;
const MAX_VALUE: usize = 16 * 1024;

/// arcad drops a client that stays idle past its receive timeout, and bond
/// writes come minutes or days apart, so the connection is made again when
/// it is gone rather than held open.
pub struct ArcaStore {
    sock: Option<UnixStream>,
    /// Where to connect again; None for a stream handed in.
    path: Option<std::path::PathBuf>,
}

impl ArcaStore {
    pub fn connect(path: &Path) -> Result<ArcaStore, Error> {
        let sock = Self::open(path)?;
        Ok(ArcaStore { sock: Some(sock), path: Some(path.to_path_buf()) })
    }

    fn open(path: &Path) -> Result<UnixStream, Error> {
        let sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        sock.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(sock)
    }

    /// Wraps an already connected stream (tests; a socket passed in).
    pub fn from_stream(sock: UnixStream) -> ArcaStore {
        ArcaStore { sock: Some(sock), path: None }
    }

    fn stream(&mut self) -> Result<&mut UnixStream, Error> {
        if self.sock.is_none() {
            let Some(p) = &self.path else {
                return Err(Error::Io(std::io::Error::from(std::io::ErrorKind::NotConnected)));
            };
            self.sock = Some(Self::open(p)?);
        }
        Ok(self.sock.as_mut().expect("just set"))
    }

    fn call(&mut self, op: u16, body: &[u8]) -> Result<Vec<u8>, Error> {
        let n = 2 + body.len();
        let mut req = Vec::with_capacity(4 + n);
        req.extend_from_slice(&(n as u32).to_le_bytes());
        req.extend_from_slice(&op.to_le_bytes());
        req.extend_from_slice(body);
        // A write that fails means arcad closed this connection before it
        // read the request (an idle timeout), so nothing was done and the
        // request is sent once more on a fresh connection. A failure after
        // the request went out is not retried: STORE is not idempotent.
        let mut w = self.stream().and_then(|s| s.write_all(&req).map_err(Error::from));
        if w.is_err() && self.path.is_some() {
            self.sock = None;
            w = self.stream().and_then(|s| s.write_all(&req).map_err(Error::from));
        }
        req.zeroize();
        if let Err(e) = w {
            self.sock = None;
            return Err(e);
        }
        let r = self.reply();
        if r.is_err() {
            // The stream may be mid-frame: never reuse it.
            self.sock = None;
        }
        r
    }

    fn reply(&mut self) -> Result<Vec<u8>, Error> {
        let s = self.stream()?;
        let mut len = [0u8; 4];
        s.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len) as usize;
        if n == 0 || n > MAX_FRAME {
            return Err(Error::Corrupt("arcad reply length"));
        }
        let mut reply = vec![0u8; n];
        s.read_exact(&mut reply)?;
        match reply[0] {
            OK => {
                reply.remove(0);
                Ok(reply)
            }
            LOCKED => Err(Error::Refused("no machine store open")),
            DENIED => Err(Error::Refused("denied")),
            _ => Err(Error::Refused("arcad error")),
        }
    }
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Corrupt("short arcad reply"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2")))
    }
    fn bytes(&mut self, cap: usize) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        if n > cap {
            return Err(Error::Corrupt("arcad field over its cap"));
        }
        self.take(n)
    }
}

impl KeyStore for ArcaStore {
    fn load_all(&mut self) -> Result<Vec<(Id, Result<Bond, Error>)>, Error> {
        let list = self.call(OP_LIST, &[])?;
        let mut r = Rd(&list);
        let n = r.u32()?;
        let mut ids = Vec::new();
        for _ in 0..n {
            let id: Id = r.take(16)?.try_into().expect("16");
            let kind = r.u16()?;
            let _class = r.take(1)?;
            let _flags = r.u32()?;
            let _account = r.bytes(MAX_ACCOUNT)?;
            if kind == KIND_BLUETOOTH_KEY {
                ids.push(id);
            }
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let mut v = self.call(OP_RELEASE, &id)?;
            let res = (|| {
                let mut r = Rd(&v);
                let value = r.bytes(MAX_VALUE)?;
                Bond::decode(value).map_err(|e| Error::Corrupt(e.0))
            })();
            v.zeroize();
            out.push((id, res));
        }
        Ok(out)
    }

    fn store(&mut self, bond: &Bond) -> Result<Id, Error> {
        let account = format!("bond {}", bond.address);
        let mut value = bond.encode();
        let mut body = Vec::with_capacity(16 + account.len() + value.len());
        body.extend_from_slice(&KIND_BLUETOOTH_KEY.to_le_bytes());
        body.push(CLASS_AFTER_BOOT);
        body.extend_from_slice(&(account.len() as u32).to_le_bytes());
        body.extend_from_slice(account.as_bytes());
        body.extend_from_slice(&(value.len() as u32).to_le_bytes());
        body.extend_from_slice(&value);
        value.zeroize();
        let r = self.call(OP_STORE, &body);
        body.zeroize();
        let reply = r?;
        reply.get(..16).and_then(|s| s.try_into().ok()).ok_or(Error::Corrupt("STORE reply"))
    }

    fn delete(&mut self, id: &Id) -> Result<(), Error> {
        self.call(OP_DELETE, id).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Address;
    use std::os::unix::net::UnixListener;

    fn bond() -> Bond {
        Bond {
            address: Address { kind: 0, bytes: [1, 2, 3, 4, 5, 6] },
            classic: false,
            identity: None,
            ltk: [7; 16],
            key_size: 16,
            secure_connections: true,
            authenticated: true,
            peer_irk: None,
            paired_by: 1000,
            name: "peer".into(),
        }
    }

    /// arcad closed the idle connection before the bond came: the write
    /// fails and the store goes through on a fresh connection.
    #[test]
    fn store_reconnects_after_arcad_drops_an_idle_connection() {
        let dir = std::env::temp_dir().join(format!("vk-arca-{}", std::process::id()));
        let _ = std::fs::create_dir(&dir);
        let path = dir.join("arcad.sock");
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).expect("bind");
        let mut store = ArcaStore::connect(&path).expect("connect");
        drop(l.accept().expect("accept")); // the idle timeout
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().expect("second accept");
            let mut len = [0u8; 4];
            s.read_exact(&mut len).expect("len");
            let mut req = vec![0u8; u32::from_le_bytes(len) as usize];
            s.read_exact(&mut req).expect("req");
            assert_eq!(u16::from_le_bytes([req[0], req[1]]), OP_STORE);
            let mut reply = vec![17, 0, 0, 0, OK];
            reply.extend_from_slice(&[9; 16]);
            s.write_all(&reply).expect("reply");
        });
        let id = store.store(&bond()).expect("store after reconnect");
        assert_eq!(id, [9; 16]);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
