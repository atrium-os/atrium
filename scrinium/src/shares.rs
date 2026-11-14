//! The file manager's side of Arca for network shares (network-shares.md
//! §5.2; arcad's `share_op`). Scrinium is arcad's configured file manager
//! (`ARCA_FILE_MANAGER`): it may turn what the user typed in its own
//! credential field (a handle; Scrinium never sees the value) into the
//! password item of one share, list the saved shares, and forget one.
//!
//! arcad's wire, kept here in a few lines: a request is `u32 LE length |
//! u16 LE op | body`, a reply `u32 LE length | u8 status | body`; strings
//! are u32-LE-length prefixed. A share is `u8 proto (1 sftp, 2 webdav) | str host |
//! u16 port | str path | key[32]`.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::connect::Address;

pub const ARCA_SOCKET: &str = "/atrium/sockets/arca/arcad.sock";

const SH_SAVE: u16 = 0xA0;
const SH_LIST: u16 = 0xA2;
const SH_DELETE: u16 = 0xA3;
const KIND_PASSWORD: u16 = 1;

pub fn arca_socket() -> PathBuf {
    std::env::var_os("SCRINIUM_ARCA_SOCKET").map(PathBuf::from).unwrap_or_else(|| ARCA_SOCKET.into())
}

/// A saved share: where, the pinned key, and the item that logs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub addr: Address,
    pub key: [u8; 32],
    pub item: [u8; 16],
}

fn put_str(b: &mut Vec<u8>, s: &[u8]) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s);
}

fn put_share(b: &mut Vec<u8>, a: &Address, key: &[u8; 32]) {
    b.push(a.proto.byte());
    put_str(b, a.host.as_bytes());
    b.extend_from_slice(&a.port.to_le_bytes());
    put_str(b, a.path.as_bytes());
    b.extend_from_slice(key);
}

fn call(sock: &std::path::Path, op: u16, body: &[u8]) -> io::Result<(u8, Vec<u8>)> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut f = Vec::with_capacity(6 + body.len());
    f.extend_from_slice(&((2 + body.len()) as u32).to_le_bytes());
    f.extend_from_slice(&op.to_le_bytes());
    f.extend_from_slice(body);
    s.write_all(&f)?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if !(1..=64 * 1024).contains(&n) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "reply length"));
    }
    let mut r = vec![0u8; n];
    s.read_exact(&mut r)?;
    let st = r.remove(0);
    Ok((st, r))
}

fn why(st: u8) -> String {
    match st {
        1 => "Arca refused (the password field's entry has expired; type it again)".into(),
        2 => "Arca is locked".into(),
        5 => "Arca doesn't know Scrinium as the file manager".into(),
        s => format!("Arca refused (status {s})"),
    }
}

/// Save the field's `handle` as `addr`'s password, pinned to `key`.
pub fn save(sock: &std::path::Path, handle: &[u8; 16], addr: &Address, key: &[u8; 32]) -> Result<[u8; 16], String> {
    let mut b = Vec::new();
    b.extend_from_slice(handle);
    put_share(&mut b, addr, key);
    put_str(&mut b, addr.user.as_bytes());
    match call(sock, SH_SAVE, &b) {
        Ok((0, r)) => r.try_into().map_err(|_| "Arca's answer was odd".to_string()),
        Ok((st, _)) => Err(why(st)),
        Err(e) => Err(format!("Arca: {e}")),
    }
}

pub fn forget(sock: &std::path::Path, s: &Saved) -> Result<(), String> {
    let mut b = Vec::new();
    put_share(&mut b, &s.addr, &s.key);
    b.extend_from_slice(&s.item);
    match call(sock, SH_DELETE, &b) {
        Ok((0, _)) => Ok(()),
        Ok((st, _)) => Err(why(st)),
        Err(e) => Err(format!("Arca: {e}")),
    }
}

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        (n <= self.0.len()).then(|| {
            let (a, b) = self.0.split_at(n);
            self.0 = b;
            a
        })
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// `SH_LIST`'s reply: the saved shares' password items.
pub fn parse_list(b: &[u8]) -> Option<Vec<Saved>> {
    let mut c = Cur(b);
    let n = c.u32()?;
    let mut out = Vec::new();
    for _ in 0..n.min(4096) {
        let proto = crate::connect::Proto::from_byte(c.take(1)?[0])?;
        let host = c.str()?;
        let port = c.u16()?;
        let path = c.str()?;
        let key: [u8; 32] = c.take(32)?.try_into().ok()?;
        let item: [u8; 16] = c.take(16)?.try_into().ok()?;
        let kind = c.u16()?;
        let user = c.str()?;
        if kind == KIND_PASSWORD {
            out.push(Saved { addr: Address { proto, user, host, port, path }, key, item });
        }
    }
    Some(out)
}

pub fn list(sock: &std::path::Path) -> Result<Vec<Saved>, String> {
    match call(sock, SH_LIST, &[]) {
        Ok((0, r)) => parse_list(&r).ok_or_else(|| "Arca's answer was odd".into()),
        Ok((st, _)) => Err(why(st)),
        Err(e) => Err(format!("Arca: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_reply_parses() {
        let a = Address { proto: crate::connect::Proto::Webdav, user: "alice".into(), host: "nas.lan".into(), port: 443, path: "/dav".into() };
        let mut b = 2u32.to_le_bytes().to_vec();
        for (kind, item) in [(1u16, [7u8; 16]), (0x100, [8u8; 16])] {
            put_share(&mut b, &a, &[5; 32]);
            b.extend_from_slice(&item);
            b.extend_from_slice(&kind.to_le_bytes());
            put_str(&mut b, b"alice");
        }
        let l = parse_list(&b).unwrap();
        assert_eq!(l, vec![Saved { addr: a, key: [5; 32], item: [7; 16] }], "keys aren't passwords");
        assert!(parse_list(&b[..b.len() - 1]).is_none());
    }
}
