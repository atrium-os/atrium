//! ★ THE DISPLAY PATH IN THE WORKER (backend §4.8, bands step 1): a
//! session's first screen as NSG, normalized ONCE per recording — through
//! both hosts, with the broker's checks.

use navigator_backend::jailed::{JailedHost, WorkerConfig};
use navigator_backend::navigatord::{check_scene, DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::{SessionId, SessionLimits};
use navigator_backend::store::MemoryStore;

const WORKER: &str = env!("CARGO_BIN_EXE_navigator-worker");

/// A recording with a stylesheet the normalizer must apply, and one
/// transition that reveals a hidden note.
fn recording() -> Vec<u8> {
    let doc = r#"<html><head><link rel=\"stylesheet\" href=\"/s.css\"></head><body><h1 class=\"t\">Title</h1><button id=\"b\">Show</button><p id=\"note\" class=\"hide\">The hidden note</p><p>Body text that is always there.</p></body></html>"#;
    format!(r##"{{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":2,"tier_reason":"","measurements":{{}},
      "transitions":[{{"trigger":"#b","event":"click","anchored":true,"effects":[{{"kind":"attribute","target":"#note","name":"class","from":"hide","to":"show"}}]}}],
      "stylesheets":[{{ "href": "/s.css", "media": "", "text": ".t {{ color: #ff0000 }} .hide {{ display: none }}" }}],
      "images":[],
      "document":"{doc}"}}"##).into_bytes()
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>) -> SessionId {
    match n.handle(Request::OpenSession { recording: recording() }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

fn scene<H: DocumentHost>(n: &mut Navigatord<H>, s: SessionId) -> (String, bool) {
    match n.handle(Request::Scene { session: s, limit_px: 600 }).pop() {
        Some(Event::Scene { nsg, from_cache, .. }) => (nsg, from_cache),
        other => panic!("no scene: {other:?}"),
    }
}

fn normalized_once_then_laid_out_from_the_store<H: DocumentHost>(n: Navigatord<H>) {
    let mut n = n.with_blobs(Box::new(MemoryStore::default()));
    let a = open(&mut n);
    let (first, cached) = scene(&mut n, a);
    assert!(!cached, "the first scene of a recording normalizes it");
    assert!(first.starts_with("nsg 0."), "{first}");
    assert!(first.contains("\"Title\"") && first.contains("ff0000ff"), "the stylesheet was applied (normalized): {first}");
    assert!(!first.contains("hidden"), "the hidden note is not drawn");
    // A second session on the same recording is laid out from the stored
    // normalized document, without normalizing again — and draws the same.
    let b = open(&mut n);
    let (second, cached) = scene(&mut n, b);
    assert!(cached, "laid out from the stored normalized document");
    assert_eq!(first, second, "the same first screen either way");
    // A navigated document is normalized again from its current state, and
    // never from (or into) the cache.
    assert!(matches!(n.handle(Request::Navigate { session: b, trigger: "#b".into() }).pop(), Some(Event::SceneReady { .. })));
    let (after, cached) = scene(&mut n, b);
    assert!(!cached, "a navigated document is not the recording's");
    assert!(after.contains("\"hidden\""), "the revealed note is drawn: {after}");
    // The untouched session still gets the cached initial state.
    assert!(scene(&mut n, a).1);
    // A limit outside the bound is refused, and the session survives.
    assert!(matches!(n.handle(Request::Scene { session: a, limit_px: 0 }).pop(), Some(Event::Blocked { .. })));
    assert!(scene(&mut n, a).0.starts_with("nsg"));
}

#[test]
fn in_process_normalizes_once_then_lays_out_from_the_store() {
    normalized_once_then_laid_out_from_the_store(Navigatord::unconfined_for_testing(InProcessHost::default()));
}

#[test]
fn jailed_normalizes_once_then_lays_out_from_the_store() {
    let host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    normalized_once_then_laid_out_from_the_store(Navigatord::unconfined_for_testing(host));
}

#[test]
fn the_broker_refuses_a_scene_of_the_wrong_shape() {
    use navigator_backend::display::SceneAnswer;
    assert!(check_scene(&SceneAnswer { nsg: "<html>".into(), normalized: None }).is_err(), "not NSG");
    assert!(check_scene(&SceneAnswer { nsg: format!("nsg 0.1\n{}", "x".repeat(9 << 20)), normalized: None }).is_err(), "over the size bound");
    assert!(check_scene(&SceneAnswer { nsg: "nsg 0.1\n".into(), normalized: Some(vec![0xff, 0xfe]) }).is_err(), "normalized not UTF-8");
    assert!(check_scene(&SceneAnswer { nsg: "nsg 0.1\n".into(), normalized: Some(b"<p>x</p>".to_vec()) }).is_ok());
}

#[test]
fn a_cached_document_is_what_is_laid_out_and_a_navigated_one_ignores_it() {
    use navigator_backend::display::first_screen;
    let rec = recording();
    let cached = b"<html><body><p>From the store</p></body></html>";
    let a = first_screen(&rec, None, Some(cached), 600).unwrap();
    assert!(a.nsg.contains("\"store\"") && !a.nsg.contains("\"Title\""), "the cached document was laid out: {}", a.nsg);
    assert!(a.normalized.is_none(), "nothing new to store");
    let b = first_screen(&rec, Some("<html><body><p>Current state</p></body></html>"), Some(cached), 600).unwrap();
    assert!(b.nsg.contains("\"Current\"") && !b.nsg.contains("\"store\""), "a navigated document ignores the cache");
    assert!(b.normalized.is_none(), "a navigated document is never stored");
    let c = first_screen(&rec, None, None, 600).unwrap();
    assert!(c.normalized.is_some(), "the initial state, normalized here, is handed back");
    assert!(first_screen(&rec, None, None, 8193).is_err() && first_screen(&rec, None, None, 0).is_err());
}
