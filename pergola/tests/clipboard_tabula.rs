//! Pergola's clipboard client against the real `tabulad` server code
//! (with a stand-in frescod): what TextField's Ctrl-C/V do on the wire.

use fresco_protocol::activation::kind;
use pergola::clipboard::{Backend, Error, Tabula};

#[test]
fn copy_and_paste_through_tabula() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tabulad.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let fake = tabula::gate::Fake::default();
    let shared = tabula::server::Shared::new(Box::new(fake.clone()));
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let s = s.unwrap();
            let shared = shared.clone();
            // The test process is both apps; Tabula sees its real uid.
            let uid = unsafe { libc::getuid() };
            std::thread::spawn(move || {
                let _ = tabula::server::serve(aqueduct::Connection::wrap(s).unwrap(), uid, shared);
            });
        }
    });
    let me = unsafe { libc::getuid() };
    let mut a = Tabula::at(&path).unwrap();
    let mut b = Tabula::at(&path).unwrap();
    a.set(fake.mint(me, kind::KEY), &[("text/plain", b"from a")], false).unwrap();
    let got = b.get(fake.mint(me, kind::KEY), &["text/plain"]).unwrap();
    assert_eq!(got, Some(("text/plain".into(), b"from a".to_vec())));
    // A token that frescod never minted.
    assert!(matches!(b.get([1; 16], &["text/plain"]), Err(Error::Refused { .. })));
}
