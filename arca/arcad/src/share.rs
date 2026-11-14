//! Share servers' identity (network-shares.md §5.1).
//!
//! A share server (`atrium-share-sftp`, one per mounted share) runs under a
//! uid from its own reserved range, and dispensatord, which starts it,
//! records that uid's label in the share registry: the user and the one
//! share, with the server identity pinned when the share was saved. arcad
//! reads the label by uid and never believes the share server about it, so
//! a share server can only ever reach its own share's items.
//!
//! Registry lines: `uid user sftp host port key-hex path-hex`. The key is
//! the SHA-256 of the server's SSH host key blob (64 hex digits); the path
//! is hex so that a folder with spaces stays one field. `uid retired` ends
//! a label, as in the courier registry (`crate::courier::lookup_with`).

use arca_store::codec::{Reader, Writer};
use arca_store::model::{caps, valid_user};
use arca_store::{Recipient, ShareProto};

/// The share-server uid range: inside jaild's policy window, below the
/// couriers' (64000..), clear of the app range's growth.
pub const UID_FIRST: u32 = 63_000;
pub const UID_LAST: u32 = 63_499;

pub const DEFAULT_REGISTRY: &str = "/var/run/atrium/share-registry";

/// The longest share path, in bytes (arca-store's own cap).
pub const PATH_MAX: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareLabel {
    pub user: String,
    /// Always `Recipient::Share`.
    pub share: Recipient,
}

impl ShareLabel {
    /// The pinned server identity.
    pub fn key(&self) -> &[u8; 32] {
        match &self.share {
            Recipient::Share { key, .. } => key,
            _ => unreachable!("a share label's recipient is a share"),
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).filter(|p| p.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))).and_then(|p| u8::from_str_radix(p, 16).ok()))
        .collect()
}

pub fn parse_line(line: &str) -> Option<(u32, ShareLabel)> {
    let f: Vec<&str> = line.split_ascii_whitespace().collect();
    if f.len() != 7 {
        return None;
    }
    let uid: u32 = f[0].parse().ok()?;
    if !(UID_FIRST..=UID_LAST).contains(&uid) || !valid_user(f[1]) || f[2] != "sftp" {
        return None;
    }
    let port: u16 = f[4].parse().ok()?;
    let key: [u8; 32] = unhex(f[5])?.try_into().ok()?;
    let path = String::from_utf8(unhex(f[6])?).ok()?;
    let share = Recipient::share(ShareProto::Sftp, f[3], port, &path, key).ok()?;
    Some((uid, ShareLabel { user: f[1].into(), share }))
}

/// The label of share server `uid`, read fresh from `path`.
pub fn lookup(path: &str, uid: u32) -> Option<ShareLabel> {
    crate::courier::lookup_with(path, uid, |l| parse_line(l).map(|(_, s)| s))
}

/// dispensatord's side: the registry line for `uid`.
pub fn label_line(uid: u32, l: &ShareLabel) -> Option<String> {
    let Recipient::Share { proto: ShareProto::Sftp, host, port, path, key } = &l.share else { return None };
    Some(format!("{uid} {} sftp {host} {port} {} {}", l.user, hex(key), hex(path.as_bytes())))
}

/// A share on the wire: `u8 proto (1 sftp) | str host | u16 port | str path
/// | key[32]`.
pub fn read_share(r: &mut Reader<'_>) -> Result<Recipient, arca_store::Error> {
    let proto = match r.u8()? {
        1 => ShareProto::Sftp,
        _ => return Err(arca_store::Error::BadRecipient),
    };
    let host = r.str(caps::HOST)?;
    let port = r.u16()?;
    let path = r.str(PATH_MAX)?;
    let key = r.array::<32>()?;
    Recipient::share(proto, host, port, path, key)
}

pub fn write_share(w: &mut Writer, s: &Recipient) {
    if let Recipient::Share { proto, host, port, path, key } = s {
        w.u8(*proto as u8);
        w.str(host);
        w.u16(*port);
        w.str(path);
        w.raw(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(uid: u32, path: &str) -> String {
        format!("{uid} alice sftp nas.lan 22 {} {}", "ab".repeat(32), hex(path.as_bytes()))
    }

    #[test]
    fn lines() {
        let (uid, l) = parse_line(&line(63001, "/home/alice/My Files")).unwrap();
        assert_eq!(uid, 63001);
        assert_eq!(l.key(), &[0xab; 32]);
        let Recipient::Share { path, host, .. } = &l.share else { panic!() };
        assert_eq!((host.as_str(), path.as_str()), ("nas.lan", "/home/alice/My Files"));
        assert_eq!(label_line(63001, &l).unwrap(), line(63001, "/home/alice/My Files"));
        assert!(parse_line(&format!("63001 alice sftp 192.168.1.5 2222 {} 2f", "00".repeat(32))).is_some());
        for bad in [
            line(64001, "/x"),                                                     // a courier uid
            line(62999, "/x"),                                                     // below the range
            format!("63001 alice smb nas.lan 22 {} 2f", "ab".repeat(32)),           // protocol
            format!("63001 alice sftp nas.lan 22 {} 2f", "AB".repeat(32)),          // not canonical hex
            format!("63001 alice sftp nas.lan 22 {} 2f", "ab".repeat(31)),          // short key
            format!("63001 alice sftp nas.lan 0 {} 2f", "ab".repeat(32)),           // port
            format!("63001 alice sftp NAS.lan 22 {} 2f", "ab".repeat(32)),          // host
            format!("63001 alice sftp nas.lan 22 {} 0a", "ab".repeat(32)),          // control char
            format!("63001 alice sftp nas.lan 22 {} ", "ab".repeat(32)),            // no path
            format!("63001 alice sftp nas.lan 22 {} 2f x", "ab".repeat(32)),
        ] {
            assert_eq!(parse_line(&bad), None, "{bad}");
        }
    }

    #[test]
    fn retire_and_duplicates_follow_the_courier_rule() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r");
        let p = p.to_str().unwrap();
        std::fs::write(p, format!("{}\n{}\n", line(63001, "/a"), line(63001, "/b"))).unwrap();
        assert_eq!(lookup(p, 63001), None, "two live labels");
        std::fs::write(p, format!("{}\n63001 retired\n{}\n", line(63001, "/a"), line(63001, "/b"))).unwrap();
        let Recipient::Share { path, .. } = lookup(p, 63001).unwrap().share else { panic!() };
        assert_eq!(path, "/b");
    }

    #[test]
    fn wire_round_trip() {
        let s = Recipient::share(ShareProto::Sftp, "nas.lan", 22, "/srv", [3; 32]).unwrap();
        let mut w = Writer::new();
        write_share(&mut w, &s);
        assert_eq!(read_share(&mut Reader::new(&w.buf)).unwrap(), s);
    }
}
