//! cryptad's Arca calls (vpn.md §4.3, as built in V1).
//!
//! arcad tells services apart by uid and serves no one as root, so the
//! calls run in a child process as `_crypta-key` (named `cryptad` in
//! arcad's `ARCA_SERVICES`), the way dispensatord's do. The child lives as
//! long as cryptad and serves one line at a time on its stdin and stdout:
//!
//! - `mint account=NAME` → `ok id= pub=`
//! - `import account=NAME key=HEX` → `ok id= pub=` (a `.conf`'s private key)
//! - `psk account=NAME value=HEX` → `ok id=`
//! - `release id=` → `ok value=` (a preshared key, for the engine's Start)
//! - `dh id= point=` → `ok shared=`
//! - `delete id=` → `ok`
//!
//! - `find account=` → `ok id=` (an item of this service by its account)
//! - `forget-all prefix=` → `ok` (every item whose account starts so)
//! - `mint-tls account=` → `ok id= pub=` (a P-256 client key, V4)
//! - `sign id= digest=` → `ok sig=` (with that key, over a SHA-256)
//!
//! Anything else, or an arcad refusal: `error status=N`.
//!
//! An AnyConnect engine gets a helper of its own (`--arca-vpn NAME`,
//! [`serve_vpn`]) on a socket only the engine holds: what the person
//! typed, a saved password, the session cookie and the client key's
//! signatures go from Arca to the engine through it, never through
//! cryptad (vpn.md §9.3).

use arca_store::codec::Writer;
use arcad::proto::{op, status};
use aura_ipc::Line;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const WG_KEY: u16 = 0x104;
const WG_PSK: u16 = 9;
const TLS_KEY: u16 = 0x105;
const PASSWORD: u16 = 1;
const COOKIE: u16 = 2;
const AFTER_BOOT: u8 = 1;
/// This service's item kinds an engine's helper may touch, by role.
const SSO_TOKEN: u16 = 3;

/// The helper's side: serve until stdin closes.
pub fn serve(socket: &str) {
    let stdin = io::stdin();
    let mut out = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(mut s) = line else { return };
        let req = Line::parse(&s);
        zeroize::Zeroize::zeroize(&mut s);
        let ans = match req {
            Some(l) => answer(socket, &l),
            None => Line::new("error").with("status", status::MALFORMED),
        };
        if ans.send(&mut out).is_err() {
            return;
        }
    }
}

fn call(socket: &str, opc: u16, body: &[u8]) -> io::Result<(u8, arca_store::secret::LockedBuf)> {
    let mut s = std::os::unix::net::UnixStream::connect(socket)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    arcad::proto::write_request(&mut s, opc, body)?;
    arcad::proto::read_reply(&mut s)
}

/// This service's items: `(id, kind, account)`.
fn items(socket: &str) -> Result<Vec<([u8; 16], u16, String)>, u8> {
    let (st, b) = call(socket, op::LIST, &[]).map_err(|_| 253u8)?;
    if st != status::OK {
        return Err(st);
    }
    let mut r = arca_store::codec::Reader::new(b.as_ref());
    let n = r.u32().map_err(|_| status::MALFORMED)?;
    let mut out = Vec::new();
    for _ in 0..n.min(4096) {
        let id = r.array::<16>().map_err(|_| status::MALFORMED)?;
        let kind = r.u16().map_err(|_| status::MALFORMED)?;
        let _class = r.u8().map_err(|_| status::MALFORMED)?;
        let _flags = r.u32().map_err(|_| status::MALFORMED)?;
        let account = r.str(256).map_err(|_| status::MALFORMED)?.to_string();
        out.push((id, kind, account));
    }
    Ok(out)
}

fn find(socket: &str, account: &str, kind: u16) -> Option<[u8; 16]> {
    items(socket).ok()?.into_iter().find(|(_, k, a)| a == account && *k == kind).map(|(i, _, _)| i)
}

fn delete(socket: &str, id: &[u8; 16]) -> bool {
    call(socket, op::DELETE, id).is_ok_and(|(st, _)| st == status::OK)
}

/// Store `value` as this service's `kind` item `account`, replacing one.
fn replace(socket: &str, account: &str, kind: u16, value: &[u8]) -> Result<(), u8> {
    let old = find(socket, account, kind);
    let mut w = Writer::new();
    w.u16(kind);
    w.u8(AFTER_BOOT);
    w.str(account);
    w.bytes(value);
    let r = call(socket, op::STORE, &w.buf);
    zeroize::Zeroize::zeroize(&mut w.buf);
    match r {
        Ok((st, _)) if st == status::OK => {
            if let Some(o) = old {
                delete(socket, &o);
            }
            Ok(())
        }
        Ok((st, _)) => Err(st),
        Err(_) => Err(253),
    }
}

fn release(socket: &str, id: &[u8; 16]) -> Result<zeroize::Zeroizing<Vec<u8>>, u8> {
    let (st, b) = call(socket, op::RELEASE, id).map_err(|_| 253u8)?;
    if st != status::OK || b.as_ref().len() < 4 {
        return Err(if st == status::OK { status::MALFORMED } else { st });
    }
    Ok(zeroize::Zeroizing::new(b.as_ref()[4..].to_vec()))
}

/// An AnyConnect engine's helper (vpn.md §9.3): serves one profile's
/// secrets on stdin/stdout, which is the engine's fd 4, until it closes.
///
/// - `take handle= keep=0|1` → `ok value=` (TAKE_VPN; with keep, held
///   until `commit` stores it as the saved password)
/// - `saved` → `ok value=`; `commit` → `ok`
/// - `cookie` → `ok value=`; `cookie-set value=`; `cookie-drop`
/// - `sso` → `ok value=` (the token Arca's courier caught, used once)
/// - `sign digest=` → `ok sig=` (the profile's client key)
pub fn serve_vpn(socket: &str, name: &str) {
    let stdin = io::stdin();
    let mut out = io::stdout();
    let mut pending: Option<zeroize::Zeroizing<Vec<u8>>> = None;
    let (pw, session, sso, tls) = (format!("{name}/password"), format!("{name}/session"), format!("{name}/sso"), format!("{name}/tls"));
    for line in stdin.lock().lines() {
        let Ok(mut s) = line else { return };
        let req = Line::parse(&s);
        zeroize::Zeroize::zeroize(&mut s);
        let err = |st: u8| Line::new("error").with("status", st);
        let value = |r: Result<zeroize::Zeroizing<Vec<u8>>, u8>| match r {
            Ok(v) => Line::new("ok").with_hex("value", &v),
            Err(st) => err(st),
        };
        let ans = match req.as_ref().map(|l| l.verb.as_str()) {
            Some("take") => {
                let l = req.as_ref().unwrap();
                match l.hex("handle").and_then(|h| <[u8; 16]>::try_from(h).ok()) {
                    None => err(status::MALFORMED),
                    Some(h) => match call(socket, op::TAKE_VPN, &h) {
                        Ok((st, b)) if st == status::OK && b.as_ref().len() >= 4 => {
                            let v = zeroize::Zeroizing::new(b.as_ref()[4..].to_vec());
                            if l.get("keep") == Some("1") {
                                pending = Some(v.clone());
                            }
                            value(Ok(v))
                        }
                        Ok((st, _)) => err(st),
                        Err(_) => err(253),
                    },
                }
            }
            Some("saved") => value(find(socket, &pw, PASSWORD).ok_or(status::DENIED).and_then(|i| release(socket, &i))),
            Some("commit") => match pending.take() {
                Some(v) => match replace(socket, &pw, PASSWORD, &v) {
                    Ok(()) => Line::new("ok"),
                    Err(st) => err(st),
                },
                None => Line::new("ok"),
            },
            Some("cookie") => value(find(socket, &session, COOKIE).ok_or(status::DENIED).and_then(|i| release(socket, &i))),
            Some("cookie-set") => match req.as_ref().unwrap().hex("value") {
                Some(v) => {
                    let v = zeroize::Zeroizing::new(v);
                    match replace(socket, &session, COOKIE, &v) {
                        Ok(()) => Line::new("ok"),
                        Err(st) => err(st),
                    }
                }
                None => err(status::MALFORMED),
            },
            Some("cookie-drop") => {
                if let Some(i) = find(socket, &session, COOKIE) {
                    delete(socket, &i);
                }
                Line::new("ok")
            }
            Some("sso") => {
                // Used once: a sign-on token is not kept past its sign-in.
                let r = find(socket, &sso, SSO_TOKEN).ok_or(status::DENIED).and_then(|i| {
                    let v = release(socket, &i);
                    delete(socket, &i);
                    v
                });
                value(r)
            }
            Some("sign") => match (req.as_ref().unwrap().hex("digest").filter(|d| d.len() == 32), find(socket, &tls, TLS_KEY)) {
                (Some(d), Some(id)) => {
                    let mut b = id.to_vec();
                    b.extend_from_slice(&d);
                    match call(socket, op::SIGN_DIGEST, &b) {
                        Ok((st, sig)) if st == status::OK => Line::new("ok").with_hex("sig", sig.as_ref()),
                        Ok((st, _)) => err(st),
                        Err(_) => err(253),
                    }
                }
                (None, _) => err(status::MALFORMED),
                (_, None) => err(status::DENIED),
            },
            _ => err(status::MALFORMED),
        };
        if ans.send(&mut out).is_err() || out.flush().is_err() {
            return;
        }
    }
}

fn answer(socket: &str, l: &Line) -> Line {
    let err = |st: u8| Line::new("error").with("status", st);
    match l.verb.as_str() {
        "find" => {
            let a = l.get("account").unwrap_or("");
            return match items(socket) {
                Ok(v) => match v.into_iter().find(|(_, _, acc)| acc == a) {
                    Some((id, _, _)) => Line::new("ok").with_hex("id", &id),
                    None => err(status::DENIED),
                },
                Err(st) => err(st),
            };
        }
        "forget-all" => {
            let pre = l.get("prefix").unwrap_or("");
            if pre.is_empty() {
                return err(status::MALFORMED);
            }
            return match items(socket) {
                Ok(v) => {
                    for (id, _, _) in v.into_iter().filter(|(_, _, a)| a.starts_with(pre)) {
                        delete(socket, &id);
                    }
                    Line::new("ok")
                }
                Err(st) => err(st),
            };
        }
        "sign" => {
            let (Some(i), Some(d)) = (l.hex("id").filter(|i| i.len() == 16), l.hex("digest").filter(|d| d.len() == 32)) else { return err(status::MALFORMED) };
            let mut b = i;
            b.extend_from_slice(&d);
            return match call(socket, op::SIGN_DIGEST, &b) {
                Ok((st, sig)) if st == status::OK => Line::new("ok").with_hex("sig", sig.as_ref()),
                Ok((st, _)) => err(st),
                Err(_) => err(253),
            };
        }
        _ => {}
    }
    let id = || l.hex("id").filter(|i| i.len() == 16);
    let mut w = Writer::new();
    let opc = match l.verb.as_str() {
        "mint-tls" => {
            w.u16(TLS_KEY);
            w.u8(AFTER_BOOT);
            w.str(l.get("account").unwrap_or(""));
            op::MINT
        }
        "mint" => {
            w.u16(WG_KEY);
            w.u8(AFTER_BOOT);
            w.str(l.get("account").unwrap_or(""));
            op::MINT
        }
        "import" | "psk" => {
            let Some(v) = l.hex(if l.verb == "import" { "key" } else { "value" }).filter(|v| v.len() == 32) else { return err(status::MALFORMED) };
            w.u16(if l.verb == "import" { WG_KEY } else { WG_PSK });
            w.u8(AFTER_BOOT);
            w.str(l.get("account").unwrap_or(""));
            w.bytes(&v);
            let mut v = v;
            zeroize::Zeroize::zeroize(&mut v);
            op::STORE
        }
        "release" | "delete" | "pubkey" => {
            let Some(i) = id() else { return err(status::MALFORMED) };
            w.raw(&i);
            match l.verb.as_str() {
                "release" => op::RELEASE,
                "delete" => op::DELETE,
                _ => op::PUBKEY,
            }
        }
        "dh" => {
            let (Some(i), Some(p)) = (id(), l.hex("point").filter(|p| p.len() == 32)) else { return err(status::MALFORMED) };
            w.raw(&i);
            w.raw(&p);
            op::DH_X25519
        }
        _ => return err(status::MALFORMED),
    };
    let r = call(socket, opc, &w.buf);
    zeroize::Zeroize::zeroize(&mut w.buf);
    let (st, b) = match r {
        Ok(x) => x,
        Err(_) => return err(253),
    };
    if st != status::OK {
        return err(st);
    }
    let b = b.as_ref();
    match l.verb.as_str() {
        "mint" | "import" if b.len() == 48 => Line::new("ok").with_hex("id", &b[..16]).with_hex("pub", &b[16..]),
        "mint-tls" if b.len() == 16 + 65 => Line::new("ok").with_hex("id", &b[..16]).with_hex("pub", &b[16..]),
        "psk" if b.len() == 16 => Line::new("ok").with_hex("id", b),
        "release" if b.len() >= 4 => Line::new("ok").with_hex("value", &b[4..]),
        "dh" if b.len() == 32 => Line::new("ok").with_hex("shared", b),
        "pubkey" if b.len() == 32 || b.len() == 65 => Line::new("ok").with_hex("pub", b),
        "delete" => Line::new("ok"),
        _ => err(status::MALFORMED),
    }
}

/// cryptad's side: the child, and a call at a time.
pub struct Keys {
    child: Child,
    to: ChildStdin,
    from: BufReader<ChildStdout>,
}

impl Keys {
    pub fn start(uid: u32, socket: &str) -> io::Result<Keys> {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("--arca-key").env_clear().env("ARCA_SOCKET", socket).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        // SAFETY: getuid has no preconditions.
        if unsafe { libc::getuid() } == 0 {
            // std drops the supplementary groups (setgroups(0)) before
            // setuid when it runs as root; pre_exec would run after setuid.
            cmd.uid(uid).gid(uid);
        }
        let mut child = cmd.spawn()?;
        let to = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
        let from = BufReader::new(child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?);
        Ok(Keys { child, to, from })
    }

    /// An AnyConnect engine's own helper (vpn.md §9.3), serving `name`'s
    /// secrets on `sock` (the engine's fd 4), as `uid`.
    pub fn start_vpn(uid: u32, socket: &str, name: &str, sock: std::os::unix::net::UnixStream) -> io::Result<Child> {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe()?;
        let fd: std::os::fd::OwnedFd = sock.into();
        let fd2 = fd.try_clone()?;
        let mut cmd = Command::new(exe);
        cmd.arg("--arca-vpn").arg(name).env_clear().env("ARCA_SOCKET", socket).stdin(Stdio::from(fd)).stdout(Stdio::from(fd2)).stderr(Stdio::inherit());
        // SAFETY: getuid has no preconditions.
        if unsafe { libc::getuid() } == 0 {
            cmd.uid(uid).gid(uid);
        }
        cmd.spawn()
    }

    pub fn call(&mut self, l: &Line) -> io::Result<Line> {
        l.send(&mut self.to)?;
        self.to.flush()?;
        let mut s = String::new();
        if self.from.read_line(&mut s)? == 0 {
            return Err(io::Error::other("the key helper exited"));
        }
        let r = Line::parse(s.trim_end());
        zeroize::Zeroize::zeroize(&mut s);
        r.ok_or_else(|| io::Error::other("the key helper said nonsense"))
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
