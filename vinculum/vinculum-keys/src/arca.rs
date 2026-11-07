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

pub struct ArcaStore {
    sock: UnixStream,
}

impl ArcaStore {
    pub fn connect(path: &Path) -> Result<ArcaStore, Error> {
        let sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        sock.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(ArcaStore { sock })
    }

    /// Wraps an already connected stream (tests; a socket passed in).
    pub fn from_stream(sock: UnixStream) -> ArcaStore {
        ArcaStore { sock }
    }

    fn call(&mut self, op: u16, body: &[u8]) -> Result<Vec<u8>, Error> {
        let n = 2 + body.len();
        let mut req = Vec::with_capacity(4 + n);
        req.extend_from_slice(&(n as u32).to_le_bytes());
        req.extend_from_slice(&op.to_le_bytes());
        req.extend_from_slice(body);
        let w = self.sock.write_all(&req);
        req.zeroize();
        w?;
        let mut len = [0u8; 4];
        self.sock.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len) as usize;
        if n == 0 || n > MAX_FRAME {
            return Err(Error::Corrupt("arcad reply length"));
        }
        let mut reply = vec![0u8; n];
        self.sock.read_exact(&mut reply)?;
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
