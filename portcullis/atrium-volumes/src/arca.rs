//! Persona folders' keys from arcad (tessera-encryption.md §4.3, §5.4,
//! E4.6). arcad tells services apart by uid, and atrium-volumes is root,
//! which arcad serves no one as; so each call runs in a child process as
//! [`VK_UID`] (`_volumes-vk`), named `atrium-volumes` in arcad's
//! `ARCA_SERVICES`. The child (`atrium-volumes arca-vk ...`) does one call
//! and prints one line. The key itself never leaves arcad: it goes from
//! arcad into the kernel's keyring.
//!
//! arcad's wire (arca/arcad/src/proto.rs): a request frame is
//! `u32 len | u16 op | body`, a reply `u32 len | u8 status | body`; a
//! string is `u32 len | bytes`.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The uid atrium-volumes' arcad calls run as.
pub const VK_UID: u32 = 65053;
pub const SOCKET: &str = "/atrium/sockets/arca/arcad.sock";

pub const VK_MINT: u16 = 0xB0;
pub const VK_LOAD: u16 = 0xB1;
pub const VK_RETIRE: u16 = 0xB9;
pub const PERSONA_DESTROY: u16 = 0xBA;

pub const OK: u8 = 0;
pub const DENIED: u8 = 1;
pub const LOCKED: u8 = 2;
pub const UNKNOWN_PEER: u8 = 5;
pub const UNSUPPORTED: u8 = 12;
pub const NO_STORE: u8 = 13;

/// Why a key call didn't work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// The person's store is locked (or not open yet).
    Locked,
    /// arcad refuses: not this person's key, or not for this scope.
    Denied,
    /// No keys here: arcad isn't running, has no keyring, or doesn't know
    /// atrium-volumes. Folders that were never encrypted stay as they are.
    Unavailable(String),
    Other(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Locked => write!(f, "the person's Arca is locked"),
            KeyError::Denied => write!(f, "Arca refused the key"),
            KeyError::Unavailable(s) => write!(f, "no folder keys here ({s})"),
            KeyError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// What atrium-volumes asks arcad. `user`/`persona` name the owner.
pub trait Keys {
    /// A new group key, stored in the person's persona and loaded.
    fn mint(&self, user: &str, persona: &str, uuid: &[u8; 16], group: u64, label: &str) -> Result<[u8; 16], KeyError>;
    /// Load the key (OK if it is loaded already).
    fn load(&self, user: &str, persona: &str, key_id: &[u8; 16], uuid: &[u8; 16], group: u64) -> Result<(), KeyError>;
    /// A removed persona's key: deleted, and dropped from the kernel.
    fn retire(&self, user: &str, persona: &str, key_id: &[u8; 16]) -> Result<(), KeyError>;
    /// A removed persona's whole Arca partition (E5): its sticks' keys
    /// dropped and every item deleted. The number of items.
    fn destroy(&self, user: &str, persona: &str) -> Result<u32, KeyError>;
}

#[derive(Default)]
pub struct Body(pub Vec<u8>);
impl Body {
    pub fn str(mut self, s: &str) -> Self {
        self.0.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    pub fn raw(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
}

pub fn call(s: &mut (impl Read + Write), op: u16, body: &[u8]) -> io::Result<(u8, Vec<u8>)> {
    s.write_all(&((2 + body.len()) as u32).to_le_bytes())?;
    s.write_all(&op.to_le_bytes())?;
    s.write_all(body)?;
    s.flush()?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if !(1..=1 << 20).contains(&n) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    let st = b.remove(0);
    Ok((st, b))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut o = [0u8; 16];
    for (i, b) in o.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(o)
}

/// The child's request from its arguments: `mint USER PERSONA UUID GROUP
/// LABEL`, `load USER PERSONA KEYID UUID GROUP`, `retire USER PERSONA KEYID`,
/// `destroy USER PERSONA`.
pub fn request(args: &[String]) -> Option<(u16, Vec<u8>)> {
    let a = |i: usize| args.get(i).map(String::as_str);
    let g = |i: usize| a(i)?.parse::<u64>().ok();
    match (a(0)?, args.len()) {
        ("mint", 6) => Some((VK_MINT, Body::default().str(a(1)?).str(a(2)?).raw(&unhex16(a(3)?)?).u64(g(4)?).str(a(5)?).0)),
        ("load", 6) => Some((VK_LOAD, Body::default().str(a(1)?).str(a(2)?).raw(&unhex16(a(3)?)?).raw(&unhex16(a(4)?)?).u64(g(5)?).0)),
        ("retire", 4) => Some((VK_RETIRE, Body::default().str(a(1)?).str(a(2)?).raw(&unhex16(a(3)?)?).0)),
        ("destroy", 3) => Some((PERSONA_DESTROY, Body::default().str(a(1)?).str(a(2)?).0)),
        _ => None,
    }
}

/// The child's one line: `ok [KEYID]` or `status N`.
pub fn answer(op: u16, st: u8, body: &[u8]) -> String {
    match (st, op) {
        (OK, VK_MINT) if body.len() == 16 => format!("ok {}", hex(body)),
        (OK, VK_LOAD | VK_RETIRE) if body.is_empty() => "ok".into(),
        (OK, PERSONA_DESTROY) if body.len() == 4 => format!("ok {}", u32::from_le_bytes(body.try_into().unwrap())),
        (OK, _) => "status 255".into(),
        (st, _) => format!("status {st}"),
    }
}

/// The child: one call, one line on stdout. Exit 0 whatever arcad said;
/// 2 when arcad couldn't be reached.
pub fn child_main(args: &[String]) -> i32 {
    let Some((op, body)) = request(args) else {
        eprintln!("atrium-volumes arca-vk: bad request");
        return 64;
    };
    let path = std::env::var_os("ATRIUM_VOLUMES_ARCA_SOCKET").map(PathBuf::from).unwrap_or_else(|| SOCKET.into());
    let mut s = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            println!("unreachable {e}");
            return 2;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    match call(&mut s, op, &body) {
        Ok((st, b)) => {
            println!("{}", answer(op, st, &b));
            0
        }
        Err(e) => {
            println!("unreachable {e}");
            2
        }
    }
}

fn status_error(line: &str) -> KeyError {
    match line.strip_prefix("status ").and_then(|n| n.trim().parse::<u8>().ok()) {
        Some(LOCKED) | Some(NO_STORE) => KeyError::Locked,
        Some(DENIED) => KeyError::Denied,
        Some(UNKNOWN_PEER) => KeyError::Unavailable("arcad doesn't know atrium-volumes".into()),
        Some(UNSUPPORTED) => KeyError::Unavailable("arcad has no keyring".into()),
        Some(n) => KeyError::Other(format!("arcad answered {n}")),
        None => match line.strip_prefix("unreachable ") {
            Some(why) => KeyError::Unavailable(format!("arcad: {why}")),
            None => KeyError::Other(format!("arca-vk: {line:?}")),
        },
    }
}

/// The real thing: each call is `atrium-volumes arca-vk ...` as [`VK_UID`].
pub struct Arcad {
    pub exe: PathBuf,
    pub uid: u32,
}

impl Arcad {
    pub fn new() -> io::Result<Self> {
        Ok(Arcad { exe: std::env::current_exe()?, uid: VK_UID })
    }

    fn run(&self, args: &[&str]) -> Result<String, KeyError> {
        let mut c = Command::new(&self.exe);
        c.arg("arca-vk").args(args).env_clear().stdin(Stdio::null()).stderr(Stdio::inherit());
        if let Some(s) = std::env::var_os("ATRIUM_VOLUMES_ARCA_SOCKET") {
            c.env("ATRIUM_VOLUMES_ARCA_SOCKET", s);
        }
        c.uid(self.uid).gid(self.uid);
        let out = c.output().map_err(|e| KeyError::Other(format!("arca-vk: {e}")))?;
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if line == "ok" || line.starts_with("ok ") {
            Ok(line.trim_start_matches("ok").trim().to_string())
        } else {
            Err(status_error(&line))
        }
    }
}

impl Keys for Arcad {
    fn mint(&self, user: &str, persona: &str, uuid: &[u8; 16], group: u64, label: &str) -> Result<[u8; 16], KeyError> {
        let a = self.run(&["mint", user, persona, &hex(uuid), &group.to_string(), label])?;
        unhex16(&a).ok_or_else(|| KeyError::Other(format!("mint answered {a:?}")))
    }
    fn load(&self, user: &str, persona: &str, key_id: &[u8; 16], uuid: &[u8; 16], group: u64) -> Result<(), KeyError> {
        self.run(&["load", user, persona, &hex(key_id), &hex(uuid), &group.to_string()]).map(|_| ())
    }
    fn retire(&self, user: &str, persona: &str, key_id: &[u8; 16]) -> Result<(), KeyError> {
        self.run(&["retire", user, persona, &hex(key_id)]).map(|_| ())
    }
    fn destroy(&self, user: &str, persona: &str) -> Result<u32, KeyError> {
        let a = self.run(&["destroy", user, persona])?;
        a.parse().map_err(|_| KeyError::Other(format!("destroy answered {a:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

    #[test]
    fn requests_match_arcads_bodies() {
        let k = "00112233445566778899aabbccddeeff";
        let (op, b) = request(&s(&["load", "alice", "work", k, k, "7"])).unwrap();
        assert_eq!(op, VK_LOAD);
        assert_eq!(b.len(), 4 + 5 + 4 + 4 + 16 + 16 + 8);
        assert_eq!(&b[b.len() - 8..], &7u64.to_le_bytes());
        let (op, b) = request(&s(&["mint", "alice", "personal", k, "9", "Personal folders"])).unwrap();
        assert_eq!(op, VK_MINT);
        assert!(b.ends_with(b"Personal folders"));
        assert_eq!(request(&s(&["retire", "alice", "work", k])).unwrap().0, VK_RETIRE);
        let (op, b) = request(&s(&["destroy", "alice", "work"])).unwrap();
        assert_eq!((op, b.len()), (PERSONA_DESTROY, 4 + 5 + 4 + 4));
        assert!(request(&s(&["load", "alice", "work", "zz", k, "7"])).is_none());
        assert!(request(&s(&["mint", "alice", "work", k, "x", "l"])).is_none());
    }

    #[test]
    fn answers_and_errors() {
        assert_eq!(answer(VK_MINT, OK, &[0xab; 16]), format!("ok {}", "ab".repeat(16)));
        assert_eq!(answer(VK_LOAD, OK, &[]), "ok");
        assert_eq!(answer(PERSONA_DESTROY, OK, &3u32.to_le_bytes()), "ok 3");
        assert_eq!(answer(VK_LOAD, LOCKED, &[]), "status 2");
        assert_eq!(status_error("status 2"), KeyError::Locked);
        assert_eq!(status_error("status 1"), KeyError::Denied);
        assert!(matches!(status_error("status 5"), KeyError::Unavailable(_)));
        assert!(matches!(status_error("status 12"), KeyError::Unavailable(_)));
        assert!(matches!(status_error("unreachable No such file"), KeyError::Unavailable(_)));
    }
}
