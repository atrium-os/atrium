//! ★★ THE IMAGE-FREE FIRST DISPLAY, THROUGH THE BROKER (legacy-web
//! §5.4.5.7): the first document is shown with its images pending, each in
//! a box; images artifacts bound to it arrive later, and the broker attaches
//! them — the session's recording then names their bytes, so the UI may ask
//! for their pixels — and says which ones changed size. Both hosts.

mod common;
use common::images::*;
use navigator_backend::jailed::{JailedHost, WorkerConfig};
use navigator_backend::navigatord::{DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::{SessionId, SessionLimits};
use navigator_backend::store::{MemoryStore, Store};

const URL: &str = "https://news.example/today";
const WORKER: &str = env!("CARGO_BIN_EXE_navigator-worker");

fn esc(s: &str) -> String { s.replace('\\', "\\\\").replace('"', "\\\"") }

const DOC: &str = "<html><body><p>Lead paragraph.</p><img src=\"/a.png\" width=\"3\" height=\"2\"><img src=\"/b.png\"><p>More.</p></body></html>";

/// The first document, as the converter writes it: `/a.png` declared 3 × 2,
/// `/b.png` with no size, both pending.
fn first() -> Vec<u8> {
    format!(r#"{{"format":"atrium-navigator-recording/3","url":"{URL}","tier":1,"tier_reason":"first display: scripts not run yet",
        "measurements":{{}},"transitions":[],"stylesheets":[],
        "images":[{{"src":"/a.png","width":3,"height":2,"ratio":"3/2","address":"","pending":"declared"}},
                  {{"src":"/b.png","width":null,"height":null,"ratio":"none","address":"","pending":"placeholder"}}],
        "document":"{}"}}"#, esc(DOC)).into_bytes()
}

fn entry(src: &str, w: u32, h: u32, address: &str) -> String {
    format!(r#"{{"src":"{src}","width":{w},"height":{h},"ratio":"{w}/{h}","address":"{address}"}}"#)
}

fn images_for(first: &[u8], complete: bool, entries: &[String]) -> Vec<u8> {
    format!(r#"{{"format":"atrium-navigator-images/1","url":"{URL}","first":"blake3:{}","complete":{complete},"images":[{}],"end":true}}"#,
        blake3::hash(first).to_hex(), entries.join(",")).into_bytes()
}

const B_PENDING: &str = r#"{"src":"/b.png","width":null,"height":null,"ratio":"none","address":"","pending":"placeholder"}"#;

struct Fixture { a: Vec<u8>, b: Vec<u8>, blobs: MemoryStore }
fn fixture() -> Fixture {
    let (a, b) = (png_rgba(3, 2, &pattern(3, 2)), png_rgba(4, 4, &pattern(4, 4)));
    let mut blobs = MemoryStore::default();
    for x in [&a, &b] { blobs.put(x).unwrap(); }
    Fixture { a, b, blobs }
}

fn in_process() -> InProcessHost { InProcessHost::default().with_store(Box::new(MemoryStore::default())) }
fn jailed() -> JailedHost {
    JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default())
        .with_store(Box::new(MemoryStore::default()))
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>, rec: Vec<u8>) -> SessionId {
    match n.handle(Request::OpenSession { recording: rec }).as_slice() {
        [Event::SessionOpened { session, .. }] => *session,
        e => panic!("{e:?}"),
    }
}

fn pixels<H: DocumentHost>(n: &mut Navigatord<H>, s: SessionId, address: &str) -> Event {
    n.handle(Request::Image { session: s, address: address.into(), painted: None }).pop().unwrap()
}

macro_rules! scenarios { ($m:ident, $host:ident) => { mod $m { use super::*;

#[test]
fn images_arrive_in_their_boxes_and_only_then_can_be_painted() {
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing($host()).with_blobs(Box::new(f.blobs));
    n.tick(1000);
    let s = open(&mut n, first());
    let (a, b) = (address(&f.a), address(&f.b));
    // Not yet: the first document names no image bytes.
    assert!(matches!(pixels(&mut n, s, &a), Event::Blocked { .. }));
    // `/a.png` arrives at its declared size: nothing moves.
    let ev = n.handle(Request::Images { session: s, images: images_for(&first(), false, &[entry("/a.png", 3, 2, &a), B_PENDING.into()]) });
    assert_eq!(ev, vec![Event::ImagesAttached { session: s, arrived: vec![a.clone()], resized: vec![], complete: false }]);
    assert!(matches!(pixels(&mut n, s, &a), Event::Image { .. }));
    assert!(matches!(pixels(&mut n, s, &b), Event::Blocked { .. }));
    // `/b.png` arrives; it had no size, so its box changes.
    let ev = n.handle(Request::Images { session: s, images: images_for(&first(), true, &[entry("/a.png", 3, 2, &a), entry("/b.png", 4, 4, &b)]) });
    assert_eq!(ev, vec![Event::ImagesAttached { session: s, arrived: vec![b.clone()], resized: vec!["/b.png".into()], complete: true }]);
    assert!(matches!(pixels(&mut n, s, &b), Event::Image { .. }));
}

#[test]
fn a_declared_image_whose_real_size_differs_is_resized_and_a_failed_one_is_gone() {
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing($host()).with_blobs(Box::new(f.blobs));
    n.tick(1000);
    let s = open(&mut n, first());
    let a = address(&f.a);
    // `/a.png` turned out 30 × 20 (the same ratio, not the same size), and
    // `/b.png` failed, so it is not in the list at all: both boxes change.
    let ev = n.handle(Request::Images { session: s, images: images_for(&first(), true, &[entry("/a.png", 30, 20, &a)]) });
    assert_eq!(ev, vec![Event::ImagesAttached { session: s, arrived: vec![a], resized: vec!["/a.png".into(), "/b.png".into()], complete: true }]);
}

#[test]
fn images_for_another_document_after_completion_or_still_pending_when_complete_are_refused() {
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing($host()).with_blobs(Box::new(f.blobs));
    n.tick(1000);
    let s = open(&mut n, first());
    let a = address(&f.a);
    let refused = |ev: Vec<Event>, has: &str| assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains(has)), "{has}: {ev:?}");
    refused(n.handle(Request::Images { session: s, images: images_for(b"another first document", true, &[entry("/a.png", 3, 2, &a)]) }), "does not show");
    refused(n.handle(Request::Images { session: s, images: images_for(&first(), true, &[entry("/a.png", 3, 2, &a), B_PENDING.into()]) }), "still pending");
    refused(n.handle(Request::Images { session: s, images: images_for(&first(), true, &[entry("/a.png", 3, 2, "blake3:nothex")]) }), "blake3");
    refused(n.handle(Request::Images { session: s, images: images_for(&first(), true, &[entry("/a.png", 3, 2, &a), entry("/a.png", 3, 2, &a)]) }), "twice");
    let done = images_for(&first(), true, &[entry("/a.png", 3, 2, &a), entry("/b.png", 4, 4, &address(&f.b))]);
    assert!(matches!(n.handle(Request::Images { session: s, images: done.clone() }).as_slice(), [Event::ImagesAttached { complete: true, .. }]));
    refused(n.handle(Request::Images { session: s, images: done }), "all its images");
}

#[test]
fn a_session_saved_after_its_images_arrived_restores_with_them() {
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing($host()).with_blobs(Box::new(f.blobs));
    n.tick(1000);
    let s = open(&mut n, first());
    let a = address(&f.a);
    n.handle(Request::Images { session: s, images: images_for(&first(), false, &[entry("/a.png", 3, 2, &a), B_PENDING.into()]) });
    let saved = match n.handle(Request::Save { session: s }).as_slice() { [Event::Saved { saved, .. }] => saved.clone(), e => panic!("{e:?}") };
    let r = match n.handle(Request::Restore { saved }).as_slice() { [Event::SessionOpened { session, .. }] => *session, e => panic!("{e:?}") };
    assert!(matches!(pixels(&mut n, r, &a), Event::Image { .. }), "the restored session names the image that had arrived");
}

#[test]
fn after_the_swap_the_first_documents_images_are_not_taken() {
    let f = fixture();
    let mut n = Navigatord::unconfined_for_testing($host()).with_blobs(Box::new(f.blobs));
    n.tick(1000);
    let s = open(&mut n, first());
    let scripted = format!(r#"{{"format":"atrium-navigator-recording/3","url":"{URL}","tier":2,"tier_reason":"t","measurements":{{}},
        "transitions":[],"stylesheets":[],"images":[],"document":"{}"}}"#,
        esc(&DOC.replace("Lead paragraph.", "A lead paragraph a script wrote, long enough to be a real difference for a reader.")));
    let ev = n.handle(Request::Upgrade { session: s, recording: scripted.into_bytes() });
    assert!(matches!(ev.as_slice(), [Event::UpgradeReady { .. }]), "{ev:?}");
    let anchor = navigator_backend::progressive::Anchor { path: String::new(), offset: 0 };
    assert!(matches!(n.handle(Request::Swap { session: s, anchor }).as_slice(), [Event::Replaced { .. }]));
    let ev = n.handle(Request::Images { session: s, images: images_for(&first(), true, &[entry("/a.png", 3, 2, &address(&f.a))]) });
    assert!(matches!(ev.as_slice(), [Event::Blocked { why, .. }] if why.contains("replaced")), "{ev:?}");
}

} } }

scenarios!(in_process_host, in_process);
scenarios!(jailed_host, jailed);
