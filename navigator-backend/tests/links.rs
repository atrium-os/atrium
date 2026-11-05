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

/// ★ Every href the broker tests below is CARRIED by this document — since
/// 2026-11-05 the broker activates nothing else (see the next section).
/// The tab in `java	script:` is a literal tab in the attribute, as a page
/// would write it.
fn recording(url: &str) -> Vec<u8> {
    let links = ["b", "#top", "javascript:alert(1)", "java\\tscript:alert(1)", "file:///etc/passwd",
                 "data:text/html,x", "blob:https://news.test/1", "mailto:a@b.test", "https://a.test/"];
    let anchors: String = links.iter().map(|h| format!("<a href=\\\"{h}\\\">x</a>")).collect();
    format!(r##"{{"format":"atrium-navigator-recording/2","url":"{url}","tier":2,
        "tier_reason":"","measurements":{{}},"transitions":[],
        "document":"<html><body>{anchors}</body></html>"}}"##).into_bytes()
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>, url: &str) -> SessionId {
    open_rec(n, recording(url))
}

fn open_rec<H: DocumentHost>(n: &mut Navigatord<H>, rec: Vec<u8>) -> SessionId {
    match n.handle(Request::OpenSession { recording: rec }).pop() {
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
            Event::Blocked { session: Some(x), why } if x == s => assert!(why.contains("refused"), "{href:?}: {why}"),
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
    the_broker_decides(Navigatord::unconfined_for_testing(jailed()));
}

fn jailed() -> JailedHost {
    JailedHost::new(
        WorkerConfig::unconfined_for_testing(env!("CARGO_BIN_EXE_navigator-worker")),
        SessionLimits::default())
}

// ── Only links the document carries ────────────────────────────────────────

/// A document with one link, a transition that adds a second by attribute
/// and one that inserts a third, and a script whose TEXT mentions a fourth
/// as markup — present in the recording's bytes, on no element.
fn carrying() -> Vec<u8> {
    br##"{"format":"atrium-navigator-recording/2","url":"https://site.test/","tier":2,
        "tier_reason":"","measurements":{},
        "transitions":[
          {"trigger":"#more","event":"click","anchored":true,
           "effects":[{"kind":"attribute","target":"#later","name":"href","from":null,"to":"https://site.test/later?a=1&b=2"}]},
          {"trigger":"#ins","event":"click","anchored":true,
           "effects":[{"kind":"insert","parent":"#box","index":0,"html":"<a href=\"/inserted\">i</a>"}]}],
        "document":"<html><body><a href=\"https://site.test/here\">h</a><button id=\"more\">m</button><button id=\"ins\">i</button><a id=\"later\">l</a><div id=\"box\"></div><script>var s='<a href=\"https://elsewhere.test/script\">';</script></body></html>"}"##.to_vec()
}

fn refused(e: Event, has: &str) {
    match e {
        Event::Blocked { why, .. } => assert!(why.contains(has), "{why:?} does not say {has:?}"),
        other => panic!("not refused ({has}): {other:?}"),
    }
}

fn only_carried_links<H: DocumentHost>(mut n: Navigatord<H>) {
    n.tick(1_000);
    let s = open_rec(&mut n, carrying());
    assert!(matches!(activate(&mut n, s, "https://site.test/here"), Event::Link { .. }));
    // ★★ A perfectly good https URL the document does not carry is refused
    // — the broker no longer goes wherever it is asked.
    refused(activate(&mut n, s, "https://evil.test/?leak=secret"), "does not carry");
    refused(activate(&mut n, s, "https://site.test/her"), "does not carry");
    refused(activate(&mut n, s, "https://site.test/here/"), "does not carry");
    // In the recording, but only as a transition's RESULT: not yet.
    refused(activate(&mut n, s, "https://site.test/later?a=1&b=2"), "current document does not carry");
    refused(activate(&mut n, s, "/inserted"), "current document does not carry");
    // In the recording's bytes as markup inside a script: on no element.
    refused(activate(&mut n, s, "https://elsewhere.test/script"), "current document does not carry");
    // After the transitions, their links are on the page.
    n.handle(Request::Navigate { session: s, trigger: "#more".into() });
    assert!(matches!(activate(&mut n, s, "https://site.test/later?a=1&b=2"), Event::Link { .. }));
    n.handle(Request::Navigate { session: s, trigger: "#ins".into() });
    assert!(matches!(activate(&mut n, s, "/inserted"),
        Event::Link { action: LinkAction::Navigate { ref url, .. }, .. } if url == "https://site.test/inserted"));
    // And going back takes the inserted link away again.
    n.handle(Request::Back { session: s });
    refused(activate(&mut n, s, "/inserted"), "current document does not carry");
    // Over-long is refused before it is scanned for.
    refused(activate(&mut n, s, &"x".repeat(MAX_HREF + 1)), "over the");
}

#[test]
fn the_in_process_broker_activates_only_carried_links() {
    only_carried_links(Navigatord::unconfined_for_testing(InProcessHost::default()));
}

#[test]
fn the_jailed_broker_activates_only_carried_links() {
    only_carried_links(Navigatord::unconfined_for_testing(jailed()));
}

/// A worker that opens with no triggers, then answers every request with
/// `answer` — a compromised one, claiming whatever it is asked.
fn liar(answer: &str) -> JailedHost {
    use navigator_backend::jailed::Confinement;
    let frame = format!("OK {}\\n{}", answer.len(), answer);
    let script = format!("printf 'OK 0\\n'; while :; do printf '{frame}'; sleep 0.05; done");
    JailedHost::new(WorkerConfig {
        worker: "unused".into(),
        confinement: Confinement::Launcher { program: "/bin/sh".into(), args: vec!["-c".into(), script, "sh".into()] },
        deadline: std::time::Duration::from_secs(5),
        shutdown_grace: std::time::Duration::from_secs(1),
    }, SessionLimits::default())
}

/// ★★★ THE POINT OF THE BOUND: a compromised worker answering "yes" to
/// everything still cannot send the reader anywhere the recording does not
/// already name — so it cannot exfiltrate what it read in a URL.
#[test]
fn a_lying_worker_cannot_invent_a_destination() {
    let mut n = Navigatord::unconfined_for_testing(liar("yes"));
    let s = open_rec(&mut n, carrying());
    refused(activate(&mut n, s, "https://evil.test/?leak=secret"), "the document does not carry");
    refused(activate(&mut n, s, "https://site.test/her"), "the document does not carry");
}

#[test]
fn a_worker_that_answers_neither_yes_nor_no_is_retired() {
    let mut n = Navigatord::unconfined_for_testing(liar("maybe"));
    let s = open_rec(&mut n, carrying());
    refused(activate(&mut n, s, "https://site.test/here"), "unknown link answer");
    assert_eq!(n.host().status(s), navigator_backend::session::Status::Failed);
}

/// The bound recognises the converter's spellings: an href with every
/// character the two escapings touch (`&`, `"`, `<`, `>`, `\`, a tab, a
/// non-ASCII letter) is found in a recording that carries it.
#[test]
fn the_bound_finds_the_converters_spellings() {
    use navigator_backend::carried::Carried;
    use navigator_backend::links::recording_spellings;
    let href = "https://x.test/?a=1&b=\"q\"<>\\\tü";
    // As the emitter writes it inside the document string: HTML-escaped,
    // then JSON-escaped.
    let in_doc = r#""document":"<a href=\"https://x.test/?a=1&amp;b=&quot;q&quot;&lt;&gt;\\\tü\">x</a>""#.as_bytes();
    // As an attribute effect's value: JSON only.
    let in_effect = r#""to": "https://x.test/?a=1&b=\"q\"<>\\\tü""#.as_bytes();
    for rec in [in_doc, in_effect] {
        let c = Carried::new(rec);
        assert!(recording_spellings(href).iter().any(|n| c.contains(n)), "{}", String::from_utf8_lossy(rec));
    }
    // And not a near miss.
    let c = Carried::new(in_doc);
    assert!(!recording_spellings("https://x.test/?a=1&b=").iter().any(|n| c.contains(n)));
}
