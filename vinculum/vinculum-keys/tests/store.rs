use std::os::unix::fs::PermissionsExt;

use vinculum_keys::{find, load_or_create_machine_key, Address, Bond, Error, FileStore, KeyStore, MemoryStore};
use vinculum_smp::crypto::ah;

fn bond(n: u8) -> Bond {
    Bond {
        address: Address { kind: 1, bytes: [n, 2, 3, 4, 5, 0xC6] },
        identity: Some(Address { kind: 0, bytes: [n, 9, 9, 9, 9, 9] }),
        ltk: [n; 16],
        key_size: 16,
        secure_connections: true,
        authenticated: n % 2 == 0,
        peer_irk: Some([0x40 + n; 16]),
        paired_by: 1001,
        name: format!("Keyboard {n}"),
    }
}

#[test]
fn record_round_trip_and_rejects() {
    for b in [bond(1), Bond { identity: None, peer_irk: None, name: String::new(), ..bond(2) }] {
        assert_eq!(Bond::decode(&b.encode()).unwrap(), b);
    }
    let e = bond(1).encode();
    assert!(Bond::decode(&e[..e.len() - 1]).is_err(), "truncated name");
    let mut v = e.clone();
    v[0] = 2;
    assert!(Bond::decode(&v).is_err(), "unknown version");
    let mut v = e.clone();
    v[2] = 3;
    assert!(Bond::decode(&v).is_err(), "key size");
    let mut v = e.clone();
    v[1] |= 0x80;
    assert!(Bond::decode(&v).is_err(), "flags");
    let long = Bond { name: "x".repeat(200), ..bond(3) };
    assert_eq!(Bond::decode(&long.encode()).unwrap().name.len(), 64);
    assert!(!format!("{:?}", bond(1)).contains("[1, 1, 1"), "Debug redacts keys");
}

#[test]
fn file_store_seals_persists_and_deletes() {
    let d = tempfile::tempdir().unwrap();
    let key = load_or_create_machine_key(&d.path().join("machine.key")).unwrap();
    assert_eq!(std::fs::metadata(d.path().join("machine.key")).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(load_or_create_machine_key(&d.path().join("machine.key")).unwrap(), key, "made once");
    let dir = d.path().join("keys");
    let (a, b) = {
        let mut s = FileStore::open(&dir, &key).unwrap();
        (s.store(&bond(1)).unwrap(), s.store(&bond(2)).unwrap())
    };
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    for e in std::fs::read_dir(&dir).unwrap() {
        let e = e.unwrap();
        assert_eq!(e.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let raw = std::fs::read(e.path()).unwrap();
        assert!(!raw.windows(16).any(|w| w == [1; 16] || w == [2; 16]), "no plaintext key on disk");
    }
    let mut s = FileStore::open(&dir, &key).unwrap();
    let all = s.load_all().unwrap();
    assert_eq!(all.len(), 2);
    let get = |id| all.iter().find(|(i, _)| *i == id).unwrap().1.as_ref().unwrap().clone();
    assert_eq!(get(a), bond(1));
    assert_eq!(get(b), bond(2));
    s.delete(&a).unwrap();
    s.delete(&a).unwrap();
    assert_eq!(s.load_all().unwrap().len(), 1);
}

#[test]
fn file_store_fails_closed() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("keys");
    let id = FileStore::open(&dir, &[7; 32]).unwrap().store(&bond(1)).unwrap();
    let all = FileStore::open(&dir, &[8; 32]).unwrap().load_all().unwrap();
    assert!(matches!(all[0].1, Err(Error::Corrupt(_))), "wrong machine key");

    // A file renamed onto another id does not open.
    let hex = |id: &[u8; 16]| id.iter().map(|b| format!("{b:02x}")).collect::<String>();
    std::fs::rename(dir.join(hex(&id)), dir.join("00".repeat(16))).unwrap();
    let all = FileStore::open(&dir, &[7; 32]).unwrap().load_all().unwrap();
    assert!(matches!(all[0].1, Err(Error::Corrupt(_))));

    // Stray files are ignored; a bad machine key file is an error, not replaced.
    std::fs::write(dir.join("README"), b"x").unwrap();
    assert_eq!(FileStore::open(&dir, &[7; 32]).unwrap().load_all().unwrap().len(), 1);
    std::fs::write(d.path().join("short.key"), [1; 31]).unwrap();
    assert!(load_or_create_machine_key(&d.path().join("short.key")).is_err());
    assert_eq!(std::fs::read(d.path().join("short.key")).unwrap(), vec![1; 31]);
}

#[test]
fn find_by_address_identity_or_rpa() {
    let mut m = MemoryStore::default();
    m.store(&bond(1)).unwrap();
    m.store(&bond(2)).unwrap();
    let bonds: Vec<Bond> = m.load_all().unwrap().into_iter().map(|(_, b)| b.unwrap()).collect();
    assert_eq!(find(&bonds, &bond(2).address).unwrap().name, "Keyboard 2");
    assert_eq!(find(&bonds, &bond(1).identity.unwrap()).unwrap().name, "Keyboard 1");
    // A fresh resolvable private address from bond 2's IRK.
    let prand = [0x11, 0x22, 0x40 | 0x33];
    let hash = ah(&[0x42; 16], &prand);
    let rpa = Address { kind: 1, bytes: [hash[0], hash[1], hash[2], prand[0], prand[1], prand[2]] };
    assert_eq!(find(&bonds, &rpa).unwrap().name, "Keyboard 2");
    let other = Address { kind: 1, bytes: [0, 0, 0, prand[0], prand[1], prand[2]] };
    assert!(find(&bonds, &other).is_none());
}

#[cfg(feature = "atrium")]
#[test]
fn arca_store_speaks_arcads_wire() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use vinculum_keys::arca::ArcaStore;

    // A stand-in arcad holding items in memory, decoding exactly the frames
    // arca.md §6.4 specifies.
    let (client, mut server) = UnixStream::pair().unwrap();
    let t = std::thread::spawn(move || {
        let mut items: Vec<([u8; 16], String, Vec<u8>)> = Vec::new();
        let mut seen = Vec::new();
        loop {
            let mut len = [0u8; 4];
            if server.read_exact(&mut len).is_err() {
                return seen;
            }
            let mut f = vec![0u8; u32::from_le_bytes(len) as usize];
            server.read_exact(&mut f).unwrap();
            let op = u16::from_le_bytes([f[0], f[1]]);
            let b = &f[2..];
            seen.push(op);
            let mut reply = vec![0u8];
            match op {
                0x04 => {
                    assert_eq!(u16::from_le_bytes([b[0], b[1]]), 7, "kind BluetoothKey");
                    assert_eq!(b[2], 1, "class after boot");
                    let an = u32::from_le_bytes(b[3..7].try_into().unwrap()) as usize;
                    let account = String::from_utf8(b[7..7 + an].to_vec()).unwrap();
                    let vn = u32::from_le_bytes(b[7 + an..11 + an].try_into().unwrap()) as usize;
                    let value = b[11 + an..11 + an + vn].to_vec();
                    assert_eq!(b.len(), 11 + an + vn);
                    let id = [items.len() as u8 + 1; 16];
                    items.push((id, account, value));
                    reply.extend_from_slice(&id);
                }
                0x06 => {
                    reply.extend_from_slice(&(items.len() as u32 + 1).to_le_bytes());
                    for (id, account, _) in &items {
                        reply.extend_from_slice(id);
                        reply.extend_from_slice(&7u16.to_le_bytes());
                        reply.push(1);
                        reply.extend_from_slice(&0u32.to_le_bytes());
                        reply.extend_from_slice(&(account.len() as u32).to_le_bytes());
                        reply.extend_from_slice(account.as_bytes());
                    }
                    // An item of another kind is skipped.
                    reply.extend_from_slice(&[0xEE; 16]);
                    reply.extend_from_slice(&3u16.to_le_bytes());
                    reply.push(1);
                    reply.extend_from_slice(&0u32.to_le_bytes());
                    reply.extend_from_slice(&0u32.to_le_bytes());
                }
                0x05 => {
                    let (_, _, v) = items.iter().find(|(id, _, _)| id[..] == b[..16]).unwrap();
                    reply.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    reply.extend_from_slice(v);
                }
                0x07 => items.retain(|(id, _, _)| id[..] != b[..16]),
                _ => reply[0] = 1,
            }
            server.write_all(&(reply.len() as u32).to_le_bytes()).unwrap();
            server.write_all(&reply).unwrap();
        }
    });
    let mut s = ArcaStore::from_stream(client);
    let id = s.store(&bond(4)).unwrap();
    let all = s.load_all().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, id);
    assert_eq!(all[0].1.as_ref().unwrap(), &bond(4));
    s.delete(&id).unwrap();
    assert!(s.load_all().unwrap().is_empty());
    drop(s);
    assert_eq!(t.join().unwrap(), vec![0x04, 0x06, 0x05, 0x07, 0x06]);
}

#[cfg(feature = "atrium")]
#[test]
fn arca_locked_fails_closed() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let (client, mut server) = UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        let mut len = [0u8; 4];
        server.read_exact(&mut len).unwrap();
        let mut f = vec![0u8; u32::from_le_bytes(len) as usize];
        server.read_exact(&mut f).unwrap();
        server.write_all(&1u32.to_le_bytes()).unwrap();
        server.write_all(&[2]).unwrap(); // LOCKED
    });
    let mut s = vinculum_keys::arca::ArcaStore::from_stream(client);
    assert!(matches!(s.load_all(), Err(Error::Refused(_))));
}
