//! ★ BANDS OVER THE WIRE (pergola.md §10.4; backend §4.8): `Request::Want`
//! in; `Event::Bands` — the source's events and its groups as NSG — out,
//! through both hosts, with the broker's checks; and Pergola's own
//! `ScrollView` bookkeeping driving a jailed worker.

use navigator_backend::display::{decode_answer, encode_answer, BandsAnswer, GroupPut};
use navigator_backend::jailed::{JailedHost, WorkerConfig};
use navigator_backend::navigatord::{check_bands, DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::session::{SessionId, SessionLimits};
use navigator_backend::store::MemoryStore;
use navigator_render::source::{Event as SE, GroupId};

// Pergola's scroll physics and band bookkeeping, the very file ScrollView uses.
#[allow(dead_code)]
#[path = "../../pergola/src/scroll.rs"]
mod pergola_scroll;

const WORKER: &str = env!("CARGO_BIN_EXE_navigator-worker");
const PX: i64 = 64;

/// A long recording: 150 sections of text, a stylesheet the normalizer must
/// apply, and a transition that reveals a note at the top.
fn recording() -> Vec<u8> {
    let mut body = String::from(r#"<button id=\"b\">Show</button><p id=\"note\" class=\"hide\">The hidden note</p>"#);
    for i in 0..150 {
        body.push_str(&format!(r#"<div class=\"s\"><h2>Section {i}</h2><p>Paragraph {i}: the quick brown fox jumps over the lazy dog, and again, until the line wraps once.</p><p>More text in section {i}.</p></div>"#));
    }
    let doc = format!(r#"<html><head><link rel=\"stylesheet\" href=\"/s.css\"></head><body>{body}</body></html>"#);
    format!(r##"{{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":2,"tier_reason":"","measurements":{{}},
      "transitions":[{{"trigger":"#b","event":"click","anchored":true,"effects":[{{"kind":"attribute","target":"#note","name":"class","from":"hide","to":"show"}}]}}],
      "stylesheets":[{{ "href": "/s.css", "media": "", "text": "h2 {{ color: #ff0000 }} .hide {{ display: none }} .s {{ background-color: #eef }}" }}],
      "images":[],
      "document":"{doc}"}}"##).into_bytes()
}

fn open<H: DocumentHost>(n: &mut Navigatord<H>) -> SessionId {
    match n.handle(Request::OpenSession { recording: recording() }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

type Answer = (Vec<SE>, Vec<GroupPut>, Vec<GroupId>, bool);

fn want<H: DocumentHost>(n: &mut Navigatord<H>, s: SessionId, y: i64) -> Answer {
    match n.handle(Request::Want { session: s, range: (y, y + 600 * PX), zoom_64: 64, prefetch: 600 * PX }).pop() {
        Some(Event::Bands { events, puts, clears, from_cache, .. }) => (events, puts, clears, from_cache),
        other => panic!("no bands: {other:?}"),
    }
}

fn idle_until_settled<H: DocumentHost>(n: &mut Navigatord<H>, s: SessionId) -> i64 {
    for _ in 0..200 {
        match n.handle(Request::BandsIdle { session: s, bands: 16 }).pop() {
            Some(Event::Bands { events, .. }) => {
                if let Some(SE::Extent { height, settled: true }) = events.iter().rev().find(|e| matches!(e, SE::Extent { .. })) { return *height }
            }
            other => panic!("no bands: {other:?}"),
        }
    }
    panic!("never settled")
}

fn wanted_through_the_broker<H: DocumentHost>(n: Navigatord<H>) {
    let mut n = n.with_blobs(Box::new(MemoryStore::default()));
    let a = open(&mut n);
    let (ev, puts, _, cached) = want(&mut n, a, 0);
    assert!(!cached, "the first source of a recording normalizes it");
    assert!(ev.iter().any(|e| matches!(e, SE::BandReady { band: 0, .. })), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, SE::Extent { settled: false, .. })), "an estimate first: {ev:?}");
    let b0 = puts.iter().find(|p| p.group == GroupId::Band(0)).expect("band 0's group");
    assert!(b0.nsg.starts_with("nsg 0.") && b0.nsg.contains("\"Section\"") && b0.nsg.contains("ff0000ff"), "normalized and drawn");
    assert!(!b0.nsg.contains("\"hidden\""));
    // Idle work settles the extent; a jump to the end evicts the top.
    let h = idle_until_settled(&mut n, a);
    let (ev, puts, clears, _) = want(&mut n, a, h - 600 * PX);
    assert!(ev.iter().any(|e| matches!(e, SE::BandEvicted { band: 0 })) && clears.contains(&GroupId::Band(0)), "{ev:?}");
    assert!(puts.iter().any(|p| p.nsg.contains("\"149:\"") || p.nsg.contains("149")), "the last section is drawn");
    // A second session on the same recording builds from the stored document.
    let b = open(&mut n);
    let (_, puts_b, _, cached) = want(&mut n, b, 0);
    assert!(cached, "built from the stored normalized document");
    assert_eq!(puts_b.iter().find(|p| p.group == GroupId::Band(0)).unwrap().nsg, b0.nsg, "the same band either way");
    // A navigated document is brought up to date in place (replay part 2):
    // the band the click changed is sent again as UPDATED, with the
    // revealed note drawn.
    assert!(matches!(n.handle(Request::Navigate { session: b, trigger: "#b".into() }).pop(), Some(Event::SceneReady { .. })));
    let (ev, puts, _, cached) = want(&mut n, b, 0);
    assert!(!cached && ev.iter().any(|e| matches!(e, SE::BandUpdated { band: 0, .. })), "{ev:?}");
    assert!(puts.iter().any(|p| p.nsg.contains("\"hidden\"")), "the band the click changed is drawn again");
    // An ask out of bounds is refused, and the session survives.
    assert!(matches!(n.handle(Request::Want { session: a, range: (10, 0), zoom_64: 64, prefetch: 0 }).pop(), Some(Event::Blocked { .. })));
    assert!(matches!(n.handle(Request::BandsIdle { session: a, bands: 0 }).pop(), Some(Event::Blocked { .. })));
    assert!(!want(&mut n, a, 0).1.is_empty());
}

#[test]
fn in_process_bands_through_the_broker() {
    wanted_through_the_broker(Navigatord::unconfined_for_testing(InProcessHost::default()));
}

#[test]
fn jailed_bands_through_the_broker() {
    let host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    wanted_through_the_broker(Navigatord::unconfined_for_testing(host));
}

#[test]
fn pergola_scroll_view_drives_a_jailed_worker() {
    use pergola_scroll::{DocRange, Physics, SourceEvent as PE, Viewport};
    let host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default());
    let mut n = Navigatord::unconfined_for_testing(host);
    let s = open(&mut n);
    let mut view = Viewport::new();
    let mut phys = Physics::new(0.0);
    let (vh, period) = (600.0, 1000.0 / 60.0);
    let mut top: i64 = 0;
    let mut t = 0.0;
    let mut placeholder_frames = 0;
    let mut wants = 0;
    for frame in 0..600 {
        t += period;
        // A swipe every half second, released into a fling (once the first
        // screen has told the physics how far there is to go).
        if frame % 30 == 1 { for k in 0..8 { phys.drag(60.0, t + k as f64 * 8.0) } t += 64.0 }
        let y_px = phys.step(t);
        top = (y_px * PX as f64) as i64;
        if let Some(w) = view.want(top, vh, 1.0) {
            wants += 1;
            let ev = match n.handle(Request::Want { session: s, range: (w.range.start, w.range.end), zoom_64: 64, prefetch: w.prefetch }).pop() {
                Some(Event::Bands { events, puts, .. }) => {
                    for p in &puts { assert!(p.nsg.starts_with("nsg 0.")) }
                    events
                }
                other => panic!("no bands at frame {frame}: {other:?}"),
            };
            for e in ev {
                let pe = match e {
                    SE::Extent { height, settled } => PE::Extent { height, settled },
                    SE::BandReady { band, range } => PE::BandReady { band, range: DocRange::new(range.0, range.1) },
                    SE::BandUpdated { band, range } => PE::BandUpdated { band, range: DocRange::new(range.0, range.1) },
                    SE::BandEvicted { band } => PE::BandEvicted { band },
                };
                top = view.apply(pe, top);
                phys.set_position(top as f64 / PX as f64);
                if let PE::Extent { height, .. } = pe { phys.set_max(((height / PX) as f64 - vh).max(0.0)) }
            }
        }
        if !view.placeholders(top, (vh as i64) * PX).is_empty() { placeholder_frames += 1 }
    }
    assert!(wants > 3, "the view asked as it scrolled: {wants}");
    assert!(top > 3000 * PX, "the fling went down the document: {}", top / PX);
    assert_eq!(placeholder_frames, 0, "every frame's rows were in resident bands");
    assert!(view.bands().count() <= 5, "the view holds a window of bands, not the document");
}

#[test]
fn the_broker_refuses_bands_of_the_wrong_shape() {
    let ok = BandsAnswer {
        events: vec![SE::BandReady { band: 3, range: (0, 100) }],
        puts: vec![GroupPut { group: GroupId::Band(3), clip: Some((0, 100)), nsg: "nsg 0.2\n".into() }],
        ..Default::default()
    };
    assert!(check_bands(&ok).is_ok());
    let mut bad = ok.clone(); bad.puts[0].group = GroupId::Band(4);
    assert!(check_bands(&bad).is_err(), "a group for a band not announced");
    let mut bad = ok.clone(); bad.puts[0].clip = Some((0, 200));
    assert!(check_bands(&bad).is_err(), "a band group clipped to another range");
    let mut bad = ok.clone(); bad.puts[0].nsg = "<html>".into();
    assert!(check_bands(&bad).is_err(), "not NSG");
    let mut bad = ok.clone(); bad.events[0] = SE::BandReady { band: 3, range: (100, 100) };
    assert!(check_bands(&bad).is_err(), "an empty range");
    let mut bad = ok.clone(); bad.puts = (0..65).map(|_| ok.puts[0].clone()).collect();
    assert!(check_bands(&bad).is_err(), "too many groups");
    let mut bad = ok.clone(); bad.puts.push(GroupPut { group: GroupId::Overlay, clip: Some((0, 1)), nsg: "nsg 0.2\n".into() });
    assert!(check_bands(&bad).is_err(), "the overlay with a band clip");
    let mut bad = ok.clone(); bad.normalized = Some(vec![0xff]);
    assert!(check_bands(&bad).is_err(), "normalized not UTF-8");
    // The wire form round-trips, and its parser refuses what is not it.
    assert_eq!(decode_answer(&encode_answer(&ok)).unwrap(), ok);
    for junk in [&b"x\n"[..], b"0\nE ready +3 0 1\n", b"0\nP b3 0 1 99\nnsg", b"0\nE extent 1 2\n", b"0\nQ\n", b"99999999999\n"] {
        assert!(decode_answer(junk).is_err(), "{:?}", String::from_utf8_lossy(junk));
    }
}
