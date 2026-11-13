//! arcad's side of the courier (arca.md §5, A1): an app's endpoint items
//! reach only that app's courier to that endpoint, never the app itself,
//! another app's courier, or a courier to another endpoint.

use arca_store::codec::{Reader, Writer};
use arca_store::{Recipient, Scheme};
use arcad::courier::{app_persona, CourierLabel};
use arcad::label::{Label, Resolver};
use arcad::proto::{courier_op as cop, op, status, write_endpoint};
use arcad::server::State;

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    (d, s)
}

fn app(p: u8, id: &str) -> Label {
    Label::App { user: "alice".into(), publisher: [p; 32], app_id: id.into() }
}

fn courier(p: u8, id: &str, host: &str) -> Label {
    Label::Courier(CourierLabel {
        user: "alice".into(),
        persona: app_persona(&[p; 32], id),
        endpoint: Recipient::endpoint(Scheme::Https, host, 443, None).unwrap(),
    })
}

fn store_for(s: &mut State, who: &Label, host: &str, value: &[u8]) -> [u8; 16] {
    let r = s.handle(who, op::STORE_FOR, &body(|w| {
        w.u16(4); // ApiKey
        w.u8(2);
        w.str("token");
        write_endpoint(w, host, 443);
        w.bytes(value);
    }));
    assert_eq!(r.status, status::OK);
    r.body.bytes().try_into().unwrap()
}

fn fill(s: &mut State, who: &Label, refs: &[(u8, [u8; 16])]) -> (u8, Vec<Vec<u8>>) {
    let r = s.handle(who, cop::FILL, &body(|w| {
        w.u8(refs.len() as u8);
        for (src, x) in refs {
            w.u8(*src);
            w.raw(x);
        }
    }));
    let mut out = Vec::new();
    let mut rd = Reader::new(r.body.bytes());
    while rd.remaining() > 0 {
        out.push(rd.bytes(1 << 20).unwrap().to_vec());
    }
    (r.status, out)
}

#[test]
fn only_this_apps_courier_to_this_endpoint_gets_the_token() {
    let (_d, mut s) = setup();
    let mail = app(7, "org.example.mail");
    let id = store_for(&mut s, &mail, "api.example", b"tok-123");

    let (st, v) = fill(&mut s, &courier(7, "org.example.mail", "api.example"), &[(cop::SOURCE_ITEM, id)]);
    assert_eq!((st, v), (status::OK, vec![b"tok-123".to_vec()]));
    // Another endpoint (an open redirect's target, say): nothing.
    assert_eq!(fill(&mut s, &courier(7, "org.example.mail", "evil.example"), &[(cop::SOURCE_ITEM, id)]).0, status::DENIED);
    // Same endpoint, another app's courier, or same app id from another publisher.
    assert_eq!(fill(&mut s, &courier(7, "org.example.chat", "api.example"), &[(cop::SOURCE_ITEM, id)]).0, status::DENIED);
    assert_eq!(fill(&mut s, &courier(9, "org.example.mail", "api.example"), &[(cop::SOURCE_ITEM, id)]).0, status::DENIED);
    // The app itself can't read it back, even with a grant op it can't send.
    assert_eq!(s.handle(&mail, op::RELEASE, &id).status, status::DENIED);
    // It can list it, for that endpoint only.
    let r = s.handle(&mail, op::LIST_FOR, &body(|w| write_endpoint(w, "api.example", 443)));
    assert_eq!(&r.body.bytes()[..4], &[1, 0, 0, 0]);
    let r = s.handle(&mail, op::LIST_FOR, &body(|w| write_endpoint(w, "evil.example", 443)));
    assert_eq!(r.body.bytes(), &[0, 0, 0, 0]);
}

#[test]
fn a_fill_is_all_or_nothing_and_couriers_get_no_other_ops() {
    let (_d, mut s) = setup();
    let mail = app(7, "org.example.mail");
    let a = store_for(&mut s, &mail, "api.example", b"one");
    let b = store_for(&mut s, &mail, "api.example", b"two");
    let c = courier(7, "org.example.mail", "api.example");
    let (st, v) = fill(&mut s, &c, &[(cop::SOURCE_ITEM, a), (cop::SOURCE_ITEM, b)]);
    assert_eq!((st, v.len()), (status::OK, 2));
    let (st, v) = fill(&mut s, &c, &[(cop::SOURCE_ITEM, a), (cop::SOURCE_ITEM, [0xee; 16])]);
    assert_eq!((st, v.len()), (status::DENIED, 0));
    assert_eq!(fill(&mut s, &c, &[]).0, status::MALFORMED);
    assert_eq!(fill(&mut s, &c, &[(cop::SOURCE_ITEM, a); 9]).0, status::MALFORMED);
    assert_eq!(fill(&mut s, &c, &[(3, a)]).0, status::MALFORMED);
    for o in [op::STORE, op::RELEASE, op::LIST, op::STORE_FOR, op::V_UNLOCK, op::U_ACCOUNTS, op::U_HOLD] {
        assert_eq!(s.handle(&c, o, &a).status, status::DENIED, "op {o:#x}");
    }
    // And apps get no courier ops.
    assert_eq!(fill(&mut s, &mail, &[(cop::SOURCE_ITEM, a)]).0, status::DENIED);
}

#[test]
fn set_cookie_records_replace_and_never_fill_by_id() {
    let (_d, mut s) = setup();
    let c = courier(7, "org.example.mail", "api.example");
    let set = |s: &mut State, v: &[u8]| s.handle(&c, cop::SET_COOKIE, &body(|w| { w.str("sid"); w.bytes(v); w.bytes(b"Path=/; HttpOnly"); w.str("/login"); })).status;
    assert_eq!(set(&mut s, b"v1"), status::OK);
    assert_eq!(set(&mut s, b"v2"), status::OK);
    assert_eq!(s.handle(&c, cop::SET_COOKIE, &body(|w| { w.str("bad name"); w.bytes(b"x"); w.bytes(b""); w.str("/"); })).status, status::DENIED, "ignored");
    assert_eq!(s.handle(&c, cop::SET_COOKIE, &body(|w| { w.str("sid"); w.bytes(b"x"); w.bytes(b""); })).status, status::MALFORMED, "no path");
    // The app lists one cookie record for that endpoint, the latest.
    let r = s.handle(&app(7, "org.example.mail"), op::LIST_FOR, &body(|w| write_endpoint(w, "api.example", 443)));
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.u32().unwrap(), 1);
    let id = rd.array::<16>().unwrap();
    assert_eq!(rd.u16().unwrap(), 2, "a Cookie");
    assert_eq!(fill(&mut s, &c, &[(cop::SOURCE_ITEM, id)]).0, status::DENIED, "cookies only by Arca's matching");
}

#[test]
fn locked_while_unlocked_items_are_locked_for_couriers_too() {
    let (_d, mut s) = setup();
    let id = store_for(&mut s, &app(7, "org.example.mail"), "api.example", b"tok");
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    let st = fill(&mut s, &courier(7, "org.example.mail", "api.example"), &[(cop::SOURCE_ITEM, id)]).0;
    assert!(st == status::LOCKED || st == status::DENIED, "{st}");
}

#[test]
fn courier_uids_resolve_only_through_the_courier_registry() {
    let d = tempfile::tempdir().unwrap();
    let reg = d.path().join("apps");
    let creg = d.path().join("couriers");
    // An app line claiming a courier uid is ignored.
    std::fs::write(&reg, format!("64001 alice org.example.mail {}\n", "ab".repeat(32))).unwrap();
    std::fs::write(&creg, "64001 alice app-x https api.example 443\n").unwrap();
    let r = Resolver { registry: reg, vestibulum_uid: None, ui_uid: None, allow_unsigned: false, courier_registry: Some(creg.clone()), web_broker: None, services: vec![] };
    match r.resolve(64001) {
        Label::Courier(c) => assert_eq!((c.user.as_str(), c.persona.as_str()), ("alice", "app-x")),
        l => panic!("{l:?}"),
    }
    assert_eq!(r.resolve(64002), Label::Unknown);
    let r = Resolver { courier_registry: None, ..r };
    assert_eq!(r.resolve(64001), Label::Unknown);
}

/// Verify-each-use (arca.md §6.2): a courier's fill of such an item waits
/// for Sigillum's check, which names the item and the endpoint; one check
/// covers the whole fill, and the next fill asks again.
#[test]
fn a_verify_each_use_token_is_filled_only_after_the_check() {
    let (_d, mut s) = setup();
    let bank = app(7, "org.example.bank");
    let plain = store_for(&mut s, &bank, "api.bank.example", b"tok-plain");
    let r = s.handle(&bank, op::STORE_FOR, &body(|w| {
        w.u16(4);
        w.u8(3);
        w.str("transfer token");
        write_endpoint(w, "api.bank.example", 443);
        w.bytes(b"tok-veu");
    }));
    assert_eq!(r.status, status::OK);
    let veu: [u8; 16] = r.body.bytes().try_into().unwrap();
    let c = courier(7, "org.example.bank", "api.bank.example");
    let refs = [(cop::SOURCE_ITEM, plain), (cop::SOURCE_ITEM, veu)];
    let b = body(|w| {
        w.u8(2);
        for (src, x) in &refs {
            w.u8(*src);
            w.raw(x);
        }
    });

    assert_eq!(fill(&mut s, &c, &refs), (status::NEEDS_VERIFICATION, vec![]));
    assert_eq!(
        s.verify_prompt(&c, cop::FILL, &b),
        Some(("alice".to_string(), "\u{201c}transfer token\u{201d} for api.bank.example".to_string()))
    );
    let r = s.handle_verified(&c, cop::FILL, &b);
    assert_eq!(r.status, status::OK);
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.bytes(64).unwrap(), b"tok-plain");
    assert_eq!(rd.bytes(64).unwrap(), b"tok-veu");
    // Once: the next fill is asked about again.
    assert_eq!(fill(&mut s, &c, &refs).0, status::NEEDS_VERIFICATION);
    // A fill of only the while-unlocked token asks nothing.
    assert!(s.verify_prompt(&c, cop::FILL, &body(|w| { w.u8(1); w.u8(cop::SOURCE_ITEM); w.raw(&plain); })).is_none());
    assert_eq!(fill(&mut s, &c, &[(cop::SOURCE_ITEM, plain)]).0, status::OK);
    // Another endpoint's courier gets no sheet, only DENIED.
    let evil = courier(7, "org.example.bank", "evil.example");
    assert!(s.verify_prompt(&evil, cop::FILL, &b).is_none());
    assert_eq!(fill(&mut s, &evil, &refs).0, status::DENIED);
}
