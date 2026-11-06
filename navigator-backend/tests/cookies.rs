//! ★ THE READER'S COOKIE ACTION (legacy-web §5.4.1e): `Request::Cookies`
//! turns a session's view cookied, with its partition (its document's
//! site), or back; a session never turned cookied has none, so nothing ever
//! asks Arca for a ticket for it.

use navigator_backend::navigatord::{Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::SessionId;

fn recording(url: &str) -> Vec<u8> {
    format!(r##"{{"format":"atrium-navigator-recording/3","url":"{url}","tier":2,"tier_reason":"","measurements":{{}},
      "transitions":[],"stylesheets":[],"images":[],
      "document":"<html><body><p>Accept cookies to continue.</p><button id=\"ok\">Accept</button></body></html>"}}"##).into_bytes()
}

fn open(n: &mut Navigatord<InProcessHost>, url: &str) -> SessionId {
    match n.handle(Request::OpenSession { recording: recording(url) }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

#[test]
fn the_readers_action_turns_a_view_cookied_with_its_sites_partition() {
    let mut n = Navigatord::unconfined_for_testing(InProcessHost::default());
    let a = open(&mut n, "https://www.news.example.co.uk/article/1");
    let b = open(&mut n, "https://cdn.other.example/");
    assert_eq!(n.cookied(a), None, "anonymous by default");
    match n.handle(Request::Cookies { session: a, allow: true }).pop() {
        Some(Event::Cookies { session, cookied: true, partition }) => {
            assert_eq!(session, a);
            assert_eq!(partition, "example.co.uk", "the registrable domain: one label past the public suffix co.uk");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(n.cookied(a), Some("example.co.uk"));
    assert_eq!(n.cookied(b), None, "another view is not touched");
    // Back to anonymous.
    assert!(matches!(n.handle(Request::Cookies { session: a, allow: false }).pop(), Some(Event::Cookies { cookied: false, .. })));
    assert_eq!(n.cookied(a), None);
    // Closed: gone with it.
    n.handle(Request::Cookies { session: b, allow: true });
    assert_eq!(n.cookied(b), Some("other.example"));
    n.handle(Request::Close { session: b });
    assert_eq!(n.cookied(b), None);
}

#[test]
fn a_document_without_a_site_or_a_session_that_is_gone_is_refused() {
    let mut n = Navigatord::unconfined_for_testing(InProcessHost::default());
    let f = open(&mut n, "file:///home/reader/page.html");
    assert!(matches!(n.handle(Request::Cookies { session: f, allow: true }).pop(), Some(Event::Blocked { .. })));
    assert_eq!(n.cookied(f), None);
    assert!(!matches!(n.handle(Request::Cookies { session: 999, allow: true }).pop(), Some(Event::Cookies { .. })));
    assert_eq!(n.cookied(999), None);
}
