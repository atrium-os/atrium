//! Exercise `SmbFs` against a real SMB 3.1.1 server (no FUSE): negotiate,
//! log in, list, create, write, read back, edit in place, rename, delete.
//!
//! ```text
//! cargo run --example smb-probe -- HOST PORT SHARE USER PASSWORD-FILE [--learn]
//! ```
//! (scripts/smb-test-server.sh starts servers to point it at.)

use std::net::TcpStream;
use std::time::Duration;

use capsa_fuse::Backend;
use capsa_share::smb::{cipher_name, guid_text, Conn, SmbFs};

type F = SmbFs<TcpStream>;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (host, port, share, user, pwfile) = (&a[1], a[2].parse::<u16>().unwrap(), &a[3], &a[4], &a[5]);
    let t = TcpStream::connect((host.as_str(), port)).unwrap();
    t.set_read_timeout(Some(Duration::from_secs(25))).unwrap();
    let mut c = match Conn::negotiate(t) {
        Ok(c) => c,
        Err(e) => {
            println!("negotiate: {e}");
            std::process::exit(3);
        }
    };
    println!("guid {} cipher {}", guid_text(&c.negotiated.guid), cipher_name(c.negotiated.cipher));
    if a.get(6).map(String::as_str) == Some("--learn") {
        return;
    }
    let pw = std::fs::read(pwfile).unwrap();
    c.login(user, pw.trim_ascii()).expect("login");
    c.tree_connect(&format!("\\\\{host}\\{share}")).expect("tree");
    let mut fs = SmbFs::new(c, "", true).expect("root");
    let ls = |fs: &mut F| {
        let mut v: Vec<String> = fs.list(1).unwrap().iter().map(|n| format!("{}{}", <F as Backend>::name(n), if <F as Backend>::attr(n).dir { "/" } else { "" })).collect();
        v.sort();
        v.join(",")
    };
    println!("list: {}", ls(&mut fs));
    let mut n = fs.create(1, "probe file.bin", false, 0).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    for (i, c) in big.chunks(131072).enumerate() {
        fs.write(&mut n, (i * 131072) as u64, c, 0).unwrap();
    }
    fs.sync().unwrap();
    let mut back = vec![0u8; big.len()];
    let mut off = 0;
    while off < back.len() {
        let k = fs.read(&n, off as u64, &mut back[off..(off + 131072).min(big.len())]).unwrap();
        assert!(k > 0);
        off += k;
    }
    assert_eq!(back, big, "written and read back");
    println!("wrote {} bytes and read them back", big.len());
    fs.write(&mut n, 10, b"EDIT", 0).unwrap();
    fs.sync().unwrap();
    let mut b = [0u8; 4];
    fs.read(&n, 10, &mut b).unwrap();
    assert_eq!(&b, b"EDIT");
    fs.truncate(&mut n, 100, 0).unwrap();
    fs.set_mtime(&mut n, 1_000_000_000).unwrap();
    fs.sync().unwrap();
    let r = fs.refresh(<F as Backend>::key(&n)).unwrap();
    assert_eq!((<F as Backend>::attr(&r).size, <F as Backend>::attr(&r).mtime), (100, 1_000_000_000));
    println!("edited in place, truncated, mtime set");
    let d = fs.create(1, "probe dir", true, 0).unwrap();
    let m = fs.rename(&n, <F as Backend>::key(&d), "moved é.bin", 0).unwrap();
    println!("list after rename: {}", ls(&mut fs));
    let sub: Vec<String> = fs.list(<F as Backend>::key(&d)).unwrap().iter().map(|n| <F as Backend>::name(n).to_string()).collect();
    assert_eq!(sub, vec!["moved é.bin".to_string()]);
    assert!(fs.rmdir(&d).is_err(), "not empty");
    fs.unlink(&m).unwrap();
    fs.rmdir(&d).unwrap();
    assert!(fs.lookup(1, "probe dir").is_err());
    println!("statfs: {:?}", fs.statfs().unwrap());
    println!("list at the end: {}", ls(&mut fs));
    println!("smb-probe: ok");
}
