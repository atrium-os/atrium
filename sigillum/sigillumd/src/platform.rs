//! Arca as the platform authenticator (sigillum.md §9.2): the client side
//! of arcad's `P_*` ops (arca.md §8.4). arcad knows sigillumd by its uid
//! (`ARCA_SERVICES` names `sigillumd`) and serves it nothing else.
//!
//! The wire is arcad's own: `u32 LE length | u16 LE op | body` out,
//! `u32 LE length | u8 status | body` back, fields as `u32 LE length |
//! bytes`. Every answer is bounded here before the broker sees it, and the
//! broker checks the authenticator data again.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::broker::{Passkey, PlatformResult};

pub const P_CREATE: u16 = 0x90;
pub const P_ASSERT: u16 = 0x91;
pub const P_LIST: u16 = 0x92;
pub const DEFAULT_SOCKET: &str = "/atrium/sockets/arca/arcad.sock";
const MAX_REPLY: usize = 64 * 1024;
const MAX_LIST: u32 = 64;

/// What the daemon asks of a platform authenticator. Calls block; arcad
/// is local and answers in milliseconds.
pub trait Platform: Send {
    /// `None`: Arca isn't available for this user now (locked, refused,
    /// unreachable).
    fn list(&mut self, user: &str, rp_id: &str) -> Option<Vec<Passkey>>;
    #[allow(clippy::too_many_arguments)]
    fn create(&mut self, user: &str, rp_id: &str, user_handle: &[u8], name: &str, display_name: &str, uv: bool) -> PlatformResult;
    fn assert(&mut self, user: &str, rp_id: &str, id: &[u8; 16], uv: bool, client_data_hash: &[u8; 32]) -> PlatformResult;
}

/// No platform authenticator: the sheet offers security keys only.
pub struct NoPlatform;

impl Platform for NoPlatform {
    fn list(&mut self, _: &str, _: &str) -> Option<Vec<Passkey>> {
        None
    }
    fn create(&mut self, _: &str, _: &str, _: &[u8], _: &str, _: &str, _: bool) -> PlatformResult {
        PlatformResult::Failed
    }
    fn assert(&mut self, _: &str, _: &str, _: &[u8; 16], _: bool, _: &[u8; 32]) -> PlatformResult {
        PlatformResult::Failed
    }
}

/// One request to arcad: `(status, body)`, or `None` on any failure.
pub trait Wire: Send {
    fn call(&mut self, op: u16, body: &[u8]) -> Option<(u8, Vec<u8>)>;
}

/// arcad's socket, one connection per call.
pub struct Socket(pub PathBuf);

impl Socket {
    fn call_io(&self, op: u16, body: &[u8]) -> io::Result<(u8, Vec<u8>)> {
        let mut s = UnixStream::connect(&self.0)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut frame = Vec::with_capacity(6 + body.len());
        frame.extend_from_slice(&((2 + body.len()) as u32).to_le_bytes());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(body);
        s.write_all(&frame)?;
        let mut len = [0u8; 4];
        s.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len) as usize;
        if !(1..=MAX_REPLY).contains(&n) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "reply length"));
        }
        let mut b = vec![0u8; n];
        s.read_exact(&mut b)?;
        let st = b.remove(0);
        Ok((st, b))
    }
}

impl Wire for Socket {
    fn call(&mut self, op: u16, body: &[u8]) -> Option<(u8, Vec<u8>)> {
        match self.call_io(op, body) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("sigillumd: arcad: {e}");
                None
            }
        }
    }
}

struct W(Vec<u8>);

impl W {
    fn bytes(&mut self, b: &[u8]) -> &mut W {
        self.0.extend_from_slice(&(b.len() as u32).to_le_bytes());
        self.0.extend_from_slice(b);
        self
    }
    fn str(&mut self, s: &str) -> &mut W {
        self.bytes(s.as_bytes())
    }
    fn raw(&mut self, b: &[u8]) -> &mut W {
        self.0.extend_from_slice(b);
        self
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn raw(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Some(h)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.raw(4)?.try_into().ok()?))
    }
    fn bytes(&mut self, cap: usize) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        if n > cap {
            return None;
        }
        self.raw(n)
    }
    fn str(&mut self, cap: usize) -> Option<String> {
        String::from_utf8(self.bytes(cap)?.to_vec()).ok()
    }
    fn id(&mut self) -> Option<[u8; 16]> {
        self.raw(16)?.try_into().ok()
    }
    fn end(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

/// Arca over a [`Wire`].
pub struct Arca<T: Wire>(pub T);

const OK: u8 = 0;

impl<T: Wire> Arca<T> {
    fn ok(&mut self, op: u16, body: &[u8]) -> Option<Vec<u8>> {
        match self.0.call(op, body)? {
            (OK, b) => Some(b),
            (st, _) => {
                eprintln!("sigillumd: arcad op {op:#06x}: status {st}");
                None
            }
        }
    }
}

impl<T: Wire> Platform for Arca<T> {
    fn list(&mut self, user: &str, rp_id: &str) -> Option<Vec<Passkey>> {
        let b = self.ok(P_LIST, &W(Vec::new()).str(user).str(rp_id).0)?;
        let mut r = R(&b);
        let n = r.u32()?;
        if n > MAX_LIST {
            return None;
        }
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(Passkey { id: r.id()?, user_handle: r.bytes(64)?.to_vec(), name: r.str(64)?, display_name: r.str(64)? });
        }
        r.end()?;
        Some(out)
    }

    fn create(&mut self, user: &str, rp_id: &str, user_handle: &[u8], name: &str, display_name: &str, uv: bool) -> PlatformResult {
        let body = W(Vec::new()).str(user).str(rp_id).bytes(user_handle).str(name).str(display_name).raw(&[uv as u8]).0.clone();
        (|| {
            let b = self.ok(P_CREATE, &body)?;
            let mut r = R(&b);
            let id = r.id()?;
            let auth_data = r.bytes(1024)?.to_vec();
            r.end()?;
            Some(PlatformResult::Created { id, auth_data })
        })()
        .unwrap_or(PlatformResult::Failed)
    }

    fn assert(&mut self, user: &str, rp_id: &str, id: &[u8; 16], uv: bool, client_data_hash: &[u8; 32]) -> PlatformResult {
        let body = W(Vec::new()).str(user).str(rp_id).raw(id).raw(&[uv as u8]).raw(client_data_hash).0.clone();
        (|| {
            let b = self.ok(P_ASSERT, &body)?;
            let mut r = R(&b);
            let auth_data = r.bytes(1024)?.to_vec();
            let signature = r.bytes(80)?.to_vec();
            let user_handle = r.bytes(64)?.to_vec();
            r.end()?;
            Some(PlatformResult::Asserted { auth_data, signature, user_handle })
        })()
        .unwrap_or(PlatformResult::Failed)
    }
}
