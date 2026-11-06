//! Tabula over a real socket, with a stand-in frescod: copy and paste
//! between two apps, and every way a request without a gesture fails.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use aqueduct::{flag, Connection};
use fresco_protocol::activation::{kind, refusal as fr, Token};
use tabula::gate::Fake;
use tabula::proto::{self, op, refusal, Format, Reply};
use tabula::server::{serve, Shared};

const EDITOR: u32 = 1001;
const VIEWER: u32 = 1002;

/// An app's connection to a Tabula that sees it as `uid`.
fn app(shared: &std::sync::Arc<Shared>, uid: u32) -> Connection {
    let (a, b) = UnixStream::pair().unwrap();
    let shared = shared.clone();
    std::thread::spawn(move || {
        let _ = serve(Connection::wrap(b).unwrap(), uid, shared);
    });
    let mut c = Connection::wrap(a).unwrap();
    c.set_cache_cap(64 << 20);
    c
}

/// Send a request, return the reply (skipping change events).
fn call<T: serde::Serialize>(c: &mut Connection, o: u16, req: &T) -> Reply {
    let payload = postcard::to_stdvec(req).unwrap();
    c.send_message(proto::CLASS, o, flag::RESPONSE_EXPECTED, &payload).unwrap();
    loop {
        let m = c.recv_message_or_timeout(Duration::from_secs(5)).unwrap().expect("reply");
        if m.op == op::EV_CHANGED {
            continue;
        }
        assert!(m.flags & flag::IS_RESPONSE != 0);
        return postcard::from_bytes(&m.payload).unwrap();
    }
}

fn copy(c: &mut Connection, token: Token, items: &[(&str, &[u8])], sensitive: bool) -> Reply {
    let formats = items.iter().map(|(m, b)| Format { mime: m.to_string(), hash: c.upload_blob(b).unwrap() }).collect();
    call(c, op::SET, &proto::Set { token, formats, sensitive })
}

fn paste(c: &mut Connection, token: Token, accept: &[&str]) -> (Reply, Option<Vec<u8>>) {
    let r = call(c, op::GET, &proto::Get { token, accept: accept.iter().map(|s| s.to_string()).collect() });
    let bytes = match &r {
        Reply::Got { format } => c.cache_get(&format.hash),
        _ => None,
    };
    (r, bytes)
}

fn setup() -> (Fake, std::sync::Arc<Shared>) {
    let fake = Fake::default();
    let shared = Shared::new(Box::new(fake.clone()));
    (fake, shared)
}

#[test]
fn copy_in_one_app_paste_in_another() {
    let (f, s) = setup();
    let mut ed = app(&s, EDITOR);
    let mut vw = app(&s, VIEWER);
    let r = copy(&mut ed, f.mint(EDITOR, kind::KEY), &[("text/plain", b"hello"), ("text/html", b"<b>hello</b>")], false);
    assert_eq!(r, Reply::Ok);
    // Format names need no gesture.
    match call(&mut vw, op::OFFERS, &()) {
        Reply::Offers { formats, .. } => assert_eq!(formats, vec!["text/plain", "text/html"]),
        other => panic!("{other:?}"),
    }
    let (r, bytes) = paste(&mut vw, f.mint(VIEWER, kind::BUTTON), &["text/html", "text/plain"]);
    assert!(matches!(r, Reply::Got { ref format } if format.mime == "text/html"), "{r:?}");
    assert_eq!(bytes.as_deref(), Some(&b"<b>hello</b>"[..]));
}

#[test]
fn no_gesture_no_paste() {
    let (f, s) = setup();
    let mut ed = app(&s, EDITOR);
    let mut vw = app(&s, VIEWER);
    assert_eq!(copy(&mut ed, f.mint(EDITOR, kind::KEY), &[("text/plain", b"secret plan")], false), Reply::Ok);

    // A made-up token.
    let (r, b) = paste(&mut vw, [7; 16], &["text/plain"]);
    assert_eq!(r, Reply::Refused { reason: refusal::NO_GESTURE, detail: fr::UNKNOWN });
    assert!(b.is_none());
    // A token minted for the other app (leaked or stolen).
    let (r, _) = paste(&mut vw, f.mint(EDITOR, kind::BUTTON), &["text/plain"]);
    assert_eq!(r, Reply::Refused { reason: refusal::NO_GESTURE, detail: fr::WRONG_UID });
    // Spent twice.
    let t = f.mint(VIEWER, kind::BUTTON);
    assert!(matches!(paste(&mut vw, t, &["text/plain"]).0, Reply::Got { .. }));
    assert_eq!(paste(&mut vw, t, &["text/plain"]).0, Reply::Refused { reason: refusal::NO_GESTURE, detail: fr::UNKNOWN });
    // Too old.
    let t = f.mint(VIEWER, kind::BUTTON);
    f.expire(&t);
    assert_eq!(paste(&mut vw, t, &["text/plain"]).0, Reply::Refused { reason: refusal::NO_GESTURE, detail: fr::EXPIRED });
    // A drop token is not a paste.
    assert_eq!(paste(&mut vw, f.mint(VIEWER, kind::DROP), &["text/plain"]).0,
        Reply::Refused { reason: refusal::NO_GESTURE, detail: fr::WRONG_KIND });
}

#[test]
fn no_gesture_no_overwrite() {
    let (f, s) = setup();
    let mut ed = app(&s, EDITOR);
    let mut vw = app(&s, VIEWER);
    assert_eq!(copy(&mut ed, f.mint(EDITOR, kind::KEY), &[("text/plain", b"mine")], false), Reply::Ok);
    // The other app tries to replace or wipe it in the background.
    assert!(matches!(copy(&mut vw, [1; 16], &[("text/plain", b"evil")], false), Reply::Refused { reason: refusal::NO_GESTURE, .. }));
    assert!(matches!(call(&mut vw, op::CLEAR, &proto::Clear { token: [2; 16] }), Reply::Refused { reason: refusal::NO_GESTURE, .. }));
    let (_, b) = paste(&mut ed, f.mint(EDITOR, kind::BUTTON), &["text/plain"]);
    assert_eq!(b.as_deref(), Some(&b"mine"[..]));
}

#[test]
fn content_must_be_uploaded_not_named() {
    let (f, s) = setup();
    let mut vw = app(&s, VIEWER);
    // Naming a hash this connection never sent (probing whether some
    // content exists on the machine) is refused, gesture or not.
    let formats = vec![Format { mime: "text/plain".into(), hash: aqueduct::cas::hash(b"someone else's file") }];
    let r = call(&mut vw, op::SET, &proto::Set { token: f.mint(VIEWER, kind::KEY), formats, sensitive: false });
    assert_eq!(r, Reply::Refused { reason: refusal::NOT_UPLOADED, detail: 0 });
}

#[test]
fn bad_entries_are_refused_before_a_gesture_is_spent() {
    let (f, s) = setup();
    let mut ed = app(&s, EDITOR);
    let t = f.mint(EDITOR, kind::KEY);
    assert_eq!(copy(&mut ed, t, &[("Text/Plain; x=1", b"a")], false), Reply::Refused { reason: refusal::BAD_MIME, detail: 0 });
    // The token is still good.
    assert_eq!(copy(&mut ed, t, &[("text/plain", b"a")], false), Reply::Ok);
    assert!(matches!(copy(&mut ed, f.mint(EDITOR, kind::KEY), &[("image/png", b"x")], true),
        Reply::Refused { reason: refusal::NO_FORMATS, .. }), "a secret must be text");
}

#[test]
fn other_apps_hear_of_a_change() {
    let (f, s) = setup();
    let mut ed = app(&s, EDITOR);
    let mut vw = app(&s, VIEWER);
    assert!(matches!(call(&mut vw, op::OFFERS, &()), Reply::Offers { .. }));
    assert_eq!(copy(&mut ed, f.mint(EDITOR, kind::KEY), &[("text/plain", b"new")], false), Reply::Ok);
    let m = vw.recv_message_or_timeout(Duration::from_secs(2)).unwrap().expect("change event");
    assert_eq!(m.op, op::EV_CHANGED);
    assert!(m.flags & flag::ASYNC_EVENT != 0);
}

#[test]
fn the_audit_log_has_uids_not_contents() {
    let (f, s) = setup();
    let mut ed = app(&s, EDITOR);
    let mut vw = app(&s, VIEWER);
    copy(&mut ed, f.mint(EDITOR, kind::KEY), &[("text/plain", b"x")], false);
    paste(&mut vw, [9; 16], &["text/plain"]);
    let log: Vec<_> = s.store.lock().unwrap().audit().map(|a| (a.uid, a.op, a.outcome)).collect();
    assert_eq!(log, vec![(EDITOR, op::SET, 0), (VIEWER, op::GET, refusal::NO_GESTURE)]);
}
