//! Pergola's clipboard client against the real `tabulad` server code
//! (with a stand-in frescod): what TextField's Ctrl-C/V do on the wire.

use fresco_protocol::activation::kind;
use pergola::clipboard::{Backend, Error, Tabula};

fn server() -> (tempfile::TempDir, std::path::PathBuf, tabula::gate::Fake) {
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
    (dir, path, fake)
}

#[test]
fn copy_and_paste_through_tabula() {
    let (_dir, path, fake) = server();
    let me = unsafe { libc::getuid() };
    let mut a = Tabula::at(&path).unwrap();
    let mut b = Tabula::at(&path).unwrap();
    a.set(fake.mint(me, kind::KEY), &[("text/plain", b"from a")], false).unwrap();
    let got = b.get(fake.mint(me, kind::KEY), &["text/plain"]).unwrap();
    assert_eq!(got, Some(("text/plain".into(), b"from a".to_vec())));
    // A token that frescod never minted.
    assert!(matches!(b.get([1; 16], &["text/plain"]), Err(Error::Refused { .. })));
}

#[test]
fn a_drop_is_taken_with_frescods_drop_token() {
    use aqueduct::flag;
    use tabula::proto::{op, DragBegin, Format, Reply, CLASS};
    let (_dir, path, fake) = server();
    let me = unsafe { libc::getuid() };
    // The source starts the drag (Pergola starts none yet: raw wire).
    let mut src = aqueduct::Connection::connect(&path).unwrap();
    let hash = src.upload_blob(b"dragged").unwrap();
    let req = DragBegin { token: fake.mint(me, kind::BUTTON), formats: vec![Format { mime: "text/plain".into(), hash }] };
    src.send_message(CLASS, op::DRAG_BEGIN, flag::RESPONSE_EXPECTED, &postcard::to_stdvec(&req).unwrap()).unwrap();
    let drag_id = loop {
        let m = src.recv_message_or_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        if m.flags & flag::IS_RESPONSE != 0 {
            match postcard::from_bytes::<Reply>(&m.payload).unwrap() {
                Reply::Dragging { drag_id } => break drag_id,
                other => panic!("{other:?}"),
            }
        }
    };
    let mut target = Tabula::at(&path).unwrap();
    // A press is not a drop.
    assert!(matches!(target.drop_get(fake.mint(me, kind::BUTTON), &["text/plain"]), Err(Error::Refused { .. })));
    let drop = fake.mint_drop(me, drag_id);
    assert_eq!(target.drop_get(drop, &["text/plain"]).unwrap(), Some(("text/plain".into(), b"dragged".to_vec())));
    // Once.
    assert!(target.drop_get(drop, &["text/plain"]).is_err());
}
