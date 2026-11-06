//! The credential field's side of arcad (arca-autofill.md): accounts for
//! the chip, handles for a pick, a typed value or a generated one, and
//! that a handle reaches only its recipient, once.

use arca_store::codec::{Reader, Writer};
use arca_store::Recipient;
use arcad::label::Label;
use arcad::proto::{op, status};
use arcad::server::State;

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn parent(w: &mut Writer, publisher: u8, app: &str) {
    w.str("alice");
    w.raw(&[publisher; 32]);
    w.str(app);
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    (d, s)
}

fn mail() -> Label {
    Label::App { user: "alice".into(), publisher: [7; 32], app_id: "org.example.mail".into() }
}

fn rcp(p: u8, app: &str) -> Recipient {
    Recipient::app([p; 32], app).unwrap()
}

#[test]
fn chip_lists_only_this_parents_passwords() {
    let (_d, mut s) = setup();
    let r = s.handle(&mail(), op::STORE, &body(|w| { w.u16(1); w.u8(2); w.str("ana@example.com"); w.bytes(b"pw"); }));
    assert_eq!(r.status, status::OK);
    let id: [u8; 16] = r.body.bytes().try_into().unwrap();
    let r = s.handle(&Label::TrustedUi, op::U_ACCOUNTS, &body(|w| parent(w, 7, "org.example.mail")));
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.u32().unwrap(), 1);
    assert_eq!(rd.array::<16>().unwrap(), id);
    assert_eq!(rd.str(64).unwrap(), "ana@example.com");
    // Same app id, another publisher: nothing on the chip.
    let r = s.handle(&Label::TrustedUi, op::U_ACCOUNTS, &body(|w| parent(w, 9, "org.example.mail")));
    assert_eq!(r.body.bytes(), &[0, 0, 0, 0]);
    // Only Arca's UI may ask.
    assert_eq!(s.handle(&mail(), op::U_ACCOUNTS, &body(|w| parent(w, 7, "org.example.mail"))).status, status::DENIED);
}

#[test]
fn a_handle_reaches_only_its_recipient_once() {
    let (_d, mut s) = setup();
    let r = s.handle(&mail(), op::STORE, &body(|w| { w.u16(1); w.u8(2); w.str("ana"); w.bytes(b"stored-pw"); }));
    let id: [u8; 16] = r.body.bytes().try_into().unwrap();
    let h: [u8; 16] = s.handle(&Label::TrustedUi, op::U_HANDLE, &body(|w| { parent(w, 7, "org.example.mail"); w.raw(&id); }))
        .body.bytes().try_into().unwrap();
    // Another recipient burns it.
    assert!(s.redeem(&h, "alice", &rcp(9, "org.example.mail")).is_none());
    assert!(s.redeem(&h, "alice", &rcp(7, "org.example.mail")).is_none());
    let h: [u8; 16] = s.handle(&Label::TrustedUi, op::U_HANDLE, &body(|w| { parent(w, 7, "org.example.mail"); w.raw(&id); }))
        .body.bytes().try_into().unwrap();
    assert_eq!(s.redeem(&h, "alice", &rcp(7, "org.example.mail")).unwrap().as_ref(), b"stored-pw");
    assert!(s.redeem(&h, "alice", &rcp(7, "org.example.mail")).is_none(), "one submit only");
    // A handle for another parent's item: refused.
    let st = s.handle(&Label::TrustedUi, op::U_HANDLE, &body(|w| { parent(w, 7, "org.example.evil"); w.raw(&id); })).status;
    assert_eq!(st, status::DENIED);
}

#[test]
fn typed_and_generated_values_are_held_not_returned() {
    let (_d, mut s) = setup();
    let r = s.handle(&Label::TrustedUi, op::U_HOLD, &body(|w| { parent(w, 7, "org.example.mail"); w.bytes(b"typed-pw"); }));
    assert_eq!(r.status, status::OK);
    assert_eq!(r.body.bytes().len(), 16, "a handle, not the value");
    let h: [u8; 16] = r.body.bytes().try_into().unwrap();
    assert_eq!(s.redeem(&h, "alice", &rcp(7, "org.example.mail")).unwrap().as_ref(), b"typed-pw");

    let r = s.handle(&Label::TrustedUi, op::U_GENERATE, &body(|w| parent(w, 7, "org.example.mail")));
    let h: [u8; 16] = r.body.bytes().try_into().unwrap();
    let v = s.redeem(&h, "alice", &rcp(7, "org.example.mail")).unwrap();
    let g = std::str::from_utf8(v.as_ref()).unwrap().to_string();
    assert_eq!(g.len(), 23);
    assert_eq!(g.split('-').count(), 4);
    assert!(g.split('-').all(|p| p.len() == 5 && p.bytes().all(|c| c.is_ascii_alphanumeric())));
    // Two generations differ.
    let h2: [u8; 16] = s.handle(&Label::TrustedUi, op::U_GENERATE, &body(|w| parent(w, 7, "org.example.mail"))).body.bytes().try_into().unwrap();
    assert_ne!(s.redeem(&h2, "alice", &rcp(7, "org.example.mail")).unwrap().as_ref(), g.as_bytes());
}

#[test]
fn lock_voids_handles_and_refuses_new_ones() {
    let (_d, mut s) = setup();
    let h: [u8; 16] = s.handle(&Label::TrustedUi, op::U_HOLD, &body(|w| { parent(w, 7, "org.example.mail"); w.bytes(b"typed"); }))
        .body.bytes().try_into().unwrap();
    s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice")));
    assert!(s.redeem(&h, "alice", &rcp(7, "org.example.mail")).is_none());
    let st = s.handle(&Label::TrustedUi, op::U_HOLD, &body(|w| { parent(w, 7, "org.example.mail"); w.bytes(b"typed"); })).status;
    assert_eq!(st, status::LOCKED);
    assert_eq!(s.handle(&Label::TrustedUi, op::U_GENERATE, &body(|w| parent(w, 7, "org.example.mail"))).status, status::LOCKED);
}

#[test]
fn dropped_handles_are_gone() {
    let (_d, mut s) = setup();
    let h: [u8; 16] = s.handle(&Label::TrustedUi, op::U_HOLD, &body(|w| { parent(w, 7, "org.example.mail"); w.bytes(b"typed"); }))
        .body.bytes().try_into().unwrap();
    assert_eq!(s.handle(&Label::TrustedUi, op::U_DROP, &h).status, status::OK);
    assert!(s.redeem(&h, "alice", &rcp(7, "org.example.mail")).is_none());
}
