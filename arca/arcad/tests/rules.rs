//! arca.md §11 A0 gate "a test app gets EACCES on another app's item",
//! and the rest of arcad's rules, against the request handler.

use arca_store::codec::{Reader, Writer};
use arcad::label::{Label, Resolver};
use arcad::proto::{op, status};
use arcad::server::State;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

fn app(id: &str, publisher: u8) -> Label {
    Label::App { user: "alice".into(), publisher: [publisher; 32], app_id: id.into() }
}

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn root(b: u8) -> Vec<u8> {
    vec![b; 32]
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    let r = s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&root(1)); }));
    assert_eq!(r.status, status::OK);
    (d, s)
}

fn store(s: &mut State, who: &Label, value: &[u8]) -> [u8; 16] {
    let r = s.handle(who, op::STORE, &body(|w| { w.u16(1); w.u8(2); w.str("db"); w.bytes(value); }));
    assert_eq!(r.status, status::OK);
    r.body.bytes().try_into().unwrap()
}

fn grant(s: &mut State, publisher: u8, app_id: &str, id: &[u8; 16]) -> u8 {
    s.handle(&Label::TrustedUi, op::U_GRANT, &body(|w| { w.str("alice"); w.raw(&[publisher; 32]); w.str(app_id); w.raw(id); })).status
}

#[test]
fn another_apps_item_is_eacces() {
    let (_d, mut s) = setup();
    let mail = app("org.example.mail", 7);
    let id = store(&mut s, &mail, b"dbpass");
    assert_eq!(grant(&mut s, 7, "org.example.mail", &id), status::OK);
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::OK);

    for other in [
        app("org.example.evil", 7),   // same publisher, another app
        app("org.example.mail", 9),   // the same app id, another publisher
        Label::App { user: "bob".into(), publisher: [7; 32], app_id: "org.example.mail".into() },
    ] {
        for o in [op::RELEASE, op::DELETE, op::PUBKEY, op::SIGN] {
            let b = if o == op::SIGN { body(|w| { w.raw(&id); w.bytes(b"x"); }) } else { id.to_vec() };
            let st = s.handle(&other, o, &b).status;
            assert!(st == status::DENIED || st == status::LOCKED, "{other:?} op {o:#x} got {st}");
        }
        let l = s.handle(&other, op::LIST, &[]);
        if l.status == status::OK {
            assert_eq!(&l.body.bytes()[..4], &[0, 0, 0, 0], "{other:?} listed foreign items");
        }
    }
    // A missing item looks exactly like a foreign one.
    assert_eq!(s.handle(&mail, op::RELEASE, &[0x55; 16]).status, status::DENIED);
}

#[test]
fn app_held_needs_a_user_grant_and_only_the_ui_can_grant() {
    let (_d, mut s) = setup();
    let mail = app("org.example.mail", 7);
    let id = store(&mut s, &mail, b"dbpass");
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::GRANT_REQUIRED);
    // The app can't grant itself, nor can Vestibulum.
    let g = body(|w| { w.str("alice"); w.raw(&[7; 32]); w.str("org.example.mail"); w.raw(&id); });
    assert_eq!(s.handle(&mail, op::U_GRANT, &g).status, status::DENIED);
    assert_eq!(s.handle(&Label::Vestibulum, op::U_GRANT, &g).status, status::DENIED);
    // A grant names the app exactly.
    assert_eq!(grant(&mut s, 9, "org.example.mail", &id), status::DENIED);
    assert_eq!(grant(&mut s, 7, "org.example.mail", &id), status::OK);
    let r = s.handle(&mail, op::RELEASE, &id);
    assert_eq!(r.status, status::OK);
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.bytes(64).unwrap(), b"dbpass");
    // Revoked: back to refusing.
    assert_eq!(s.handle(&Label::TrustedUi, op::U_REVOKE, &g).status, status::OK);
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::GRANT_REQUIRED);
}

#[test]
fn keys_are_used_never_read() {
    let (_d, mut s) = setup();
    let a = app("org.example.chat", 7);
    let r = s.handle(&a, op::MINT, &body(|w| { w.u16(0x100); w.u8(2); w.str("chat.example"); }));
    assert_eq!(r.status, status::OK);
    let (id, pk): ([u8; 16], [u8; 32]) = (r.body.bytes()[..16].try_into().unwrap(), r.body.bytes()[16..].try_into().unwrap());
    let r = s.handle(&a, op::SIGN, &body(|w| { w.raw(&id); w.bytes(b"challenge"); }));
    assert_eq!(r.status, status::OK);
    let sig = Signature::from_bytes(r.body.bytes().try_into().unwrap());
    VerifyingKey::from_bytes(&pk).unwrap().verify(b"challenge", &sig).unwrap();
    assert_eq!(s.handle(&a, op::PUBKEY, &id).body.bytes(), &pk);
    // Never released, not even with a grant.
    assert_eq!(s.handle(&a, op::RELEASE, &id).status, status::NOT_RELEASABLE);
    assert_eq!(grant(&mut s, 7, "org.example.chat", &id), status::NOT_RELEASABLE);
    // An app can't import a "key" either.
    let st = s.handle(&a, op::STORE, &body(|w| { w.u16(0x100); w.u8(2); w.str(""); w.bytes(&[1; 32]); })).status;
    assert_eq!(st, status::UNSUPPORTED);
    // Another app can't use it.
    let st = s.handle(&app("org.example.evil", 7), op::SIGN, &body(|w| { w.raw(&id); w.bytes(b"x"); })).status;
    assert_eq!(st, status::DENIED);
}

#[test]
fn lock_and_unlock_through_vestibulum_only() {
    let (d, mut s) = setup();
    let mail = app("org.example.mail", 7);
    let id = store(&mut s, &mail, b"pw");
    grant(&mut s, 7, "org.example.mail", &id);
    let user = body(|w| w.str("alice"));
    // Apps can't lock, unlock or create.
    for o in [op::V_LOCK, op::V_STATE, op::V_UNLOCK, op::V_CREATE] {
        assert_eq!(s.handle(&mail, o, &user).status, status::DENIED);
    }
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &user).status, status::OK);
    assert_eq!(s.handle(&Label::Vestibulum, op::V_STATE, &user).body.bytes(), &[2]);
    // While-unlocked: gone after lock.
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::DENIED);
    let wrong = body(|w| { w.str("alice"); w.raw(&root(2)); });
    assert_eq!(s.handle(&Label::Vestibulum, op::V_UNLOCK, &wrong).status, status::WRONG_ROOT);
    let right = body(|w| { w.str("alice"); w.raw(&root(1)); });
    assert_eq!(s.handle(&Label::Vestibulum, op::V_UNLOCK, &right).status, status::OK);
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::OK);

    // A restarted daemon starts sealed for every user until Vestibulum unlocks.
    drop(s);
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::LOCKED);
    assert_eq!(s.handle(&Label::Vestibulum, op::V_UNLOCK, &right).status, status::OK);
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::OK);
    // No store for a user who never logged in.
    let bob = body(|w| { w.str("bob"); w.raw(&root(1)); });
    assert_eq!(s.handle(&Label::Vestibulum, op::V_UNLOCK, &bob).status, status::NO_STORE);
}

#[test]
fn unknown_peers_get_nothing() {
    let (_d, mut s) = setup();
    for o in [op::MINT, op::STORE, op::LIST, op::V_UNLOCK, op::U_GRANT] {
        assert_eq!(s.handle(&Label::Unknown, o, &[]).status, status::UNKNOWN_PEER);
    }
}

#[test]
fn malformed_bodies_are_refused_cleanly() {
    let (_d, mut s) = setup();
    let a = app("org.example.mail", 7);
    let good = body(|w| { w.u16(1); w.u8(2); w.str("db"); w.bytes(b"v"); });
    for n in 0..good.len() {
        assert_eq!(s.handle(&a, op::STORE, &good[..n]).status, status::MALFORMED, "len {n}");
    }
    let mut long = good.clone();
    long.push(0);
    assert_eq!(s.handle(&a, op::STORE, &long).status, status::MALFORMED);
    // Verify-each-use isn't offered to apps until Vestibulum's check is wired.
    let veu = body(|w| { w.u16(1); w.u8(3); w.str("db"); w.bytes(b"v"); });
    assert_eq!(s.handle(&a, op::STORE, &veu).status, status::UNSUPPORTED);
    // A huge length prefix allocates nothing and fails.
    let huge = body(|w| { w.u16(1); w.u8(2); w.u32(u32::MAX); });
    assert_eq!(s.handle(&a, op::STORE, &huge).status, status::MALFORMED);
}

#[test]
fn audit_log_has_no_values_or_accounts() {
    let (d, mut s) = setup();
    let mail = app("org.example.mail", 7);
    let r = s.handle(&mail, op::STORE, &body(|w| { w.u16(1); w.u8(2); w.str("alice@example.com"); w.bytes(b"SECRET-VALUE"); }));
    let id: [u8; 16] = r.body.bytes().try_into().unwrap();
    grant(&mut s, 7, "org.example.mail", &id);
    s.handle(&mail, op::RELEASE, &id);
    drop(s);
    let log = std::fs::read_to_string(d.path().join("audit.log")).unwrap();
    assert!(log.contains("app:org.example.mail/"));
    assert!(log.lines().count() >= 4);
    assert!(!log.contains("SECRET-VALUE") && !log.contains("alice@example.com"));
}

#[test]
fn resolver_fails_closed() {
    let d = tempfile::tempdir().unwrap();
    let reg = d.path().join("registry");
    std::fs::write(&reg, format!("50001 alice org.example.mail {}\n50002 alice org.example.unsigned\n", "ab".repeat(32))).unwrap();
    let mut r = Resolver { registry: reg.clone(), vestibulum_uid: Some(65020), ui_uid: None, allow_unsigned: false, courier_registry: None };
    assert_eq!(r.resolve(50001), app("org.example.mail", 0xab));
    assert_eq!(r.resolve(50002), Label::Unknown);
    assert_eq!(r.resolve(1001), Label::Unknown);
    assert_eq!(r.resolve(65020), Label::Vestibulum);
    r.allow_unsigned = true;
    assert_eq!(r.resolve(50002), app("org.example.unsigned", 0));
    // A system uid that also appears as an app in the registry is neither.
    std::fs::write(&reg, "65020 alice org.example.squatter\n").unwrap();
    r.registry = reg.clone();
    assert_eq!(r.resolve(65020), Label::Unknown);
    r.registry = d.path().join("missing");
    assert_eq!(r.resolve(50001), Label::Unknown);
}

/// A hostile app throwing random ops and bodies (seeded, so a failure
/// reproduces). Positive control: its own item still works afterwards, and
/// another app's item is never released.
#[test]
fn random_requests_from_a_hostile_app() {
    let (_d, mut s) = setup();
    let victim = app("org.example.bank", 7);
    let vid = store(&mut s, &victim, b"victim-secret");
    grant(&mut s, 7, "org.example.bank", &vid);
    let evil = app("org.example.evil", 7);
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..50_000 {
        let o = [op::MINT, op::PUBKEY, op::SIGN, op::STORE, op::RELEASE, op::LIST, op::DELETE, op::U_GRANT, op::V_LOCK, 0xffff][(next() % 10) as usize];
        let n = (next() % 64) as usize;
        let mut b: Vec<u8> = (0..n).map(|_| next() as u8).collect();
        // Sometimes aim at the victim's id.
        if next() % 4 == 0 && b.len() >= 16 {
            b[..16].copy_from_slice(&vid);
        }
        let r = s.handle(&evil, o, &b);
        assert!(!r.body.bytes().windows(13).any(|w| w == b"victim-secret"));
    }
    let r = s.handle(&victim, op::RELEASE, &vid);
    assert_eq!(r.status, status::OK);
}
