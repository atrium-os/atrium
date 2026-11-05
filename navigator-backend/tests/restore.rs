//! ★ A SAVED SESSION COMES BACK AS THE SAME READER'S STATE, OR NOT AT ALL.
//!
//! Byte equality on the document, again: a restore that reaches a document
//! which merely has the right menu open would pass any weaker check. And the
//! record is untrusted — it comes back from a store — so every way it can lie
//! is a refusal here, and a refusal leaves no half-built session behind.

use navigator_backend::navigatord::{Event, InProcessHost, Navigatord, Request};
use navigator_backend::reverse::{Back, Forward};
use navigator_backend::session::{NotRestorable, RestoreError, Saved, SessionLimits, Sessions};
use navigator_backend::store::{DirStore, MemoryStore, Store};
use navigator_backend::{ingest, Limits, Transition};

const NOW: u64 = 24 * 60 * 60 * 1000;

/// Three anchored steps that compose: open the menu, mark it, open a panel.
fn recording() -> Vec<u8> {
    br##"{"format":"atrium-navigator-recording/2","url":"https://a.test/","tier":2,
         "tier_reason":"","measurements":{},
         "transitions":[
           {"trigger":"#menu","event":"click","anchored":true,
            "effects":[{"kind":"attribute","target":"#nav","name":"class","from":"shut","to":"open"}]},
           {"trigger":"#mark","event":"click","anchored":true,
            "effects":[{"kind":"attribute","target":"#nav","name":"data-mark","from":null,"to":"1"}]},
           {"trigger":"#panel","event":"click","anchored":true,
            "effects":[{"kind":"attribute","target":"#p","name":"hidden","from":"","to":null}]}],
         "document":"<html><body><button id=\"menu\">m</button><button id=\"mark\">k</button><button id=\"panel\">p</button><nav id=\"nav\" class=\"shut\"></nav><div id=\"p\" hidden=\"\">x</div></body></html>"}"##.to_vec()
}

fn sessions() -> (Sessions, navigator_backend::Recording) {
    (Sessions::new(SessionLimits::default()), ingest(&recording(), &Limits::default()).expect("ingests"))
}

fn doc(s: &Sessions, id: u64) -> String { s.get(id).unwrap().document().dom.serialize() }

#[test]
fn a_restored_session_has_the_same_document_and_the_same_back_and_forward() {
    let (mut s, r) = sessions();
    let a = s.open(&r, NOW).unwrap();
    {
        let x = s.get_mut(a, NOW).unwrap();
        x.navigate("#menu").unwrap();
        x.navigate("#mark").unwrap();
        x.navigate("#panel").unwrap();
        assert_eq!(x.back().unwrap(), Back::Stepped); // one step ahead
    }
    let saved = s.get(a).unwrap().saved("0".repeat(64).as_str()).unwrap();
    let b = s.restore(&r, &saved, NOW).unwrap();
    assert_eq!(doc(&s, a), doc(&s, b), "the restored document differs");

    // Forward reaches the same state in both, and back walks the same path.
    for id in [a, b] {
        assert_eq!(s.get_mut(id, NOW).unwrap().forward().unwrap(), Forward::Stepped);
    }
    assert_eq!(doc(&s, a), doc(&s, b), "forward diverged after restore");
    for _ in 0..3 {
        for id in [a, b] { assert_eq!(s.get_mut(id, NOW).unwrap().back().unwrap(), Back::Stepped) }
        assert_eq!(doc(&s, a), doc(&s, b), "back diverged after restore");
    }
    for id in [a, b] { assert_eq!(s.get_mut(id, NOW).unwrap().back().unwrap(), Back::AtStart) }
}

#[test]
fn the_record_survives_its_encoding() {
    let (mut s, r) = sessions();
    let a = s.open(&r, NOW).unwrap();
    s.get_mut(a, NOW).unwrap().navigate("#menu").unwrap();
    let saved = s.get(a).unwrap().saved(&"ab".repeat(32)).unwrap();
    assert_eq!(Saved::from_json(&saved.to_json()).unwrap(), saved);
}

#[test]
fn a_step_out_of_range_is_refused_and_leaves_nothing_open() {
    let (mut s, r) = sessions();
    let saved = Saved { url: "u".into(), recording: "0".repeat(64), steps: vec![0, 9], ahead: vec![], document: "0".repeat(64) };
    assert_eq!(s.restore(&r, &saved, NOW), Err(RestoreError::NoSuchStep { index: 9 }));
    assert_eq!(s.len(), 0);
}

#[test]
fn a_record_whose_end_state_is_not_reached_is_refused_and_closed() {
    let (mut s, r) = sessions();
    let a = s.open(&r, NOW).unwrap();
    s.get_mut(a, NOW).unwrap().navigate("#menu").unwrap();
    let mut saved = s.get(a).unwrap().saved(&"0".repeat(64)).unwrap();
    saved.document = "f".repeat(64);
    assert_eq!(s.restore(&r, &saved, NOW), Err(RestoreError::Diverged));
    assert_eq!(s.len(), 1, "only the original session remains");
}

#[test]
fn a_step_that_no_longer_applies_is_refused() {
    let (mut s, r) = sessions();
    // #menu twice: the second finds `open`, not the `shut` it records replacing.
    let saved = Saved { url: "u".into(), recording: "0".repeat(64), steps: vec![0, 0], ahead: vec![], document: "0".repeat(64) };
    assert!(matches!(s.restore(&r, &saved, NOW), Err(RestoreError::Replay { at: 1, .. })));
    assert_eq!(s.len(), 0);
}

#[test]
fn a_step_not_in_the_recording_makes_a_session_unsaveable_and_says_so() {
    let (mut s, r) = sessions();
    let a = s.open(&r, NOW).unwrap();
    let foreign = Transition { trigger: "#menu".into(), event: "click".into(), anchored: true,
        effects: vec![navigator_backend::Effect::Attribute { target: "#nav".into(), name: "data-y".into(), from: None, to: Some("1".into()), at: None }] };
    s.get_mut(a, NOW).unwrap().go(&foreign).unwrap();
    assert_eq!(s.get(a).unwrap().saved("x"), Err(NotRestorable::UnrecordedStep));
}

#[test]
fn malformed_records_are_refused() {
    for bad in [&b"[]"[..], b"{}", br#"{"format":"atrium-navigator-session/1"}"#,
                br#"{"format":"atrium-navigator-session/1","url":"u","recording":"../x","steps":[],"ahead":[],"document":"x"}"#,
                br#"{"format":"atrium-navigator-session/1","url":"u","recording":"0000000000000000000000000000000000000000000000000000000000000000","steps":[-1],"ahead":[],"document":"0000000000000000000000000000000000000000000000000000000000000000"}"#] {
        assert!(Saved::from_json(bad).is_err(), "{}", String::from_utf8_lossy(bad));
    }
    assert!(Saved::from_json(&vec![b' '; 200 * 1024]).is_err(), "oversized is refused before parsing");
}

#[test]
fn a_store_refuses_bytes_that_do_not_match_their_name() {
    let dir = std::env::temp_dir().join(format!("nav-store-{}", std::process::id()));
    let mut st = DirStore::new(&dir).unwrap();
    let a = st.put(b"hello").unwrap();
    assert_eq!(st.get(&a).unwrap(), b"hello");
    std::fs::write(dir.join(&a), b"tampered").unwrap();
    assert!(st.get(&a).is_err(), "a replaced object must not be returned");
    assert!(st.get("../../etc/passwd").is_err(), "not an address, never a path");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- through the broker ----------------------------------------------

fn broker(store: bool) -> Navigatord<InProcessHost> {
    let host = InProcessHost::default();
    let host = if store { host.with_store(Box::new(MemoryStore::default())) } else { host };
    let mut n = Navigatord::unconfined_for_testing(host);
    n.tick(NOW);
    n
}

fn opened(ev: Vec<Event>) -> u64 {
    match ev.as_slice() {
        [Event::SessionOpened { session, .. }] => *session,
        other => panic!("{other:?}"),
    }
}

/// ★ THE POINT OF SAVING: the session expires (or the process restarts) and
/// the reader comes back to where they were.
#[test]
fn a_saved_session_outlives_its_expiry() {
    let mut n = broker(true);
    let s = opened(n.handle(Request::OpenSession { recording: recording() }));
    n.handle(Request::Navigate { session: s, trigger: "#menu".into() });
    n.handle(Request::Navigate { session: s, trigger: "#panel".into() });
    n.handle(Request::Back { session: s });
    let saved = match n.handle(Request::Save { session: s }).as_slice() {
        [Event::Saved { saved, .. }] => saved.clone(),
        other => panic!("{other:?}"),
    };
    let later = NOW + 9 * 60 * 60 * 1000;
    assert!(n.tick(later).iter().any(|e| matches!(e, Event::Expired { session, .. } if *session == s)));

    let r = opened(n.handle(Request::Restore { saved }));
    // Forward goes to the panel the reader had backed out of; back goes to
    // the menu, then the start.
    assert!(matches!(n.handle(Request::Forward { session: r }).as_slice(), [Event::Advanced { how: Forward::Stepped, .. }]));
    assert!(matches!(n.handle(Request::Forward { session: r }).as_slice(), [Event::Advanced { how: Forward::AtEnd, .. }]));
    for how in [Back::Stepped, Back::Stepped, Back::AtStart] {
        match n.handle(Request::Back { session: r }).as_slice() {
            [Event::Rewound { how: h, .. }] => assert_eq!(*h, how),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_host_without_a_store_says_it_cannot_save() {
    let mut n = broker(false);
    let s = opened(n.handle(Request::OpenSession { recording: recording() }));
    assert!(matches!(n.handle(Request::Save { session: s }).as_slice(), [Event::Blocked { .. }]));
    assert!(matches!(n.handle(Request::Restore { saved: "0".repeat(64) }).as_slice(), [Event::Blocked { session: None, .. }]));
}

#[test]
fn restoring_an_address_the_store_does_not_hold_is_blocked() {
    let mut n = broker(true);
    assert!(matches!(n.handle(Request::Restore { saved: "0".repeat(64) }).as_slice(), [Event::Blocked { session: None, .. }]));
    assert!(matches!(n.handle(Request::Restore { saved: "../x".into() }).as_slice(), [Event::Blocked { session: None, .. }]));
}
