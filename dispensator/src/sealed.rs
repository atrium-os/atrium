//! Sealed Tessera sticks (tessera-encryption.md §7): what dispensatord
//! reads from a stick, and what it asks arcad.
//!
//! A stick is trusted only when it opens with a key this machine holds for
//! the session's person (device-assignment.md §4.6). dispensatord reads the
//! two superblock copies' fixed clear fields (magic, the SEALED bit, the
//! volume uuid and the key id: four fields at fixed offsets, nothing
//! variable) and asks arcad to load the key. Everything else, the HMAC
//! included, is checked by the kernel when it mounts: a header that lies
//! about its key id or uuid only fails that mount.
//!
//! arcad tells services apart by uid, and dispensatord is root, which arcad
//! serves no one as. So the arcad calls run in a child process as
//! [`VK_UID`] (`_dispensator-vk`), named in arcad's `ARCA_SERVICES` as
//! `dispensatord`; the child does one call and prints the answer.

/// The uid dispensatord's arcad calls run as.
pub const VK_UID: u32 = 65043;

pub const SECTOR: usize = 4096;
const MAGIC: &[u8; 8] = b"TESSERA1";
const INCOMPAT_SEALED: u32 = 1 << 1;
const OFF_INCOMPAT: usize = 20;
const OFF_UUID: usize = 32;
const OFF_KEY_ID: usize = 264;
/// `key_slots[0]` (format.h): a recovery slot (§7.4) when its type is 3.
const OFF_SLOT0: usize = 280;
pub const SLOT_LEN: usize = 256;
const SLOT_RECOVERY: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    /// Not Tessera (FAT, exFAT, nothing): the FUSE server's.
    Other,
    /// Tessera, not sealed: the read-only FUSE reader's (§6.9).
    Plain,
    /// Sealed: only the kernel mounts it, with the key. `recovery`: it
    /// carries a recovery slot (§7.4).
    Sealed { key_id: [u8; 16], uuid: [u8; 16], recovery: bool },
}

fn one(sb: &[u8]) -> Header {
    if sb.len() < SECTOR || &sb[..8] != MAGIC {
        return Header::Other;
    }
    let incompat = u32::from_le_bytes(sb[OFF_INCOMPAT..OFF_INCOMPAT + 4].try_into().unwrap());
    if incompat & INCOMPAT_SEALED == 0 {
        return Header::Plain;
    }
    Header::Sealed {
        key_id: sb[OFF_KEY_ID..OFF_KEY_ID + 16].try_into().unwrap(),
        uuid: sb[OFF_UUID..OFF_UUID + 16].try_into().unwrap(),
        recovery: sb[OFF_SLOT0] == SLOT_RECOVERY,
    }
}

/// The recovery slot of the copy [`header`] chose, as bytes for arcad. Not
/// authenticated here: arcad's unwrap is bound to the uuid and key id, and
/// the kernel checks the HMAC over all of it at mount.
pub fn recovery_slot(first_two_sectors: &[u8]) -> Option<[u8; SLOT_LEN]> {
    let copy = |i: usize| first_two_sectors.get(i * SECTOR..(i + 1) * SECTOR).unwrap_or(&[]);
    let i = if matches!(one(copy(0)), Header::Sealed { .. }) { 0 } else { 1 };
    match one(copy(i)) {
        Header::Sealed { recovery: true, .. } => copy(i)[OFF_SLOT0..OFF_SLOT0 + SLOT_LEN].try_into().ok(),
        _ => None,
    }
}

/// What the first two sectors (superblock copies A and B) say. A copy that
/// says sealed wins: a stick with one sealed copy is never handed to a
/// reader that would refuse it anyway.
pub fn header(first_two_sectors: &[u8]) -> Header {
    let a = one(first_two_sectors.get(..SECTOR).unwrap_or(&[]));
    let b = one(first_two_sectors.get(SECTOR..2 * SECTOR).unwrap_or(&[]));
    match (a, b) {
        (s @ Header::Sealed { .. }, _) | (_, s @ Header::Sealed { .. }) => s,
        (Header::Plain, _) | (_, Header::Plain) => Header::Plain,
        _ => Header::Other,
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
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

/// arcad's wire (arca/arcad/src/proto.rs): a request frame is
/// `u32 len | u16 op | body`, a reply `u32 len | u8 status | body`; a
/// string is `u32 len | bytes`.
pub mod arca {
    use std::io::{self, Read, Write};

    pub const SOCKET: &str = "/atrium/sockets/arca/arcad.sock";
    pub const VK_MINT: u16 = 0xB0;
    pub const VK_RECORD: u16 = 0xB2;
    pub const VK_FIND: u16 = 0xB4;
    pub const VK_RECOVERY: u16 = 0xB5;
    pub const VK_ADOPT: u16 = 0xB6;
    pub const OK: u8 = 0;
    pub const DENIED: u8 = 1;
    pub const LOCKED: u8 = 2;
    pub const MALFORMED: u8 = 4;

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

    /// `find USER KEYID UUID`, `mint USER PERSONA UUID LABEL`,
    /// `record USER PERSONA KEYID`, `recovery USER PERSONA KEYID UUID`,
    /// `adopt USER PERSONA KEYID UUID SLOT KEEP` (the code is `code`, which
    /// the child reads from its stdin, never its arguments): the request
    /// the child sends.
    pub fn request(args: &[String], code: &str) -> Option<(u16, Vec<u8>)> {
        let a = |i: usize| args.get(i).map(String::as_str);
        match (a(0)?, args.len()) {
            ("recovery", 5) => Some((VK_RECOVERY, Body::default().str(a(1)?).str(a(2)?).raw(&super::unhex16(a(3)?)?).raw(&super::unhex16(a(4)?)?).0)),
            ("adopt", 7) => {
                let slot = super::unhex(a(5)?)?;
                if slot.len() != super::SLOT_LEN || code.is_empty() {
                    return None;
                }
                let keep = match a(6)? {
                    "keep" => 1u8,
                    "once" => 0,
                    _ => return None,
                };
                Some((VK_ADOPT, Body::default().str(a(1)?).str(a(2)?).raw(&super::unhex16(a(3)?)?).raw(&super::unhex16(a(4)?)?).raw(&slot).raw(&[keep]).str(code).0))
            }
            ("find", 4) => Some((VK_FIND, Body::default().str(a(1)?).raw(&super::unhex16(a(2)?)?).raw(&super::unhex16(a(3)?)?).u64(0).0)),
            ("mint", 5) => Some((VK_MINT, Body::default().str(a(1)?).str(a(2)?).raw(&super::unhex16(a(3)?)?).u64(0).str(a(4)?).0)),
            ("record", 4) => Some((VK_RECORD, Body::default().str(a(1)?).str(a(2)?).raw(&super::unhex16(a(3)?)?).0)),
            _ => None,
        }
    }

    /// The child's one line: `ok <answer>` or `status <n>`. find answers
    /// the persona, mint the key id (hex), record the high-water mark.
    pub fn answer(op: u16, st: u8, body: &[u8]) -> String {
        if st != OK {
            return format!("status {st}");
        }
        match op {
            VK_FIND => format!("ok {}", String::from_utf8_lossy(body)),
            VK_MINT if body.len() == 16 => format!("ok {}", super::hex(body)),
            VK_RECORD if body.len() == 8 => format!("ok {}", u64::from_le_bytes(body.try_into().unwrap())),
            // The slot (hex), then the code.
            VK_RECOVERY if body.len() > super::SLOT_LEN && body[super::SLOT_LEN..].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-') => {
                format!("ok {} {}", super::hex(&body[..super::SLOT_LEN]), String::from_utf8_lossy(&body[super::SLOT_LEN..]))
            }
            VK_ADOPT if body.is_empty() => "ok".into(),
            _ => "status 255".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb(sealed: bool) -> Vec<u8> {
        let mut b = vec![0u8; SECTOR];
        b[..8].copy_from_slice(MAGIC);
        if sealed {
            b[OFF_INCOMPAT] = 2;
            b[OFF_UUID..OFF_UUID + 16].copy_from_slice(&[0xa0; 16]);
            b[OFF_KEY_ID..OFF_KEY_ID + 16].copy_from_slice(&[0x11; 16]);
        }
        b
    }

    #[test]
    fn reads_the_clear_fields() {
        // The bytes tessera-mkfs-sealed wrote for key id 0011..ff, uuid
        // a0a1..af (od of a real image, 2026-11-17).
        let mut real = vec![0u8; SECTOR];
        real[..8].copy_from_slice(b"TESSERA1");
        real[8] = 1;
        real[16] = 9;
        real[20] = 2;
        real[24] = 1;
        for i in 0..16 {
            real[32 + i] = 0xa0 + i as u8;
            real[264 + i] = (i as u8) * 0x11;
        }
        real[256] = 1;
        real[259] = 1;
        let two = [real.clone(), real].concat();
        assert_eq!(
            header(&two),
            Header::Sealed {
                key_id: unhex16("00112233445566778899aabbccddeeff").unwrap(),
                uuid: unhex16("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf").unwrap(),
                recovery: false,
            }
        );
    }

    #[test]
    fn either_copy_and_the_rest() {
        let s = Header::Sealed { key_id: [0x11; 16], uuid: [0xa0; 16], recovery: false };
        assert_eq!(header(&[sb(true), sb(true)].concat()), s);
        assert_eq!(header(&[vec![0; SECTOR], sb(true)].concat()), s, "copy A damaged");
        assert_eq!(header(&[sb(false), sb(true)].concat()), s, "one sealed copy is enough");
        assert_eq!(header(&[sb(false), sb(false)].concat()), Header::Plain);
        assert_eq!(header(&[vec![0xeb; SECTOR], vec![0; SECTOR]].concat()), Header::Other);
        assert_eq!(header(&[0u8; 100]), Header::Other, "short read");
        assert_eq!(header(&[]), Header::Other);
    }

    #[test]
    fn arca_requests_and_answers() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let k = "00112233445566778899aabbccddeeff";
        let (op, b) = arca::request(&s(&["find", "alice", k, k]), "").unwrap();
        assert_eq!(op, arca::VK_FIND);
        assert_eq!(b.len(), 4 + 5 + 16 + 16 + 8);
        let (op, b) = arca::request(&s(&["mint", "alice", "work", k, "Work stick"]), "").unwrap();
        assert_eq!(op, arca::VK_MINT);
        assert_eq!(b.len(), 9 + 8 + 16 + 8 + 4 + 10);
        assert!(arca::request(&s(&["mint", "alice", "work", "zz", "x"]), "").is_none());
        assert!(arca::request(&s(&["release", "x"]), "").is_none());
        assert_eq!(arca::answer(arca::VK_FIND, 0, b"work"), "ok work");
        assert_eq!(arca::answer(arca::VK_MINT, 0, &[0x11; 16]), format!("ok {}", "11".repeat(16)));
        assert_eq!(arca::answer(arca::VK_RECORD, 0, &57u64.to_le_bytes()), "ok 57");
        assert_eq!(arca::answer(arca::VK_FIND, arca::DENIED, b""), "status 1");
        // A reply that isn't the expected shape is never taken for a key id.
        assert_eq!(arca::answer(arca::VK_MINT, 0, &[0; 32]), "status 255");
    }

    #[test]
    fn recovery_slots_and_their_requests() {
        let mut a = sb(true);
        a[OFF_SLOT0] = 3;
        a[OFF_SLOT0 + 1] = 1;
        a[OFF_SLOT0 + 255] = 0xee;
        let two = [a.clone(), a.clone()].concat();
        assert!(matches!(header(&two), Header::Sealed { recovery: true, .. }));
        let slot = recovery_slot(&two).unwrap();
        assert_eq!((slot[0], slot[255]), (3, 0xee));
        assert_eq!(recovery_slot(&[sb(true), sb(true)].concat()), None);
        assert_eq!(recovery_slot(&[vec![0; SECTOR], a.clone()].concat()).unwrap()[255], 0xee, "copy A damaged");
        assert_eq!(recovery_slot(&[sb(false), sb(false)].concat()), None);

        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let k = "00112233445566778899aabbccddeeff";
        let h = hex(&slot);
        let (op, b) = arca::request(&s(&["adopt", "alice", "personal", k, k, &h, "keep"]), "ABCDE-FGHJK").unwrap();
        assert_eq!(op, arca::VK_ADOPT);
        assert_eq!(b.len(), 4 + 5 + 4 + 8 + 16 + 16 + 256 + 1 + 4 + 11);
        assert!(arca::request(&s(&["adopt", "alice", "personal", k, k, &h, "keep"]), "").is_none(), "no code");
        assert!(arca::request(&s(&["adopt", "alice", "personal", k, k, "00", "keep"]), "x").is_none(), "short slot");
        assert!(arca::request(&s(&["adopt", "alice", "personal", k, k, &h, "maybe"]), "x").is_none());
        assert!(arca::request(&s(&["recovery", "alice", "personal", k, k]), "").is_some());
        let mut reply = slot.to_vec();
        reply.extend_from_slice(b"ABCDE-FGHJK");
        assert_eq!(arca::answer(arca::VK_RECOVERY, 0, &reply), format!("ok {h} ABCDE-FGHJK"));
        reply.push(b' ');
        assert_eq!(arca::answer(arca::VK_RECOVERY, 0, &reply), "status 255", "nothing but a code after the slot");
        assert_eq!(arca::answer(arca::VK_ADOPT, 0, b""), "ok");
    }

    #[test]
    fn the_wire_round_trips() {
        struct Pipe(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl std::io::Read for Pipe {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(b)
            }
        }
        impl std::io::Write for Pipe {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.1.write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut reply = 5u32.to_le_bytes().to_vec();
        reply.push(0);
        reply.extend_from_slice(b"work");
        let mut p = Pipe(std::io::Cursor::new(reply), Vec::new());
        assert_eq!(arca::call(&mut p, arca::VK_FIND, b"xy").unwrap(), (0, b"work".to_vec()));
        assert_eq!(p.1, [4, 0, 0, 0, 0xb4, 0, b'x', b'y']);
    }
}
