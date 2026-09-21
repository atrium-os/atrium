//! ★ A BOUND ON SESSIONS IS A BOUND ON MEMORY THAT IS NOT THE CALLER'S OWN.
//!
//! What an open session holds is the document, and documents come from
//! untrusted input. Without a bound, "open recordings until the process dies"
//! is a supported operation.
//!
//! Two properties get more attention than the happy path:
//!
//!   - Each refusal NAMES ITS BOUND. A caller told "too many sessions" closes
//!     one; a caller told "out of memory" may close several and still fail,
//!     because the next document is simply too large. Merging them into one
//!     error would make the right response unguessable.
//!   - Every bound is paired with the case that must still be ALLOWED. A
//!     session manager that refused everything would satisfy every refusal
//!     test in this file.

use navigator_backend::session::{Expiry, OpenError, SessionLimits, Sessions, Status};
use navigator_backend::{ingest, Effect, Limits, Recording, Transition};

/// Build a real recording by ingesting real JSON — a hand-built `Recording`
/// would let this file pass while the format drifted underneath it.
fn recording(url: &str, body: &str) -> Recording {
    let doc = format!(
        r#"<html><body><button id=\"t\">go</button><nav id=\"m\" data-n=\"0\">{body}</nav></body></html>"#);
    let json = format!(
        r##"{{"format":"atrium-navigator-recording/2","url":"{url}","tier":2,
             "tier_reason":"","measurements":{{}},"transitions":[],
             "document":"{doc}"}}"##);
    ingest(json.as_bytes(), &Limits::default()).expect("a valid recording")
}

/// One fixed instant. Every test that is not ABOUT time uses it, so no
/// test accidentally depends on the clock moving.
/// A day in, so tests that step the clock BACKWARDS have room to do so
/// without underflowing — the backwards case is one of the tests.
const NOW: navigator_backend::session::Millis = 24 * 60 * 60 * 1000;

fn small(url: &str) -> Recording { recording(url, "<span id=\\\"a\\\">A</span>") }

fn step(from: &str, to: &str) -> Transition {
    Transition {
        trigger: "#t".into(), event: "click".into(), anchored: true,
        effects: vec![Effect::Attribute {
            target: "#m".into(), name: "data-n".into(),
            from: Some(from.into()), to: Some(to.into()),
        }],
    }
}

#[test]
fn a_session_opens_and_reads_its_document() {
    let mut s = Sessions::default();
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    assert_eq!(s.len(), 1);
    let sess = s.get(id).expect("is there");
    assert_eq!(sess.url, "https://a.test/");
    assert!(sess.document().dom.by_id("m").is_some());
}

/// ★ THE COUNT BOUND, and the refusal names both numbers.
#[test]
fn opening_past_the_session_limit_is_refused() {
    let limits = SessionLimits { max_sessions: 2, ..Default::default() };
    let mut s = Sessions::new(limits);
    s.open(&small("https://a.test/"), NOW).expect("1");
    s.open(&small("https://b.test/"), NOW).expect("2");
    match s.open(&small("https://c.test/"), NOW).expect_err("must be refused") {
        OpenError::TooManySessions { open, allowed } => {
            assert_eq!((open, allowed), (2, 2));
        }
        other => panic!("wrong error: {other}"),
    }
    assert_eq!(s.len(), 2, "the refused session must not have been created");
}

/// ★ AND CLOSING ONE MAKES ROOM. A bound that could not be recovered from
/// would be a leak with a nicer name.
#[test]
fn closing_a_session_makes_room_for_another() {
    let limits = SessionLimits { max_sessions: 1, ..Default::default() };
    let mut s = Sessions::new(limits);
    let a = s.open(&small("https://a.test/"), NOW).expect("1");
    assert!(s.open(&small("https://b.test/"), NOW).is_err());
    assert!(s.close(a));
    s.open(&small("https://b.test/"), NOW).expect("room was made");
    assert_eq!(s.len(), 1);
}

/// Closing twice is not an error: a caller cleaning up after a failure should
/// not have to know which of two paths already did it.
#[test]
fn closing_an_absent_session_is_false_not_a_panic() {
    let mut s = Sessions::default();
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    assert!(s.close(id));
    assert!(!s.close(id));
    assert!(s.is_empty());
}

/// ★ THE MEMORY BOUND IS SEPARATE FROM THE COUNT, and reports what it needed
/// against what was left — the two numbers a caller needs to decide whether
/// closing something can possibly help.
#[test]
fn a_document_too_large_for_the_budget_is_refused_by_memory_not_by_count() {
    let limits = SessionLimits {
        max_sessions: 16, max_total_bytes: 256, ..Default::default()
    };
    let mut s = Sessions::new(limits);
    let big = recording("https://big.test/", &"<span>x</span>".repeat(100));
    match s.open(&big, NOW).expect_err("must be refused") {
        OpenError::WouldExceedMemory { needed, available } => {
            assert!(needed > available, "needed {needed}, available {available}");
            assert_eq!(available, 256, "nothing else was open");
        }
        other => panic!("wrong bound reported: {other}"),
    }
    assert!(s.is_empty());
}

/// ★★ THE PROFILE REFUSAL COMES FIRST. A caller told it is out of memory will
/// close sessions to make room for a document that was never going to open.
#[test]
fn an_out_of_profile_document_is_refused_as_that_not_as_memory() {
    let limits = SessionLimits { max_total_bytes: 64, ..Default::default() };
    let mut s = Sessions::new(limits);
    let depth = navigator_dom::profile::MAX_DEPTH + 10;
    let deep = recording("https://deep.test/",
        &format!("{}{}", "<div>".repeat(depth), "</div>".repeat(depth)));
    match s.open(&deep, NOW).expect_err("must be refused") {
        OpenError::OutsideProfile(v) =>
            assert!(v.iter().any(|x| x.ceiling == "tree depth"), "{v:?}"),
        other => panic!("the memory bound masked the real reason: {other}"),
    }
}

/// The budget is shared: several small sessions consume it together.
#[test]
fn sessions_share_one_budget() {
    let mut s = Sessions::default();
    let a = s.open(&small("https://a.test/"), NOW).expect("1");
    let before = s.bytes();
    let b = s.open(&small("https://b.test/"), NOW).expect("2");
    assert!(s.bytes() > before, "the second session cost nothing");
    assert_eq!(s.report().len(), 2);
    s.close(a);
    s.close(b);
    assert_eq!(s.bytes(), 0);
}

/// ★ A session's cost is MEASURED as it moves, not fixed at its base. A
/// reader who expands a collapsed section holds more than was published, and
/// a budget charging only the base drifts further from the truth the longer
/// the session lives.
#[test]
fn a_growing_session_costs_more() {
    let mut s = Sessions::default();
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    let before = s.bytes();
    let sess = s.get_mut(id, NOW).expect("is there");
    sess.go(&Transition {
        trigger: "#t".into(), event: "click".into(), anchored: true,
        effects: vec![Effect::Insert {
            parent: "#m".into(), index: Some(0),
            html: "<p>".to_string() + &"content ".repeat(200) + "</p>",
        }],
    }).expect("applies");
    assert!(s.bytes() > before + 1000,
        "the session grew by 1.6 KB but is charged {} more", s.bytes() - before);
}

/// History works through a session, and going back reaches the start.
#[test]
fn a_session_carries_its_history() {
    use navigator_backend::reverse::Back;
    let mut s = Sessions::default();
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    let sess = s.get_mut(id, NOW).expect("is there");
    let start = sess.document().dom.serialize();
    sess.go(&step("0", "1")).expect("1");
    sess.go(&step("1", "2")).expect("2");
    assert_eq!(sess.depth(), 2);
    while sess.back().expect("unwinds") == Back::Stepped {}
    assert_eq!(sess.document().dom.serialize(), start);
}

/// Eviction is the caller's decision, executed here — this module never picks.
#[test]
fn the_caller_chooses_what_to_evict() {
    let mut s = Sessions::new(SessionLimits { max_sessions: 3, ..Default::default() });
    let a = s.open(&small("https://a.test/"), NOW).expect("1");
    let b = s.open(&small("https://b.test/"), NOW).expect("2");
    let c = s.open(&small("https://c.test/"), NOW).expect("3");
    // The caller decides; a report is what it decides from.
    let doomed: Vec<_> = s.report().into_iter()
        .filter(|(id, _, _)| *id != b).map(|(id, _, _)| id).collect();
    assert_eq!(s.close_all(doomed), 2);
    assert_eq!(s.len(), 1);
    assert!(s.get(b).is_some(), "the caller's choice must be the one kept");
    assert!(s.get(a).is_none() && s.get(c).is_none());
}

/// ★ THE COUNTERWEIGHT: the default bound must admit an ordinary workload. A
/// limit that engaged on normal use would be a bug, not protection.
#[test]
fn the_default_bound_admits_a_realistic_set_of_sessions() {
    let mut s = Sessions::default();
    for i in 0..SessionLimits::default().max_sessions {
        s.open(&small(&format!("https://{i}.test/")), NOW)
            .unwrap_or_else(|e| panic!("session {i} refused under the default bound: {e}"));
    }
    assert_eq!(s.len(), 16);
    assert!(s.bytes() < SessionLimits::default().max_total_bytes,
        "16 ordinary documents already exhaust the budget: {}", s.bytes());
}

// ---- lifetime ---------------------------------------------------------
//
// ★ Every test here is exact: the clock is an argument, so an expiry that
// takes eight hours of wall time takes none here. A test that had to sleep to
// prove a timeout would be slow now and flaky later.

const MIN: navigator_backend::session::Millis = 60 * 1000;

/// An idle session expires, and says which bound ended it.
#[test]
fn an_idle_session_expires() {
    let mut s = Sessions::new(SessionLimits { max_idle_ms: 30 * MIN, ..Default::default() });
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    assert!(s.expire(NOW + 29 * MIN).is_empty(), "expired early");
    assert_eq!(s.status(id), Status::Open);

    let gone = s.expire(NOW + 31 * MIN);
    assert_eq!(gone, vec![(id, Expiry::Idle)]);
    assert!(s.is_empty());
}

/// ★★ AGE IS NOT IDLENESS. A session touched on a timer is never idle, so
/// idleness alone would let anything with a heartbeat live forever — and
/// "forever" is not a lifetime.
#[test]
fn an_active_session_still_expires_by_age() {
    let mut s = Sessions::new(SessionLimits {
        max_idle_ms: 30 * MIN, max_age_ms: 120 * MIN, ..Default::default()
    });
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    // Busy every ten minutes for three hours: never idle for a moment.
    let mut t = NOW;
    let mut expired = vec![];
    for _ in 0..18 {
        t += 10 * MIN;
        s.touch(id, t);
        expired.extend(s.expire(t));
    }
    assert_eq!(expired, vec![(id, Expiry::Age)], "activity defeated the lifetime");
}

/// ★ Interaction keeps a session alive — the counterweight, without which the
/// test above would pass on an implementation that expired everything.
#[test]
fn interaction_keeps_a_session_alive() {
    let mut s = Sessions::new(SessionLimits {
        max_idle_ms: 30 * MIN, max_age_ms: 8 * 60 * MIN, ..Default::default()
    });
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    let mut t = NOW;
    for _ in 0..10 {
        t += 25 * MIN;
        assert!(s.touch(id, t), "touch lost the session");
        assert!(s.expire(t).is_empty(), "expired a session in continuous use");
    }
    assert_eq!(s.status(id), Status::Open);
}

/// A mutation counts as interaction without a separate touch.
#[test]
fn reading_the_session_mutably_counts_as_activity() {
    let mut s = Sessions::new(SessionLimits { max_idle_ms: 30 * MIN, ..Default::default() });
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    let t = NOW + 25 * MIN;
    s.get_mut(id, t).expect("is there").go(&step("0", "1")).expect("applies");
    assert!(s.expire(t + 25 * MIN).is_empty(), "the navigation did not count");
}

/// ★★ AN EXPIRED SESSION IS NOT AN UNKNOWN ONE. A reader coming back deserves
/// "that timed out, here it is again", not "no such thing".
#[test]
fn an_expired_session_is_reported_as_expired_not_unknown() {
    let mut s = Sessions::new(SessionLimits { max_idle_ms: 30 * MIN, ..Default::default() });
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    s.expire(NOW + 31 * MIN);
    assert_eq!(s.status(id), Status::Expired(Expiry::Idle));
    assert_eq!(s.status(9999), Status::Unknown, "an id never issued is unknown");
}

/// ★ And the memory of expiries is itself bounded, so it cannot become the
/// leak it was added to explain. The boundary is honest: beyond it, Unknown.
#[test]
fn the_memory_of_expired_sessions_is_bounded() {
    let mut s = Sessions::new(SessionLimits {
        max_idle_ms: MIN, remember_expired: 3, ..Default::default()
    });
    let mut ids = vec![];
    let mut t = NOW;
    for i in 0..6 {
        ids.push(s.open(&small(&format!("https://{i}.test/")), t).expect("opens"));
        t += 2 * MIN;
        s.expire(t);
    }
    assert_eq!(s.status(ids[5]), Status::Expired(Expiry::Idle), "the newest is remembered");
    assert_eq!(s.status(ids[0]), Status::Unknown, "the oldest has faded");
}

/// ★★ OPENING SWEEPS FIRST. Turning a reader away because of a session they
/// abandoned an hour ago is the bound working against the person it protects.
#[test]
fn an_abandoned_session_does_not_block_a_new_one() {
    let mut s = Sessions::new(SessionLimits {
        max_sessions: 1, max_idle_ms: 30 * MIN, ..Default::default()
    });
    let old = s.open(&small("https://a.test/"), NOW).expect("1");
    // Immediately, the bound holds.
    assert!(s.open(&small("https://b.test/"), NOW).is_err());
    // An hour later, the abandoned session is swept and the reader gets in.
    let new = s.open(&small("https://b.test/"), NOW + 60 * MIN).expect("must make room");
    assert_eq!(s.len(), 1);
    assert_eq!(s.status(old), Status::Expired(Expiry::Idle));
    assert_eq!(s.status(new), Status::Open);
}

/// ★ A CLOCK THAT GOES BACKWARDS MUST NOT DELETE SESSIONS. NTP steps wall
/// clocks; a caller may pass one. Going backwards reads as no time passing,
/// which keeps a session open rather than vanishing it under a reader.
#[test]
fn a_clock_that_moves_backwards_expires_nothing() {
    let mut s = Sessions::new(SessionLimits { max_idle_ms: 30 * MIN, ..Default::default() });
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    assert!(s.expire(NOW - 10 * MIN).is_empty(), "a backwards clock expired a session");
    assert_eq!(s.status(id), Status::Open);
    // And a backwards touch cannot make a session look newer than it is.
    s.touch(id, NOW - 20 * MIN);
    assert_eq!(s.expire(NOW + 31 * MIN).len(), 1, "the session became immortal");
}

/// The defaults must admit an ordinary reading session: half an hour of
/// thinking between clicks, over a working day.
#[test]
fn the_default_lifetime_admits_an_ordinary_reading_session() {
    let limits = SessionLimits::default();
    let mut s = Sessions::default();
    let id = s.open(&small("https://a.test/"), NOW).expect("opens");
    let mut t = NOW;
    // A click every 20 minutes for 6 hours: inside both defaults.
    for _ in 0..18 {
        t += 20 * MIN;
        s.touch(id, t);
        assert!(s.expire(t).is_empty(), "the default expired an ordinary session at {t}");
    }
    assert!(t - NOW < limits.max_age_ms, "the test did not stay inside the age bound");
    assert_eq!(s.status(id), Status::Open);
}
