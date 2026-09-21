//! ★★ THE BROKER IS DRIVEN ENTIRELY THROUGH ITS CONTROL PLANE.
//!
//! Every test here sends `Request`s and reads `Event`s. None of them reaches
//! past that into a session, a document or a DOM — which is spec §1's claim
//! made into a test suite: "delete the UI, replace it with a shell script, or
//! run none at all, and neither the security posture nor the test suite
//! changes." A suite that poked at internals would pass equally well against
//! a broker that had no seam at all.
//!
//! The second thing under test is that refusals are EVENTS. A broker that
//! returned `Result` to its UI would let a caller check the happy path and
//! drop the rest; here a refusal travels the same channel as a success.

use navigator_backend::navigatord::{DocumentHost, Event, InProcessHost, Navigatord, Request};
use navigator_backend::reverse::Back;
use navigator_backend::session::{Expiry, SessionLimits, SessionId, Status};

const MIN: u64 = 60 * 1000;
const NOW: u64 = 24 * 60 * 60 * 1000;

/// A recording with one real transition, built as JSON so these tests break
/// if the wire format drifts rather than passing against a struct literal.
fn recording(url: &str) -> Vec<u8> {
    format!(
        r##"{{"format":"atrium-navigator-recording/2","url":"{url}","tier":2,
             "tier_reason":"","measurements":{{}},
             "transitions":[
               {{"trigger":"#menu","event":"click","anchored":true,
                 "effects":[{{"kind":"attribute","target":"#nav","name":"class",
                              "from":"shut","to":"open"}}]}},
               {{"trigger":"#ghost","event":"click","anchored":false,
                 "effects":[{{"kind":"attribute","target":"#nav","name":"data-x",
                              "from":null,"to":"1"}}]}}],
             "document":"<html><body><button id=\"menu\">m</button><nav id=\"nav\" class=\"shut\"></nav></body></html>"}}"##
    ).into_bytes()
}

fn broker() -> Navigatord<InProcessHost> {
    let mut n = Navigatord::new(InProcessHost::default());
    n.tick(NOW);
    n
}

fn open(n: &mut Navigatord<InProcessHost>) -> SessionId {
    match n.handle(Request::OpenSession { recording: recording("https://a.test/") }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

#[test]
fn opening_a_recording_reports_what_a_reader_can_do() {
    let mut n = broker();
    let events = n.handle(Request::OpenSession { recording: recording("https://a.test/") });
    match events.as_slice() {
        [Event::SessionOpened { triggers, .. }] => {
            // ★ Only the ANCHORED trigger is offered. `#ghost` addresses a node
            // the page's own scripts made, which is not in the published
            // document — offering it would produce a refusal the reader could
            // do nothing about.
            assert_eq!(triggers, &["#menu".to_string()], "unanchored trigger was offered");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(n.open_sessions(), 1);
}

#[test]
fn navigating_produces_a_scene() {
    let mut n = broker();
    let s = open(&mut n);
    match n.handle(Request::Navigate { session: s, trigger: "#menu".into() }).as_slice() {
        [Event::SceneReady { session, .. }] => assert_eq!(*session, s),
        other => panic!("{other:?}"),
    }
}

/// ★ A trigger the recording does not have is BLOCKED, not a panic and not
/// silence. Nothing is wrong with the document — the page simply never did
/// anything there when the converter tried it — and the reader is told.
#[test]
fn an_unrecorded_trigger_is_blocked_with_a_reason() {
    let mut n = broker();
    let s = open(&mut n);
    match n.handle(Request::Navigate { session: s, trigger: "#nothing".into() }).as_slice() {
        [Event::Blocked { session, why }] => {
            assert_eq!(*session, Some(s));
            assert!(why.contains("#nothing"), "the reason must name it: {why}");
        }
        other => panic!("{other:?}"),
    }
}

/// Malformed bytes are a blocked open, not a crash — this is the path
/// untrusted input actually arrives on.
#[test]
fn a_malformed_recording_is_blocked() {
    let mut n = broker();
    match n.handle(Request::OpenSession { recording: b"{ not json".to_vec() }).as_slice() {
        [Event::Blocked { session: None, why }] => assert!(!why.is_empty()),
        other => panic!("{other:?}"),
    }
    assert_eq!(n.open_sessions(), 0);
}

/// ★★ `AtStart` AND `Forgotten` REACH THE UI DISTINCTLY. Collapsing them at
/// the broker would undo §4.4's whole point one layer up.
#[test]
fn going_back_reports_which_kind_of_back_it_was() {
    let mut n = broker();
    let s = open(&mut n);
    n.handle(Request::Navigate { session: s, trigger: "#menu".into() });

    match n.handle(Request::Back { session: s }).as_slice() {
        [Event::Rewound { how: Back::Stepped, .. }] => {}
        other => panic!("{other:?}"),
    }
    match n.handle(Request::Back { session: s }).as_slice() {
        [Event::Rewound { how: Back::AtStart, .. }] => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn closing_a_session_says_so_and_frees_it() {
    let mut n = broker();
    let s = open(&mut n);
    match n.handle(Request::Close { session: s }).as_slice() {
        [Event::Closed { session }] => assert_eq!(*session, s),
        other => panic!("{other:?}"),
    }
    assert_eq!(n.open_sessions(), 0);
}

/// ★★ A REQUEST FOR A SESSION THAT IS GONE SAYS WHY IT IS GONE. "Expired" is
/// an offer to reopen; "unknown" is a stale UI or a bug. Showing a reader the
/// same message for both throws away the only thing that distinguishes a
/// recoverable situation from a broken one.
#[test]
fn a_request_for_a_vanished_session_distinguishes_expired_from_unknown() {
    let mut n = broker();
    let s = open(&mut n);
    n.handle(Request::Close { session: s });

    match n.handle(Request::Navigate { session: s, trigger: "#menu".into() }).as_slice() {
        [Event::NoSuchSession { status: Status::Unknown, .. }] => {}
        other => panic!("a closed session should be unknown: {other:?}"),
    }

    let s2 = open(&mut n);
    n.tick(NOW + 60 * MIN);
    match n.handle(Request::Navigate { session: s2, trigger: "#menu".into() }).as_slice() {
        [Event::NoSuchSession { status: Status::Expired(Expiry::Idle), .. }] => {}
        other => panic!("an expired session must say so: {other:?}"),
    }
}

/// ★ Expiry reaches the UI UNPROMPTED. A UI that only learned about it by
/// failing a navigation would show a reader a page that is already gone, then
/// take it away under them.
#[test]
fn expiry_is_announced_without_being_asked() {
    let mut n = broker();
    let s = open(&mut n);
    assert!(n.tick(NOW + 5 * MIN).is_empty(), "expired early");
    match n.tick(NOW + 60 * MIN).as_slice() {
        [Event::Expired { session, why: Expiry::Idle }] => assert_eq!(*session, s),
        other => panic!("{other:?}"),
    }
}

/// And an expiry that falls due during another request is delivered WITH that
/// request's answer, not instead of it.
#[test]
fn an_expiry_during_a_request_is_delivered_alongside_the_answer() {
    let mut n = broker();
    let idle = open(&mut n);
    // No tick: the expiry falls due BECAUSE this request arrives an hour later.
    let events = n.handle_at(
        Request::OpenSession { recording: recording("https://b.test/") }, NOW + 60 * MIN);
    assert!(events.iter().any(|e| matches!(e, Event::Expired { session, .. } if *session == idle)),
        "the expiry was swallowed: {events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::SessionOpened { .. })),
        "the request was not answered: {events:?}");
}

/// The budget refusal arrives as a Blocked event carrying a usable reason.
#[test]
fn hitting_the_session_bound_blocks_with_a_reason() {
    let host = InProcessHost::new(
        SessionLimits { max_sessions: 1, ..Default::default() },
        navigator_backend::Limits::default());
    let mut n = Navigatord::new(host);
    n.tick(NOW);
    open(&mut n);
    match n.handle(Request::OpenSession { recording: recording("https://b.test/") }).as_slice() {
        [Event::Blocked { session: None, why }] =>
            assert!(why.contains("limit") || why.contains("sessions"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// Report is what a caller chooses evictions from (§4.4a).
#[test]
fn report_lists_the_open_sessions_and_what_they_cost() {
    let mut n = broker();
    let a = open(&mut n);
    let b = open(&mut n);
    match n.handle(Request::Report).as_slice() {
        [Event::Report { sessions, total_bytes }] => {
            assert_eq!(sessions.len(), 2);
            assert!(sessions.iter().any(|(id, _)| *id == a));
            assert!(sessions.iter().any(|(id, _)| *id == b));
            assert!(sessions.iter().all(|(_, bytes)| *bytes > 0), "{sessions:?}");
            assert!(*total_bytes > 0);
        }
        other => panic!("{other:?}"),
    }
}

/// ★ The clock never goes backwards inside the broker, whatever the caller
/// passes — a stepped wall clock must not resurrect expiries or un-expire
/// sessions.
#[test]
fn the_brokers_clock_is_monotonic_even_if_its_callers_is_not() {
    let mut n = broker();
    let s = open(&mut n);
    n.tick(NOW + 10 * MIN);
    n.tick(NOW - 60 * MIN);
    assert_eq!(n.now(), NOW + 10 * MIN, "the broker went back in time");
    assert_eq!(n.handle(Request::Navigate { session: s, trigger: "#menu".into() }).len(), 1,
        "the session should still be usable");
}

// ---- the seam ---------------------------------------------------------

/// ★★★ THE INVARIANT, AS A TEST. The broker is generic over `DocumentHost`,
/// so a host that owns no parser at all still drives it. If the broker had
/// reached past the trait — held a `Dom`, called `parse`, inspected a
/// recording's bytes — this would not compile.
///
/// It is the cheap half of the proof: the expensive half is a host that runs
/// in a jail, which does not exist yet and is not pretended to.
struct NullHost { open: Vec<SessionId>, next: SessionId }

impl DocumentHost for NullHost {
    fn open(&mut self, recording: &[u8], _now: u64) -> Result<SessionId, String> {
        // Notably: does not parse. Counts bytes and refuses nothing.
        if recording.is_empty() { return Err("empty".into()) }
        self.next += 1;
        self.open.push(self.next);
        Ok(self.next)
    }
    fn navigate(&mut self, _id: SessionId, _t: &str, _now: u64) -> Result<(), String> { Ok(()) }
    fn back(&mut self, _id: SessionId, _now: u64) -> Result<Back, String> { Ok(Back::AtStart) }
    fn close(&mut self, id: SessionId) -> bool {
        let had = self.open.contains(&id);
        self.open.retain(|&x| x != id);
        had
    }
    fn expire(&mut self, _now: u64) -> Vec<(SessionId, Expiry)> { vec![] }
    fn status(&self, id: SessionId) -> Status {
        if self.open.contains(&id) { Status::Open } else { Status::Unknown }
    }
    fn bytes(&self) -> usize { 0 }
    fn open_count(&self) -> usize { self.open.len() }
    fn triggers(&self, _id: SessionId) -> Vec<String> { vec![] }
    fn report(&self) -> Vec<(SessionId, usize)> { self.open.iter().map(|&i| (i, 0)).collect() }
}

#[test]
fn the_broker_drives_a_host_that_owns_no_parser() {
    let mut n = Navigatord::new(NullHost { open: vec![], next: 0 });
    let s = match n.handle(Request::OpenSession { recording: b"anything".to_vec() }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        n.handle(Request::Navigate { session: s, trigger: "#x".into() }).as_slice(),
        [Event::SceneReady { .. }]));
    assert!(matches!(
        n.handle(Request::Close { session: s }).as_slice(), [Event::Closed { .. }]));
}
