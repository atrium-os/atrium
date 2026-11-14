//! Network shares' side of arcad (network-shares.md §5): the file manager
//! saves a typed password or mints a key for one share; only that share's
//! server, after proving the pinned identity, gets the password or a
//! signature, and nothing else does.

use arca_store::codec::{Reader, Writer};
use arca_store::{Recipient, ShareProto};
use arcad::label::Label;
use arcad::proto::{op, share_op, status};
use arcad::server::State;
use arcad::share::{write_share, ShareLabel};

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

const KEY: [u8; 32] = [0x5a; 32];

fn nas(path: &str, key: [u8; 32]) -> Recipient {
    Recipient::share(ShareProto::Sftp, "nas.lan", 22, path, key).unwrap()
}

fn files() -> Label {
    Label::FileManager { user: "alice".into(), publisher: [4; 32], app_id: "org.atrium.scrinium".into() }
}

fn server(share: Recipient) -> Label {
    Label::Share(ShareLabel { user: "alice".into(), share })
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    (d, s)
}

/// What the user typed in `parent`'s credential field: a handle.
fn typed(s: &mut State, publisher: u8, app: &str, pw: &[u8]) -> [u8; 16] {
    let r = s.handle(&Label::TrustedUi, op::U_HOLD, &body(|w| { w.str("alice"); w.raw(&[publisher; 32]); w.str(app); w.bytes(pw); }));
    assert_eq!(r.status, status::OK);
    r.body.bytes().try_into().unwrap()
}

fn save(s: &mut State, who: &Label, h: [u8; 16], share: &Recipient, account: &str) -> (u8, Option<[u8; 16]>) {
    let r = s.handle(who, share_op::SH_SAVE, &body(|w| { w.raw(&h); write_share(w, share); w.str(account); }));
    (r.status, r.body.bytes().try_into().ok())
}

fn items(s: &mut State, who: &Label, key: [u8; 32]) -> (u8, Vec<([u8; 16], u16, String)>) {
    let r = s.handle(who, share_op::SH_ITEMS, &key);
    if r.status != status::OK {
        return (r.status, vec![]);
    }
    let mut rd = Reader::new(r.body.bytes());
    let n = rd.u32().unwrap();
    let v = (0..n).map(|_| (rd.array::<16>().unwrap(), rd.u16().unwrap(), rd.str(256).unwrap().to_string())).collect();
    (r.status, v)
}

fn fill(s: &mut State, who: &Label, key: [u8; 32], id: [u8; 16]) -> (u8, Vec<u8>) {
    let r = s.handle(who, share_op::SH_FILL, &body(|w| { w.raw(&key); w.raw(&id); }));
    let b = r.body.bytes();
    (r.status, if b.len() >= 4 { b[4..].to_vec() } else { vec![] })
}

#[test]
fn a_typed_password_reaches_only_its_share_after_the_pin() {
    let (_d, mut s) = setup();
    let share = nas("/home/alice", KEY);
    let h = typed(&mut s, 4, "org.atrium.scrinium", b"hunter2-nas");
    let (st, id) = save(&mut s, &files(), h, &share, "alice");
    assert_eq!(st, status::OK);
    let id = id.unwrap();

    let me = server(share.clone());
    let (st, list) = items(&mut s, &me, KEY);
    assert_eq!(st, status::OK);
    assert_eq!(list, vec![(id, arca_store::Kind::Password as u16, "alice".to_string())]);
    assert_eq!(fill(&mut s, &me, KEY, id), (status::OK, b"hunter2-nas".to_vec()));

    // A server that showed another key gets nothing, not even the list.
    assert_eq!(items(&mut s, &me, [0x5b; 32]).0, status::DENIED);
    assert_eq!(fill(&mut s, &me, [0x5b; 32], id).0, status::DENIED);
    // Another share's server (another path, or the same path with another
    // pinned key) can't reach it by id.
    let other = server(nas("/srv", KEY));
    assert_eq!(fill(&mut s, &other, KEY, id).0, status::DENIED);
    let rekeyed = server(nas("/home/alice", [0x5b; 32]));
    assert_eq!(fill(&mut s, &rekeyed, [0x5b; 32], id).0, status::DENIED);
    assert!(items(&mut s, &rekeyed, [0x5b; 32]).1.is_empty());
    // Apps can't fill, and the file manager itself never gets the value.
    assert_eq!(fill(&mut s, &files(), KEY, id).0, status::DENIED);
    let st = s.handle(&files(), op::RELEASE, &id).status;
    assert_ne!(st, status::OK);
}

#[test]
fn only_the_file_manager_saves_and_only_from_its_own_field() {
    let (_d, mut s) = setup();
    let share = nas("/home/alice", KEY);
    // Another app has the user type a password, then tries to bind it to a
    // server of its choosing.
    let evil = Label::App { user: "alice".into(), publisher: [9; 32], app_id: "org.example.evil".into() };
    let h = typed(&mut s, 9, "org.example.evil", b"pw");
    assert_eq!(save(&mut s, &evil, h, &share, "alice").0, status::DENIED);
    // ... or hands its handle to the file manager.
    let h = typed(&mut s, 9, "org.example.evil", b"pw");
    assert_eq!(save(&mut s, &files(), h, &share, "alice").0, status::DENIED);
    // A handle is redeemed once.
    let h = typed(&mut s, 4, "org.atrium.scrinium", b"pw");
    assert_eq!(save(&mut s, &files(), h, &share, "alice").0, status::OK);
    assert_eq!(save(&mut s, &files(), h, &share, "alice").0, status::DENIED);
    // A share server can't save.
    let h = typed(&mut s, 4, "org.atrium.scrinium", b"pw");
    assert_eq!(save(&mut s, &server(share.clone()), h, &share, "alice").0, status::DENIED);
}

#[test]
fn saving_again_replaces_and_the_list_and_delete_work() {
    let (_d, mut s) = setup();
    let share = nas("/home/alice", KEY);
    let h = typed(&mut s, 4, "org.atrium.scrinium", b"old");
    let (_, first) = save(&mut s, &files(), h, &share, "alice");
    let h = typed(&mut s, 4, "org.atrium.scrinium", b"new");
    let (_, second) = save(&mut s, &files(), h, &share, "alice");
    let me = server(share.clone());
    let (_, list) = items(&mut s, &me, KEY);
    assert_eq!(list.len(), 1, "one password per share and account");
    assert_eq!(fill(&mut s, &me, KEY, second.unwrap()), (status::OK, b"new".to_vec()));
    assert_eq!(fill(&mut s, &me, KEY, first.unwrap()).0, status::DENIED);

    let r = s.handle(&files(), share_op::SH_LIST, &[]);
    assert_eq!(r.status, status::OK);
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.u32().unwrap(), 1);
    assert_eq!(arcad::share::read_share(&mut rd).unwrap(), share);
    assert_eq!(rd.array::<16>().unwrap(), second.unwrap());

    let st = s.handle(&files(), share_op::SH_DELETE, &body(|w| { write_share(w, &share); w.raw(&second.unwrap()); })).status;
    assert_eq!(st, status::OK);
    assert!(items(&mut s, &me, KEY).1.is_empty());
}

#[test]
fn a_shares_key_signs_for_its_server_and_never_leaves() {
    let (_d, mut s) = setup();
    let share = nas("/home/alice", KEY);
    let r = s.handle(&files(), share_op::SH_MINT, &body(|w| { write_share(w, &share); w.str("alice"); }));
    assert_eq!(r.status, status::OK);
    let id: [u8; 16] = r.body.bytes()[..16].try_into().unwrap();
    let pk: [u8; 32] = r.body.bytes()[16..].try_into().unwrap();

    let me = server(share.clone());
    let r = s.handle(&me, share_op::SH_PUBKEY, &body(|w| { w.raw(&KEY); w.raw(&id); }));
    assert_eq!(r.body.bytes(), &pk);
    let r = s.handle(&me, share_op::SH_SIGN, &body(|w| { w.raw(&KEY); w.raw(&id); w.bytes(b"session data"); }));
    assert_eq!(r.status, status::OK);
    let sig = ed25519_dalek::Signature::from_bytes(r.body.bytes().try_into().unwrap());
    ed25519_dalek::VerifyingKey::from_bytes(&pk).unwrap().verify_strict(b"session data", &sig).unwrap();
    // The seed is never filled, and another identity can't sign.
    assert_eq!(fill(&mut s, &me, KEY, id).0, status::DENIED);
    let st = s.handle(&me, share_op::SH_SIGN, &body(|w| { w.raw(&[0; 32]); w.raw(&id); w.bytes(b"x"); })).status;
    assert_eq!(st, status::DENIED);
}

#[test]
fn a_lock_stops_the_share_server() {
    let (_d, mut s) = setup();
    let share = nas("/home/alice", KEY);
    let h = typed(&mut s, 4, "org.atrium.scrinium", b"pw");
    let id = save(&mut s, &files(), h, &share, "alice").1.unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    let me = server(share);
    assert_ne!(fill(&mut s, &me, KEY, id).0, status::OK);
}
