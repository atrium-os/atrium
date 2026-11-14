//! Exercise `DavFs` against a real WebDAV server (no FUSE): list, create,
//! stream a file up, edit it in place, read it back, rename, delete.
//!
//! ```text
//! cargo run --example dav-probe -- HOST PORT CA.pem|pin:SHA256:... USER PASSWORD-FILE /dav
//! ```
//! (scripts/webdav-test-server.sh starts a server to point it at.)

use std::net::TcpStream;
use std::sync::Arc;

use capsa_fuse::Backend;
use capsa_share::http::{Client, Dial};
use capsa_share::tls::{self, Identity};
use capsa_share::webdav::{Dav, DavFs};

struct D {
    host: String,
    port: u16,
    cfg: Arc<rustls::ClientConfig>,
}

impl Dial for D {
    type Stream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;
    fn dial(&mut self) -> std::io::Result<Self::Stream> {
        let mut t = TcpStream::connect((self.host.as_str(), self.port))?;
        let mut c = rustls::ClientConnection::new(self.cfg.clone(), tls::server_name(&self.host).unwrap()).map_err(std::io::Error::other)?;
        while c.is_handshaking() {
            c.complete_io(&mut t)?;
        }
        Ok(rustls::StreamOwned::new(c, t))
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (host, port, trust, user, pwfile, root) = (&a[1], a[2].parse::<u16>().unwrap(), &a[3], &a[4], &a[5], &a[6]);
    let (id, roots) = match trust.strip_prefix("pin:") {
        Some(fp) => (Identity::Pinned(capsa_share::cert::key_of(fp).expect("pin")), Vec::new()),
        None => (Identity::Trusted, std::fs::read(trust).unwrap()),
    };
    let (cfg, _) = tls::config(id, &roots).unwrap();
    let mut http = Client::new(D { host: host.clone(), port, cfg }, &format!("{host}:{port}"));
    let pw = std::fs::read(pwfile).unwrap();
    http.set_basic(user, pw.trim_ascii());
    let mut fs = DavFs::new(Dav::new(http, &format!("https://{host}:{port}")), root, true).expect("root");
    let ls = |fs: &mut DavFs<D>| {
        let mut v: Vec<String> = fs.list(1).unwrap().iter().map(|n| format!("{}{}", <DavFs<D> as Backend>::name(n), if <DavFs<D> as Backend>::attr(n).dir { "/" } else { "" })).collect();
        v.sort();
        v.join(",")
    };
    println!("list: {}", ls(&mut fs));
    let mut n = fs.create(1, "probe file.bin", false, 0).unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    for (i, c) in big.chunks(65536).enumerate() {
        fs.write(&mut n, (i * 65536) as u64, c, 0).unwrap();
    }
    fs.sync().unwrap();
    let mut back = vec![0u8; big.len()];
    let mut off = 0;
    while off < back.len() {
        let k = fs.read(&n, off as u64, &mut back[off..(off + 131072).min(big.len())]).unwrap();
        assert!(k > 0);
        off += k;
    }
    assert_eq!(back, big, "streamed up and read back in ranges");
    println!("streamed {} bytes and read them back", big.len());
    fs.write(&mut n, 10, b"EDIT", 0).unwrap();
    fs.sync().unwrap();
    let mut b = [0u8; 4];
    fs.read(&n, 10, &mut b).unwrap();
    assert_eq!(&b, b"EDIT");
    println!("edited in place");
    let d = fs.create(1, "probe dir", true, 0).unwrap();
    let m = fs.rename(&n, <DavFs<D> as Backend>::key(&d), "moved é.bin", 0).unwrap();
    println!("list after rename: {}", ls(&mut fs));
    let sub: Vec<String> = fs.list(<DavFs<D> as Backend>::key(&d)).unwrap().iter().map(|n| <DavFs<D> as Backend>::name(n).to_string()).collect();
    assert_eq!(sub, vec!["moved é.bin".to_string()]);
    assert!(fs.rmdir(&d).is_err(), "not empty");
    fs.unlink(&m).unwrap();
    fs.rmdir(&d).unwrap();
    println!("statfs: {:?}", fs.statfs().unwrap());
    println!("list at the end: {}", ls(&mut fs));
    println!("dav-probe: ok");
}
