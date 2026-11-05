//! ★★ WHERE A LINK MAY GO IS THE BROKER'S DECISION (threat model §2.10).
//!
//! https, http (marked insecure) and in-document anchors; every other scheme
//! refused unless the user allowlisted it, and `javascript:` never. Checked
//! on the URL the href RESOLVES to, so the spellings a browser normalises
//! (tabs, case, leading controls) cannot slip past, and checked in
//! `navigatord` — through both hosts — because the href comes from a scene a
//! worker rendered from hostile bytes.

use navigator_backend::jailed::{JailedHost, WorkerConfig};
use navigator_backend::links::{decide, LinkAction, LinkRefusal, SchemeAllowlist, MAX_HREF};
use navigator_backend::navigatord::{DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::{SessionId, SessionLimits};

const DOC: &str = "https://news.test/a/story?id=7";

fn none() -> SchemeAllowlist { SchemeAllowlist::default() }

fn go(href: &str) -> Result<LinkAction, LinkRefusal> { decide(DOC, href, &none()) }

#[test]
fn https_and_http_navigate_and_http_is_marked() {
    assert_eq!(go("https://other.test/x"),
        Ok(LinkAction::Navigate { url: "https://other.test/x".into(), insecure: false }));
    assert_eq!(go("http://other.test/x"),
        Ok(LinkAction::Navigate { url: "http://other.test/x".into(), insecure: true }));
    assert_eq!(go("HTTPS://Other.TEST/x"),
        Ok(LinkAction::Navigate { url: "https://other.test/x".into(), insecure: false }));
}

#[test]
fn relative_links_resolve_against_the_document() {
    assert_eq!(go("../b"),
        Ok(LinkAction::Navigate { url: "https://news.test/b".into(), insecure: false }));
    assert_eq!(go("//cdn.test/p"),
        Ok(LinkAction::Navigate { url: "https://cdn.test/p".into(), insecure: false }));
    // An http page's relative links are http, and marked.
    assert_eq!(decide("http://plain.test/", "x", &none()),
        Ok(LinkAction::Navigate { url: "http://plain.test/x".into(), insecure: true }));
}

#[test]
fn anchors_stay_in_the_document() {
    assert_eq!(go("#comments"), Ok(LinkAction::InDocument { fragment: "comments".into() }));
    assert_eq!(go("#"), Ok(LinkAction::InDocument { fragment: "".into() }));
    // This document's own URL with a fragment is the same document…
    assert_eq!(go("https://news.test/a/story?id=7#c2"),
        Ok(LinkAction::InDocument { fragment: "c2".into() }));
    assert_eq!(go("?id=7#c2"), Ok(LinkAction::InDocument { fragment: "c2".into() }));
    // …another query is another page.
    assert_eq!(go("?id=8#c2"),
        Ok(LinkAction::Navigate { url: "https://news.test/a/story?id=8#c2".into(), insecure: false }));
}

/// ★ Every refused scheme, in the spellings a browser would still open.
#[test]
fn refused_schemes_are_refused_however_they_are_spelled() {
    for href in [
        "javascript:alert(1)", "JavaScript:alert(1)", " javascript:alert(1)",
        "java\tscript:alert(1)", "java\nscript:alert(1)", "\u{1}javascript:alert(1)",
        "javascript&colon;alert(1)",
    ] {
        match go(href) {
            Err(LinkRefusal::Scheme(s)) => assert_eq!(s, "javascript", "{href:?}"),
            // `&colon;` is an entity only in HTML; as a URL it is a relative
            // path, which resolves to https — and that is the right answer.
            Ok(LinkAction::Navigate { url, insecure: false }) if href.contains("&colon;") =>
                assert!(url.starts_with("https://news.test/")),
            other => panic!("{href:?} -> {other:?}"),
        }
    }
    for (href, scheme) in [
        ("file:///etc/passwd", "file"), ("FILE://server/share", "file"),
        ("data:text/html,<script>alert(1)</script>", "data"),
        ("blob:https://news.test/1b2c", "blob"), ("vbscript:msgbox", "vbscript"),
        ("mailto:a@b.test", "mailto"), ("tel:+15551234", "tel"),
        ("ms-settings:privacy", "ms-settings"), ("zoommtg://join?x", "zoommtg"),
        ("about:blank", "about"), ("chrome://settings", "chrome"),
    ] {
        assert_eq!(go(href), Err(LinkRefusal::Scheme(scheme.into())), "{href:?}");
    }
}

#[test]
fn an_allowlisted_scheme_is_handed_off_and_only_that_one() {
    let mut a = none();
    a.allow("mailto").unwrap();
    a.allow("TEL:").unwrap();
    assert_eq!(decide(DOC, "mailto:a@b.test", &a),
        Ok(LinkAction::Handoff { url: "mailto:a@b.test".into(), scheme: "mailto".into() }));
    assert_eq!(decide(DOC, "tel:+15551234", &a),
        Ok(LinkAction::Handoff { url: "tel:+15551234".into(), scheme: "tel".into() }));
    assert_eq!(decide(DOC, "sms:+1", &a), Err(LinkRefusal::Scheme("sms".into())));
}

#[test]
fn javascript_cannot_be_allowlisted_and_nonsense_is_not_a_scheme() {
    let mut a = none();
    assert!(a.allow("javascript").is_err());
    assert!(a.allow("JavaScript:").is_err());
    assert!(a.allow("vbscript").is_err());
    assert!(a.allow("").is_err());
    assert!(a.allow("mail to").is_err());
    assert!(a.allow("1abc").is_err());
    assert_eq!(decide(DOC, "javascript:alert(1)", &a), Err(LinkRefusal::Scheme("javascript".into())));
}

#[test]
fn an_unusable_base_leaves_only_absolute_links() {
    assert_eq!(decide("not a url", "x", &none()), Err(LinkRefusal::Unparseable));
    assert_eq!(decide("not a url", "https://a.test/", &none()),
        Ok(LinkAction::Navigate { url: "https://a.test/".into(), insecure: false }));
    // A document whose own URL is a refused scheme cannot launder its
    // relative links, or its anchors, into anything else.
    assert_eq!(decide("file:///home/u/page.html", "x", &none()), Err(LinkRefusal::Scheme("file".into())));
    assert_eq!(decide("file:///home/u/page.html", "#a", &none()), Err(LinkRefusal::Scheme("file".into())));
}

#[test]
fn an_oversized_href_is_refused_before_it_is_parsed() {
    let long = format!("https://a.test/{}", "x".repeat(MAX_HREF));
    assert_eq!(go(&long), Err(LinkRefusal::TooLong(long.len())));
}

// ── Through the broker ─────────────────────────────────────────────────────

fn recording(url: &str) -> Vec<u8> {
    format!(r##"{{"format":"atrium-navigator-recording/2","url":"{url}","tier":2,
        "tier_reason":"","measurements":{{}},"transitions":[],
        "document":"<html><body><a href=\"javascript:alert(1)\">x</a></body></html>"}}"##).into_bytes()
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>, url: &str) -> SessionId {
    match n.handle(Request::OpenSession { recording: recording(url) }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

fn activate<H: DocumentHost>(n: &mut Navigatord<H>, session: SessionId, href: &str) -> Event {
    n.handle(Request::Activate { session, href: href.into() }).pop().expect("an answer")
}

fn the_broker_decides<H: DocumentHost>(mut n: Navigatord<H>) {
    n.tick(1_000);
    let s = open(&mut n, "https://news.test/a/");
    assert_eq!(activate(&mut n, s, "b"), Event::Link { session: s,
        action: LinkAction::Navigate { url: "https://news.test/a/b".into(), insecure: false } });
    assert_eq!(activate(&mut n, s, "#top"),
        Event::Link { session: s, action: LinkAction::InDocument { fragment: "top".into() } });
    for href in ["javascript:alert(1)", "java\tscript:alert(1)", "file:///etc/passwd",
                 "data:text/html,x", "blob:https://news.test/1", "mailto:a@b.test"] {
        match activate(&mut n, s, href) {
            Event::Blocked { session: Some(x), why } if x == s => assert!(why.contains("refused"), "{why}"),
            other => panic!("{href:?} was not refused: {other:?}"),
        }
    }
    n.allow_scheme("mailto").unwrap();
    assert!(matches!(activate(&mut n, s, "mailto:a@b.test"),
        Event::Link { action: LinkAction::Handoff { .. }, .. }));
    assert!(n.allow_scheme("javascript").is_err());
    // A session that does not exist says so, rather than deciding anything.
    assert!(matches!(activate(&mut n, 999, "https://a.test/"), Event::NoSuchSession { .. }));
}

#[test]
fn the_in_process_broker_decides_links() {
    the_broker_decides(Navigatord::unconfined_for_testing(InProcessHost::default()));
}

/// The same decisions with the document in a worker process: the base URL
/// crosses the pipe (`URL`), the decision does not.
#[test]
fn the_jailed_broker_decides_links() {
    let host = JailedHost::new(
        WorkerConfig::unconfined_for_testing(env!("CARGO_BIN_EXE_navigator-worker")),
        SessionLimits::default());
    the_broker_decides(Navigatord::unconfined_for_testing(host));
}
