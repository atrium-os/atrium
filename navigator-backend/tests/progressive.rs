//! ★★ Progressive display through the broker (legacy-web §5.4.5.7): the
//! tier-1 document first, the scripted one swapped in at an idle moment with
//! the reader kept where they were, the transition table attached when it
//! arrives. Driven only through requests and events, like navigatord.rs.

use navigator_backend::jailed::{JailedHost, WorkerConfig};
use navigator_backend::navigatord::{DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::SessionLimits;
use navigator_backend::progressive::{Anchor, How};
use navigator_backend::session::SessionId;

const URL: &str = "https://news.example/today";

fn esc(s: &str) -> String { s.replace('\\', "\\\\").replace('"', "\\\"") }

fn recording(tier: u8, transitions: &str, doc: &str) -> Vec<u8> {
    format!(r#"{{"format":"atrium-navigator-recording/3","url":"{URL}","tier":{tier},"tier_reason":"t",
        "measurements":{{}},"transitions":[{transitions}],"stylesheets":[],"images":[],"document":"{}"}}"#, esc(doc)).into_bytes()
}

fn transitions_for(readable: &[u8], transitions: &str) -> Vec<u8> {
    format!(r#"{{"format":"atrium-navigator-transitions/1","url":"{URL}","readable":"blake3:{}",
        "interactive_found":1,"transitions_dropped":0,"transitions":[{transitions}],"end":true}}"#,
        blake3::hash(readable).to_hex()).into_bytes()
}

/// 30 paragraphs of distinct text: what the server sent.
fn article(banner: &str, extra_class: &str) -> String { article_lead(banner, "", extra_class) }
fn article_lead(banner: &str, lead: &str, extra_class: &str) -> String {
    let mut h = format!("<html class=\"{extra_class}\"><body>{banner}<main id=\"story\">{lead}");
    for i in 0..30 { h.push_str(&format!("<p>Paragraph {i} says something particular about item {i}.</p>")); }
    h.push_str("</main><nav id=\"nav\" class=\"shut\"><a href=\"/more\">more</a></nav></body></html>");
    h
}

/// The scripted conversion: a hydrated banner with an image ABOVE the story,
/// and a 40-word live-updates paragraph at the top OF the story — the kind of
/// insertion that moves everything under a reader, and every positional path.
fn scripted() -> String {
    let words: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
    article_lead("<header><img src=\"/hero.jpg\" width=\"10\" height=\"10\"><a href=\"/live\">live</a></header>",
                 &format!("<p>Live: {}</p>", words.join(" ")), "js")
}

const MENU: &str = r##"{"trigger":"#nav","event":"click","anchored":true,
    "effects":[{"kind":"attribute","target":"#nav","name":"class","from":"shut","to":"open"}]}"##;

const WORKER: &str = env!("CARGO_BIN_EXE_navigator-worker");

fn in_process(store: bool) -> InProcessHost {
    let h = InProcessHost::default();
    if store { h.with_store(Box::new(navigator_backend::store::MemoryStore::default())) } else { h }
}
/// ★ The same scenarios through a REAL worker process and the wire: the
/// jailed host keeps its own copy of what it handed over, and must agree.
fn jailed(store: bool) -> JailedHost {
    let h = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    if store { h.with_store(Box::new(navigator_backend::store::MemoryStore::default())) } else { h }
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>, rec: Vec<u8>) -> SessionId {
    match n.handle(Request::OpenSession { recording: rec }).as_slice() {
        [Event::SessionOpened { session, .. }] => *session,
        e => panic!("{e:?}"),
    }
}

/// The reader's anchor: paragraph `i` of the story, `offset` into it.
fn at_paragraph(doc: &str, i: usize, offset: i64) -> Anchor {
    let d = navigator_dom::parse(doc);
    let p = d.by_tag_anywhere("p").into_iter().find(|&h| d.visible_text(h).starts_with(&format!("Paragraph {i} "))).unwrap();
    Anchor { path: d.exact_path(p), offset }
}

fn text_at(doc: &str, path: &str) -> String {
    let d = navigator_dom::parse(doc);
    d.resolve_path(path).map(|h| d.visible_text(h)).unwrap_or_default()
}

macro_rules! scenarios { ($m:ident, $host:ident) => { mod $m { use super::*;
#[test]
fn a_reader_mid_scroll_stays_on_the_paragraph_they_were_reading() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, "", &first));
    let ev = n.handle(Request::Upgrade { session: s, recording: recording(2, "", &scripted()) });
    assert!(matches!(ev.as_slice(), [Event::UpgradeReady { .. }]), "{ev:?}");
    // Mid-scroll: the viewport's top is 640 units (10 px) into paragraph 17.
    let a = at_paragraph(&first, 17, 640);
    // ★ The naive answer is wrong: the same POSITIONAL path in the new
    // document names another element, because the header went in above.
    assert_ne!(text_at(&scripted(), &a.path), text_at(&first, &a.path));
    let ev = n.handle(Request::Swap { session: s, anchor: a.clone() });
    let [Event::Replaced { anchor, .. }] = ev.as_slice() else { panic!("{ev:?}") };
    assert_eq!(anchor.how, How::Same);
    assert_eq!(anchor.anchor.offset, 640);
    assert!(text_at(&scripted(), &anchor.anchor.path).starts_with("Paragraph 17 "), "{anchor:?}");
}

#[test]
fn a_rewritten_paragraph_anchors_to_the_nearest_one_that_survived() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, "", &first));
    let new = scripted().replace("Paragraph 17 says something particular about item 17.", "Rewritten by the page's own script.");
    n.handle(Request::Upgrade { session: s, recording: recording(2, "", &new) });
    let ev = n.handle(Request::Swap { session: s, anchor: at_paragraph(&first, 17, 640) });
    let [Event::Replaced { anchor, .. }] = ev.as_slice() else { panic!("{ev:?}") };
    assert_eq!(anchor.how, How::Nearest);
    // Above first: the reader lands on the paragraph before, never past text.
    assert!(text_at(&new, &anchor.anchor.path).starts_with("Paragraph 16 "), "{anchor:?}");
    assert_eq!(anchor.anchor.offset, 0);
}

#[test]
fn a_scripted_document_that_is_not_meaningfully_different_is_kept_out() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, "", &first));
    // A class flip and two words: nothing a reader would notice.
    let same = article("", "js").replace("item 3.", "item three, today.");
    let ev = n.handle(Request::Upgrade { session: s, recording: recording(2, "", &same) });
    assert!(matches!(ev.as_slice(), [Event::UpgradeKept { .. }]), "{ev:?}");
    let ev = n.handle(Request::Swap { session: s, anchor: at_paragraph(&first, 3, 0) });
    assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains("no scripted document")), "{ev:?}");
}

#[test]
fn a_table_for_the_kept_out_document_brings_it_in_at_the_next_idle_moment() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, "", &first));
    let readable = recording(2, "", &article("", "js"));
    n.handle(Request::Upgrade { session: s, recording: readable.clone() });
    let ev = n.handle(Request::Attach { session: s, transitions: transitions_for(&readable, MENU) });
    assert!(matches!(ev.as_slice(), [Event::UpgradeReady { .. }]), "{ev:?}");
    let ev = n.handle(Request::Swap { session: s, anchor: at_paragraph(&first, 5, 64) });
    let [Event::Replaced { anchor, triggers, .. }] = ev.as_slice() else { panic!("{ev:?}") };
    assert_eq!((anchor.how, anchor.anchor.offset), (How::Same, 64));
    assert_eq!(triggers, &vec!["#nav".to_string()]);
    assert!(matches!(n.handle(Request::Navigate { session: s, trigger: "#nav".into() }).as_slice(), [Event::SceneReady { .. }]));
}

#[test]
fn a_table_for_the_shown_document_attaches_at_once() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let readable = recording(2, "", &scripted());
    let s = open(&mut n, readable.clone());
    let ev = n.handle(Request::Attach { session: s, transitions: transitions_for(&readable, MENU) });
    assert_eq!(ev, vec![Event::TransitionsAttached { session: s, triggers: vec!["#nav".into()] }]);
    assert!(matches!(n.handle(Request::Navigate { session: s, trigger: "#nav".into() }).as_slice(), [Event::SceneReady { .. }]));
    // Not twice.
    let ev = n.handle(Request::Attach { session: s, transitions: transitions_for(&readable, MENU) });
    assert!(matches!(ev.as_slice(), [Event::Blocked { .. }]), "{ev:?}");
}

#[test]
fn a_table_for_the_held_document_goes_in_with_the_swap() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, "", &first));
    let readable = recording(2, "", &scripted());
    n.handle(Request::Upgrade { session: s, recording: readable.clone() });
    let ev = n.handle(Request::Attach { session: s, transitions: transitions_for(&readable, MENU) });
    assert_eq!(ev, vec![Event::TransitionsHeld { session: s }]);
    let ev = n.handle(Request::Swap { session: s, anchor: at_paragraph(&first, 0, 0) });
    assert!(matches!(ev.as_slice(), [Event::Replaced { triggers, .. }] if triggers == &vec!["#nav".to_string()]), "{ev:?}");
}

#[test]
fn the_swap_waits_while_the_reader_has_changed_the_document() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, MENU, &first));
    n.handle(Request::Upgrade { session: s, recording: recording(2, "", &scripted()) });
    n.handle(Request::Navigate { session: s, trigger: "#nav".into() });
    let a = at_paragraph(&first, 2, 0);
    let ev = n.handle(Request::Swap { session: s, anchor: a.clone() });
    assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains("changed this document")), "{ev:?}");
    n.handle(Request::Back { session: s });
    // Back at the start, but with a step ahead: still a history to lose.
    let ev = n.handle(Request::Swap { session: s, anchor: a.clone() });
    assert!(matches!(ev.as_slice(), [Event::Blocked { .. }]), "{ev:?}");
}

#[test]
fn upgrades_for_another_document_twice_or_tables_for_strangers_are_refused() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let s = open(&mut n, recording(1, "", &article("", "no-js")));
    let other = String::from_utf8(recording(2, "", &scripted())).unwrap().replace(URL, "https://other.example/");
    let ev = n.handle(Request::Upgrade { session: s, recording: other.into_bytes() });
    assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains("not this session")), "{ev:?}");
    n.handle(Request::Upgrade { session: s, recording: recording(2, "", &scripted()) });
    let ev = n.handle(Request::Upgrade { session: s, recording: recording(2, "", &scripted()) });
    assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains("already")), "{ev:?}");
    let stranger = transitions_for(b"some other recording", MENU);
    let ev = n.handle(Request::Attach { session: s, transitions: stranger });
    assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains("does not have")), "{ev:?}");
}

#[test]
fn what_the_session_carries_follows_what_it_shows() {
    let mut n = Navigatord::unconfined_for_testing($host(false)); n.tick(1000);
    let first = article("", "no-js");
    let s = open(&mut n, recording(1, "", &first));
    // `/live` is only in the scripted document.
    let ev = n.handle(Request::Activate { session: s, href: "/live".into() });
    assert!(matches!(ev.as_slice(), [Event::Blocked { .. }]), "{ev:?}");
    n.handle(Request::Upgrade { session: s, recording: recording(2, "", &scripted()) });
    n.handle(Request::Swap { session: s, anchor: at_paragraph(&first, 1, 0) });
    let ev = n.handle(Request::Activate { session: s, href: "/live".into() });
    assert!(matches!(ev.as_slice(), [Event::Link { .. }]), "{ev:?}");
}

#[test]
fn a_session_saved_after_its_table_arrived_restores_with_it() {
    let mut n = Navigatord::unconfined_for_testing($host(true));
    n.tick(1000);
    let readable = recording(2, "", &scripted());
    let s = open(&mut n, readable.clone());
    n.handle(Request::Attach { session: s, transitions: transitions_for(&readable, MENU) });
    n.handle(Request::Navigate { session: s, trigger: "#nav".into() });
    let ev = n.handle(Request::Save { session: s });
    let [Event::Saved { saved, .. }] = ev.as_slice() else { panic!("{ev:?}") };
    let ev = n.handle(Request::Restore { saved: saved.clone() });
    assert!(matches!(ev.as_slice(), [Event::SessionOpened { triggers, .. }] if triggers == &vec!["#nav".to_string()]), "{ev:?}");
}

} } }

scenarios!(in_process_host, in_process);
scenarios!(jailed_host, jailed);
