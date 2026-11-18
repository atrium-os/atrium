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
//! Anything else, or an arcad refusal: `error status=N`.

use arca_store::codec::Writer;
use arcad::proto::{op, status};
use aura_ipc::Line;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const WG_KEY: u16 = 0x104;
const WG_PSK: u16 = 9;
const AFTER_BOOT: u8 = 1;

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

fn answer(socket: &str, l: &Line) -> Line {
    let err = |st: u8| Line::new("error").with("status", st);
    let id = || l.hex("id").filter(|i| i.len() == 16);
    let mut w = Writer::new();
    let opc = match l.verb.as_str() {
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
        "psk" if b.len() == 16 => Line::new("ok").with_hex("id", b),
        "release" if b.len() >= 4 => Line::new("ok").with_hex("value", &b[4..]),
        "dh" if b.len() == 32 => Line::new("ok").with_hex("shared", b),
        "pubkey" if b.len() == 32 => Line::new("ok").with_hex("pub", b),
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
