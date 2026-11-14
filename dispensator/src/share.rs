//! Network shares as a volume source (docs/spec/network-shares.md §6.1).
//!
//! The file manager asks to connect a share it has saved in Arca (the
//! password or key is Arca's, never ours). dispensatord resolves the host
//! once, takes a share-server uid, records that uid's label in Arca's share
//! registry, and has jaild start the protocol's server (`atrium-share-sftp`,
//! `-webdav`, `-smb`) in its own jail: FUSE
//! from jaild, a routed vnet whose only way out is that one address and
//! port, and the `share-server` system call profile. dispensatord never
//! parses a byte from the network and never holds a credential.
//!
//! Everything here is pure; the daemon carries it out.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

/// Arca's share-server uid range (arcad::share, jaild `[share_servers]`).
pub const UID_FIRST: u32 = 63_000;
pub const UID_LAST: u32 = 63_499;
/// Arca's share registry (arcad::share::DEFAULT_REGISTRY).
pub const REGISTRY: &str = "/var/run/atrium/share-registry";
pub const NAME_PREFIX: &str = "system-share-";
pub const EXEC: &str = "/usr/local/bin/atrium-share-sftp";
pub const EXEC_WEBDAV: &str = "/usr/local/bin/atrium-share-webdav";
pub const EXEC_SMB: &str = "/usr/local/bin/atrium-share-smb";
/// The trust store a WebDAV share server reads (nullfs, read-only).
pub const TRUST_DIR: &str = "/etc/ssl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Sftp,
    /// WebDAV over HTTPS (S2).
    Webdav,
    /// SMB 3.1.1, encrypted, password (S3).
    Smb,
}

impl Proto {
    pub fn name(self) -> &'static str {
        match self {
            Proto::Sftp => "sftp",
            Proto::Webdav => "webdav",
            Proto::Smb => "smb",
        }
    }

    pub fn exec(self) -> &'static str {
        match self {
            Proto::Sftp => EXEC,
            Proto::Webdav => EXEC_WEBDAV,
            Proto::Smb => EXEC_SMB,
        }
    }

    /// What the share jail sees of the host, read-only, besides Arca's
    /// socket.
    pub fn ro_mounts(self) -> &'static [&'static str] {
        match self {
            Proto::Sftp | Proto::Smb => &["/lib", "/libexec", "/usr/lib", "/usr/local/bin"],
            Proto::Webdav => &["/lib", "/libexec", "/usr/lib", "/usr/local/bin", TRUST_DIR],
        }
    }
}

/// Every directory any share jail mounts, deepest last (for unmounting).
pub const ALL_RO_MOUNTS: &[&str] = &["/lib", "/libexec", "/usr/lib", "/usr/local/bin", TRUST_DIR];

/// What a WebDAV share's identity is, as the file manager sends it: the
/// trust store vouches for the name.
pub const TRUSTED: &str = "trusted";
pub const JAIL_ROOT: &str = "/var/lib/atrium/jails";
/// Where Arca's socket is inside the jail (a nullfs of the host's).
pub const ARCA_SOCKET: &str = "/atrium/sockets/arca/arcad.sock";

/// What the file manager asks for (`atrium_media::Connect`), checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub proto: Proto,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub user: String,
    /// `SHA256:` and 43 base64 characters, as `--learn` printed it; for
    /// WebDAV, the pinned key's, or [`TRUSTED`].
    pub fingerprint: String,
    /// The same digest as bytes: what Arca pins (zeros for a trusted
    /// WebDAV share).
    pub key: [u8; 32],
}

/// A host as the user typed it: a lowercase DNS name or an IPv4 literal
/// (the routed jail's grants are IPv4 for now).
pub fn valid_host(h: &str) -> bool {
    if h.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn valid_user(u: &str) -> bool {
    !u.is_empty() && u.len() <= 64 && !u.starts_with('-') && u.bytes().all(|b| b.is_ascii_graphic() && b != b':')
}

fn valid_path(p: &str) -> bool {
    !p.is_empty() && p.len() <= 1024 && !p.starts_with('-') && !p.chars().any(|c| c.is_control())
}

/// The digest in an OpenSSH-style fingerprint.
pub fn key_of(fp: &str) -> Option<[u8; 32]> {
    let b = fp.strip_prefix("SHA256:")?;
    if b.len() != 43 {
        return None;
    }
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(33);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in b.bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // 43 characters = 258 bits: 32 bytes and two zero bits.
    if acc != 0 {
        return None;
    }
    out.try_into().ok()
}

pub fn check(proto: Proto, host: &str, port: u16, path: &str, user: &str, fingerprint: &str) -> Result<Share, String> {
    if !valid_host(host) {
        return Err("not a host name or IPv4 address".into());
    }
    if port == 0 {
        return Err("port 0".into());
    }
    if !valid_path(path) {
        return Err("not a folder path".into());
    }
    if !valid_user(user) {
        return Err("not a user name".into());
    }
    if proto == Proto::Webdav && !path.starts_with('/') {
        return Err("a WebDAV folder starts with /".into());
    }
    if proto == Proto::Smb && (!path.starts_with('/') || path.trim_matches('/').is_empty()) {
        return Err("an SMB path is /share or /share/folder".into());
    }
    let key = if proto == Proto::Webdav && fingerprint == TRUSTED { Some([0; 32]) } else { key_of(fingerprint) };
    let key = key.ok_or("not a SHA256 fingerprint")?;
    Ok(Share { proto, host: host.into(), port, path: path.into(), user: user.into(), fingerprint: fingerprint.into(), key })
}

/// May a share server dial `a`? LAN addresses are fine (the user typed this
/// server, §4.3); the link-local block (cloud metadata), loopback (the
/// jail's own), and anything not unicast are not.
pub fn dialable(a: Ipv4Addr) -> bool {
    !(a.is_loopback() || a.is_link_local() || a.is_unspecified() || a.is_broadcast() || a.is_multicast() || a.octets()[0] == 0)
}

/// The lowest free uid, or `None` when every share uid is taken.
pub fn free_uid(used: &BTreeSet<u32>) -> Option<u32> {
    (UID_FIRST..=UID_LAST).find(|u| !used.contains(u))
}

pub fn part(uid: u32) -> String {
    format!("share-{uid}")
}

pub fn uid_of_part(part: &str) -> Option<u32> {
    let u: u32 = part.strip_prefix("share-")?.parse().ok()?;
    (part == format!("share-{u}") && (UID_FIRST..=UID_LAST).contains(&u)).then_some(u)
}

pub fn jail_name(uid: u32) -> String {
    format!("{NAME_PREFIX}{uid}")
}

pub fn mountpoint(uid: u32) -> String {
    format!("/media/share-{uid}")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Arca's registry line for `uid` (arcad::share::parse_line's format):
/// `uid user proto host port key-hex path-hex`. The host is the name the
/// share was saved under, so it matches the Arca item's recipient.
pub fn registry_line(uid: u32, session_user: &str, s: &Share) -> String {
    format!("{uid} {session_user} {} {} {} {} {}", s.proto.name(), s.host, s.port, hex(&s.key), hex(s.path.as_bytes()))
}

pub fn retired_line(uid: u32) -> String {
    format!("{uid} retired")
}

/// The share server's argv: it dials the resolved address (no DNS in the
/// jail, so the name can't later point elsewhere), checks the pin, asks
/// Arca, and serves the FUSE descriptor jaild gave it as fd 3 to the
/// session's user.
pub fn argv(s: &Share, addr: Ipv4Addr, session_uid: u32, session_gid: u32, read_only: bool) -> Vec<String> {
    let mut v: Vec<String> = vec![s.proto.exec().into()];
    match s.proto {
        Proto::Sftp => v.extend(["--host".into(), addr.to_string()]),
        // The name is what the certificate is checked against; the address
        // is the one it dials.
        Proto::Webdav | Proto::Smb => v.extend(["--host".into(), s.host.clone(), "--addr".into(), addr.to_string()]),
    }
    let identity = match s.proto {
        Proto::Webdav if s.fingerprint == TRUSTED => vec!["--trusted".to_string()],
        _ => vec!["--pin".into(), s.fingerprint.clone()],
    };
    v.extend(identity);
    v.extend::<[String; 6]>([
        "--port".into(), s.port.to_string(),
        "--user".into(), s.user.clone(),
        "--root".into(), s.path.clone(),
    ]);
    v.extend::<Vec<String>>(vec![
        "--fuse-fd".into(), "3".into(),
        // jaild's control socket, next after /dev/fuse (no device before
        // it): grants of the share's folders (§4.8).
        "--control-fd".into(), "4".into(),
        "--arca".into(), ARCA_SOCKET.into(),
        "--uid".into(), session_uid.to_string(),
        "--gid".into(), session_gid.to_string(),
    ]);
    if read_only {
        v.push("--ro".into());
    }
    v
}

/// `--learn`'s argv: what the server shows comes back on stdout.
pub fn learn_argv(proto: Proto, host: &str, addr: Ipv4Addr, port: u16) -> Vec<String> {
    let mut v = vec![proto.exec().to_string(), "--host".into()];
    match proto {
        Proto::Sftp => v.push(addr.to_string()),
        Proto::Webdav | Proto::Smb => v.extend([host.to_string(), "--addr".into(), addr.to_string()]),
    }
    v.extend(["--port".into(), port.to_string(), "--learn".into()]);
    v
}

/// What `--learn` printed. SFTP: the fingerprint line. WebDAV: `trusted`
/// or `untrusted <why>`, the fingerprint, then `subject`, `issuer` and
/// `expires` lines, as one text for the file manager (printable, at most
/// 1 KiB). SMB: `smb 3.1.1 <cipher>`, the fingerprint of the server's GUID
/// and `guid <text>`.
pub fn parse_learned(proto: Proto, out: &str) -> Option<String> {
    let fp = out.lines().map(str::trim).find(|l| key_of(l).is_some()).map(String::from)?;
    let clean = |v: Vec<String>| -> String { v.join("\n").chars().filter(|c| *c == '\n' || !c.is_control()).take(1024).collect() };
    match proto {
        Proto::Sftp => Some(fp),
        Proto::Smb => {
            let first = out.lines().next()?.trim();
            let guid = out.lines().map(str::trim).find(|l| l.starts_with("guid "))?;
            if !first.starts_with("smb 3.1.1 ") {
                return None;
            }
            Some(clean(vec![first.to_string(), fp, guid.to_string()]))
        }
        Proto::Webdav => {
            let first = out.lines().next()?.trim();
            if first != "trusted" && !first.starts_with("untrusted") {
                return None;
            }
            let keep = |l: &&str| l.starts_with("subject ") || l.starts_with("issuer ") || l.starts_with("expires ");
            let mut v = vec![first.to_string(), fp];
            v.extend(out.lines().map(str::trim).filter(keep).map(String::from));
            let t: String = v.join("\n").chars().filter(|c| *c == '\n' || !c.is_control()).take(1024).collect();
            Some(t)
        }
    }
}

/// Why `--learn` refused a server (`refused <why>`: an SMB server below
/// the floor, §5.3), for the file manager to show.
pub fn learn_refusal(out: &str) -> Option<String> {
    let why = out.lines().next()?.trim().strip_prefix("refused ")?;
    Some(why.chars().filter(|c| !c.is_control()).take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "SHA256:WmXuVRg10pJRVYDpsFPc/bGwkJRcX4mhTs6Zz3tuWEQ";

    #[test]
    fn fingerprints_decode() {
        let k = key_of(FP).unwrap();
        assert_eq!(k[0], 0x5a);
        assert!(key_of("SHA256:WmXuVRg10pJRVYDpsFPc/bGwkJRcX4mhTs6Zz3tuWEQ=").is_none());
        assert!(key_of("SHA256:WmXuVRg10pJRVYDpsFPc/bGwkJRcX4mhTs6Zz3tuWER").is_none(), "nonzero tail bits");
        assert!(key_of("MD5:aa").is_none());
        assert_eq!(parse_learned(Proto::Sftp, &format!("{FP}\nkey 5a...\n")).as_deref(), Some(FP));
        let w = parse_learned(Proto::Webdav, &format!("untrusted invalid peer certificate: UnknownIssuer\n{FP}\nkey 5a\nsubject my-nas\nissuer my-nas\nexpires 261214\n")).unwrap();
        assert_eq!(w, format!("untrusted invalid peer certificate: UnknownIssuer\n{FP}\nsubject my-nas\nissuer my-nas\nexpires 261214"));
        assert_eq!(parse_learned(Proto::Webdav, &format!("{FP}\n")), None);
        let s = parse_learned(Proto::Smb, &format!("smb 3.1.1 AES-128-GCM\nguid 00006d76-0000-0000-0000-000000000000\n{FP}\nkey 5a\n")).unwrap();
        assert_eq!(s, format!("smb 3.1.1 AES-128-GCM\n{FP}\nguid 00006d76-0000-0000-0000-000000000000"));
        assert_eq!(parse_learned(Proto::Smb, &format!("{FP}\nguid x\n")), None);
        assert_eq!(learn_refusal("refused the server won't encrypt (no AES-GCM)\n").as_deref(), Some("the server won't encrypt (no AES-GCM)"));
        assert_eq!(learn_refusal(&format!("{FP}\n")), None);
    }

    #[test]
    fn requests_are_checked() {
        assert!(check(Proto::Sftp, "nas.lan", 22, "/home/alice", "alice", FP).is_ok());
        assert!(check(Proto::Sftp, "192.168.1.5", 2222, ".", "alice", FP).is_ok());
        for (h, port, p, u, f) in [
            ("NAS.lan", 22, "/x", "alice", FP),
            ("nas.lan", 0, "/x", "alice", FP),
            ("nas.lan", 22, "-oProxyCommand=x", "alice", FP),
            ("nas.lan", 22, "/x\n", "alice", FP),
            ("nas.lan", 22, "/x", "-alice", FP),
            ("nas.lan", 22, "/x", "al ice", FP),
            ("nas.lan", 22, "/x", "alice", "SHA256:short"),
            ("-nas", 22, "/x", "alice", FP),
        ] {
            assert!(check(Proto::Sftp, h, port, p, u, f).is_err(), "{h} {port} {p} {u} {f}");
        }
        assert_eq!(check(Proto::Webdav, "nas.lan", 443, "/dav", "alice", TRUSTED).unwrap().key, [0; 32]);
        assert!(check(Proto::Sftp, "nas.lan", 22, "/x", "alice", TRUSTED).is_err(), "trusted is WebDAV's alone");
        assert!(check(Proto::Webdav, "nas.lan", 443, "dav", "alice", FP).is_err());
        assert!(check(Proto::Smb, "nas.lan", 445, "/files/Photos", "HOME\\alice", FP).is_ok());
        assert!(check(Proto::Smb, "nas.lan", 445, "/", "alice", FP).is_err(), "no share named");
        assert!(check(Proto::Smb, "nas.lan", 445, "/files", "alice", TRUSTED).is_err(), "trusted is WebDAV's alone");
        assert!(!dialable("169.254.169.254".parse().unwrap()));
        assert!(!dialable("127.0.0.1".parse().unwrap()));
        assert!(dialable("192.168.1.5".parse().unwrap()));
    }

    #[test]
    fn uids_parts_and_the_registry_line() {
        let mut used = BTreeSet::new();
        assert_eq!(free_uid(&used), Some(63000));
        used.insert(63000);
        assert_eq!(free_uid(&used), Some(63001));
        assert_eq!(uid_of_part(&part(63001)), Some(63001));
        assert_eq!(uid_of_part("share-063001"), None);
        assert_eq!(uid_of_part("da0s1"), None);
        let s = check(Proto::Sftp, "nas.lan", 22, "/a b", "alice", FP).unwrap();
        let l = registry_line(63001, "alice", &s);
        assert!(l.starts_with("63001 alice sftp nas.lan 22 5a"));
        assert!(l.ends_with(" 2f612062"));
        let a = argv(&s, "192.168.1.5".parse().unwrap(), 1001, 1001, false);
        assert_eq!(a[2], "192.168.1.5");
        assert!(a.windows(2).any(|w| w == ["--fuse-fd", "3"]));
        assert!(a.windows(2).any(|w| w == ["--pin", FP]));
        let w = check(Proto::Webdav, "nas.lan", 443, "/dav", "alice", TRUSTED).unwrap();
        assert!(registry_line(63002, "alice", &w).starts_with("63002 alice webdav nas.lan 443 0000"));
        let a = argv(&w, "192.168.1.5".parse().unwrap(), 1001, 1001, true);
        assert_eq!(&a[..5], [EXEC_WEBDAV, "--host", "nas.lan", "--addr", "192.168.1.5"]);
        assert!(a.contains(&"--trusted".to_string()) && a.contains(&"--ro".to_string()) && !a.contains(&"--pin".to_string()));
        let l = learn_argv(Proto::Webdav, "nas.lan", "192.168.1.5".parse().unwrap(), 443);
        assert_eq!(l, [EXEC_WEBDAV, "--host", "nas.lan", "--addr", "192.168.1.5", "--port", "443", "--learn"]);
    }
}
