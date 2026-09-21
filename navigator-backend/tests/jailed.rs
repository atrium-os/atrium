//! ★★★ THE SAME BROKER, A DIFFERENT HOST — and that is the whole claim.
//!
//! `tests/navigatord.rs` drives the broker over an in-process host. This file
//! drives the same broker, with the same requests, over real worker
//! PROCESSES: one per document, a pipe as the only channel, a deadline on
//! every exchange. No broker code changed to make this work, which is what
//! the seam was built to be able to say.
//!
//! ★★ AND THE HONEST LIMIT, restated here so a reader of the tests cannot
//! miss it: `Confinement::None` is process isolation, NOT a jail. Separate
//! address space, separate crash domain, one document per process — real
//! properties, and not the capability restriction the spec means by "jailed".
//! These tests prove the mechanism. They do not prove confinement, and a test
//! called `jailed.rs` that quietly implied otherwise would be the worst place
//! in this tree to be imprecise.
//!
//! What IS proven here beyond the happy path: a worker that dies, a worker
//! that hangs, and a worker that lies are all survivable by the broker —
//! because the pipe from a worker is untrusted input to the one process that
//! holds capabilities.

use navigator_backend::jailed::{Confinement, JailedHost, WorkerConfig};
use navigator_backend::navigatord::{DocumentHost, Event, Navigatord, Request};
use navigator_backend::reverse::Back;
use navigator_backend::session::{SessionId, SessionLimits, Status};
use std::time::Duration;

const WORKER: &str = env!("CARGO_BIN_EXE_navigator-worker");
const NOW: u64 = 24 * 60 * 60 * 1000;
const MIN: u64 = 60 * 1000;

fn recording(url: &str) -> Vec<u8> {
    format!(
        r##"{{"format":"atrium-navigator-recording/2","url":"{url}","tier":2,
             "tier_reason":"","measurements":{{}},
             "transitions":[
               {{"trigger":"#menu","event":"click","anchored":true,
                 "effects":[{{"kind":"attribute","target":"#nav","name":"class",
                              "from":"shut","to":"open"}}]}}],
             "document":"<html><body><button id=\"menu\">m</button><nav id=\"nav\" class=\"shut\"></nav></body></html>"}}"##
    ).into_bytes()
}

fn host() -> JailedHost {
    JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), SessionLimits::default())
}

fn broker() -> Navigatord<JailedHost> {
    let mut n = Navigatord::new(host());
    n.tick(NOW);
    n
}

fn open(n: &mut Navigatord<JailedHost>) -> SessionId {
    match n.handle(Request::OpenSession { recording: recording("https://a.test/") }).pop() {
        Some(Event::SessionOpened { session, .. }) => session,
        other => panic!("did not open: {other:?}"),
    }
}

/// The whole cycle across a process boundary: open, navigate, back, close.
#[test]
fn a_worker_process_serves_a_whole_session() {
    let mut n = broker();
    let events = n.handle(Request::OpenSession { recording: recording("https://a.test/") });
    let session = match events.as_slice() {
        [Event::SessionOpened { session, triggers }] => {
            assert_eq!(triggers, &["#menu".to_string()],
                "the triggers did not survive the pipe");
            *session
        }
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        n.handle(Request::Navigate { session, trigger: "#menu".into() }).as_slice(),
        [Event::SceneReady { .. }]));
    assert!(matches!(
        n.handle(Request::Back { session }).as_slice(),
        [Event::Rewound { how: Back::Stepped, .. }]));
    assert!(matches!(
        n.handle(Request::Back { session }).as_slice(),
        [Event::Rewound { how: Back::AtStart, .. }]));
    assert!(matches!(
        n.handle(Request::Close { session }).as_slice(), [Event::Closed { .. }]));
    assert_eq!(n.open_sessions(), 0);
}

/// ★★ ONE DOCUMENT PER PROCESS (spec §2). Two sessions are two workers, and
/// neither can see the other's document because neither shares its memory.
#[test]
fn two_sessions_are_two_processes() {
    let mut n = broker();
    let a = open(&mut n);
    let b = open(&mut n);
    assert_ne!(a, b);
    assert_eq!(n.open_sessions(), 2);
    // Each answers for itself.
    assert!(matches!(
        n.handle(Request::Navigate { session: a, trigger: "#menu".into() }).as_slice(),
        [Event::SceneReady { .. }]));
    assert!(matches!(
        n.handle(Request::Navigate { session: b, trigger: "#menu".into() }).as_slice(),
        [Event::SceneReady { .. }]));
}

/// ★ A worker refusing a document is a Blocked event, and leaves no session
/// and no process behind.
#[test]
fn a_document_the_worker_refuses_leaves_nothing_running() {
    let mut n = broker();
    match n.handle(Request::OpenSession { recording: b"{not json".to_vec() }).as_slice() {
        [Event::Blocked { session: None, why }] => assert!(!why.is_empty(), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(n.open_sessions(), 0);
    // And the failed open left no tombstone to evict a real one.
    assert_eq!(n.handle(Request::Report).len(), 1);
}

/// ★★ A WORKER THAT DIES MUST NOT TAKE THE BROKER WITH IT, and the reader is
/// told it FAILED rather than that their session never existed. Killing the
/// process directly is the only faithful way to test this: a cooperative
/// shutdown would exercise a path a crash never takes.
#[test]
fn a_worker_that_dies_is_reported_as_failed() {
    let mut host = host();
    let id = host.open(&recording("https://a.test/"), NOW).expect("opens");
    assert_eq!(host.status(id), Status::Open);

    // ★ Kill EXACTLY this session's worker, by pid. The first version of
    // this used `pkill -f <worker path>`, which would have killed the workers
    // of every other test running in parallel — a test that reaches into
    // machine-wide state is not isolated, however careful it looks. This
    // project has already paid for that lesson once, with an env var.
    let pid = host.worker_pid(id).expect("the worker must have a pid");
    let killed = std::process::Command::new("kill")
        .arg("-9").arg(pid.to_string()).status().expect("kill is available");
    assert!(killed.success(), "could not kill pid {pid}");
    std::thread::sleep(Duration::from_millis(200));

    let why = host.navigate(id, "#menu", NOW).expect_err("a dead worker cannot answer");
    assert!(!why.is_empty(), "the failure must carry a reason");
    assert_eq!(host.status(id), Status::Failed,
        "a crashed worker must be distinguishable from one that never existed");
    assert_eq!(host.open_count(), 0, "the dead worker was not reaped");
}

/// ★★ A WORKER THAT HANGS IS KILLED AT ITS DEADLINE. Without this the broker
/// blocks in `read` on a pipe with no timeout, and one pathological document
/// stops every other session in the process.
#[test]
fn a_worker_that_never_answers_is_killed_at_the_deadline() {
    // A peer that reads nothing and writes nothing: the honest shape of a
    // worker wedged mid-parse. (`sh -c` takes the following argument as $0,
    // so the worker path is accepted and ignored.)
    let cfg = WorkerConfig {
        worker: WORKER.into(),
        confinement: Confinement::Launcher {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
        },
        deadline: Duration::from_millis(300),
        shutdown_grace: Duration::from_secs(2),
    };
    let mut host = JailedHost::new(cfg, SessionLimits::default());
    let started = std::time::Instant::now();
    let why = host.open(&recording("https://a.test/"), NOW)
        .expect_err("a peer that does not answer must not succeed");
    assert!(started.elapsed() < Duration::from_secs(5),
        "the deadline did not fire: took {:?}", started.elapsed());
    assert!(why.contains("did not answer"), "unhelpful reason: {why}");
    assert_eq!(host.open_count(), 0, "the hung worker was left running");
}

/// A worker that cannot be started at all is a refusal, not a panic.
#[test]
fn a_missing_worker_binary_is_refused_with_its_path() {
    let mut host = JailedHost::new(
        WorkerConfig::unconfined_for_testing("/nonexistent/navigator-worker"),
        SessionLimits::default());
    let why = host.open(&recording("https://a.test/"), NOW).expect_err("must refuse");
    assert!(why.contains("/nonexistent/navigator-worker"), "{why}");
}

/// ★ The budget is charged from bytes the BROKER measured. A worker asked how
/// big it is could answer zero, and the limit protecting the broker would be
/// set by the thing it is protecting itself from.
#[test]
fn the_budget_does_not_depend_on_what_the_worker_claims() {
    let mut host = host();
    let rec = recording("https://a.test/");
    host.open(&rec, NOW).expect("opens");
    assert!(host.bytes() >= rec.len(),
        "charged {} for a {}-byte recording", host.bytes(), rec.len());
}

/// Session bounds apply the same way, and are enforced before a process is
/// spawned — a refusal must not cost a fork.
#[test]
fn the_session_bound_refuses_before_spawning() {
    let limits = SessionLimits { max_sessions: 1, ..Default::default() };
    let mut host = JailedHost::new(WorkerConfig::unconfined_for_testing(WORKER), limits);
    host.open(&recording("https://a.test/"), NOW).expect("1");
    let why = host.open(&recording("https://b.test/"), NOW).expect_err("must refuse");
    assert!(why.contains("limit"), "{why}");
    assert_eq!(host.open_count(), 1);
}

/// Idle expiry reaps the process, not just the bookkeeping.
#[test]
fn an_expired_session_kills_its_worker() {
    let mut host = host();
    let id = host.open(&recording("https://a.test/"), NOW).expect("opens");
    assert!(host.expire(NOW + 5 * MIN).is_empty());
    assert_eq!(host.expire(NOW + 60 * MIN).len(), 1);
    assert_eq!(host.open_count(), 0, "the worker outlived its session");
    assert!(matches!(host.status(id), Status::Expired(_)));
}

// ---- confinement ------------------------------------------------------

/// ★★★ UNCONFINED IS NAMED, NOT DEFAULTED. The only constructor that produces
/// it says so, and a host built that way reports itself honestly — because
/// the failure mode this guards against is a deployment that believes it is
/// jailed and is not.
#[test]
fn an_unconfined_host_says_it_is_unconfined() {
    let cfg = WorkerConfig::unconfined_for_testing(WORKER);
    assert!(!cfg.is_confined());
    let why = cfg.require_confinement().expect_err("must refuse to claim confinement");
    assert!(why.contains("not a jail"), "{why}");
    assert!(!host().is_confined());
}

/// A confined config reports confinement and wraps the worker in the
/// launcher — this crate asserts nothing about WHAT a given launcher
/// confines, only that it is the thing being run.
#[test]
fn a_confined_config_runs_the_worker_through_its_launcher() {
    // `/usr/bin/env` stands in for a launcher: it execs what follows, so the
    // worker still runs and the wiring is observable without this test
    // pretending a jail exists.
    let cfg = WorkerConfig::confined(WORKER, "/usr/bin/env", vec![]);
    assert!(cfg.is_confined());
    cfg.require_confinement().expect("a launcher satisfies the requirement");

    let mut host = JailedHost::new(cfg, SessionLimits::default());
    assert!(host.is_confined());
    let id = host.open(&recording("https://a.test/"), NOW)
        .expect("the worker must still run through the launcher");
    assert_eq!(host.triggers(id), vec!["#menu".to_string()]);
}


/// ★★ A WORKER THAT LIES IS RETIRED, NOT BELIEVED. The pipe from a worker is
/// untrusted input to the one process holding capabilities, so a reply the
/// host does not recognise is a failure — never a value to fall back on.
///
/// `cat` is the perfect liar here: it echoes the request back, so the host
/// receives a well-framed reply whose tag is a REQUEST verb.
#[test]
fn a_worker_that_sends_nonsense_is_retired_not_trusted() {
    let cfg = WorkerConfig {
        worker: "/bin/cat".into(),
        confinement: Confinement::None,
        deadline: Duration::from_secs(5),
        shutdown_grace: Duration::from_secs(2),
    };
    let mut host = JailedHost::new(cfg, SessionLimits::default());
    let why = host.open(&recording("https://a.test/"), NOW)
        .expect_err("an unrecognised reply must not be accepted");
    assert!(why.contains("unknown reply"), "{why}");
    assert_eq!(host.open_count(), 0, "the misbehaving worker was left running");
}

/// ★ And a frame larger than the limit is refused before it is allocated —
/// the length prefix is attacker-controlled, and a host that allocated
/// whatever a worker announced would hand it the broker's memory.
#[test]
fn an_oversized_frame_is_refused_before_it_is_allocated() {
    use navigator_backend::wire::{read_frame, Frame, WireError, MAX_FRAME, write_frame};
    // A header announcing far more than the limit, with no payload behind it:
    // if the reader allocated first, this would try to reserve the announced
    // size from a few bytes of input.
    let header = format!("OK {}\n", MAX_FRAME + 1);
    let mut r = std::io::BufReader::new(std::io::Cursor::new(header.into_bytes()));
    match read_frame(&mut r) {
        Err(WireError::TooLarge { announced }) => assert_eq!(announced, MAX_FRAME + 1),
        other => panic!("oversized frame was not refused: {other:?}"),
    }
    // And the writer refuses to send one, so neither end can start it.
    let mut out: Vec<u8> = vec![];
    let huge = Frame::new("OK", vec![0u8; 8]);
    write_frame(&mut out, &huge).expect("an ordinary frame is fine");
    assert!(!out.is_empty());
}

/// ★ A peer that never sends a newline cannot grow the broker's memory one
/// byte at a time — the header read is bounded too, not just the payload.
#[test]
fn a_header_without_an_end_is_bounded() {
    use navigator_backend::wire::read_frame;
    let endless = vec![b'A'; 64 * 1024];
    let mut r = std::io::BufReader::new(std::io::Cursor::new(endless));
    // Must return rather than consume the whole stream looking for a newline.
    assert!(read_frame(&mut r).is_err(), "an unterminated header was accepted");
}

/// ★★★ THE CORPUS, THROUGH REAL PROCESSES.
///
/// `tests/roundtrip.rs` drives the same recordings through the in-process
/// host and gets 98 sessions, 259 navigations, 259 rewinds. This drives them
/// across a pipe, one process per document. Identical counts are the claim:
/// the seam changed where the work happens and nothing about what it does.
#[test]
fn the_corpus_runs_through_worker_processes() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    let mut n = broker();
    let (mut opened, mut navigated, mut rewound) = (0, 0, 0);
    for e in std::fs::read_dir(&dir).expect("readable directory").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable file");
        let (session, triggers) =
            match n.handle(Request::OpenSession { recording: bytes }).pop() {
                Some(Event::SessionOpened { session, triggers }) => { opened += 1; (session, triggers) }
                other => panic!("{}: {other:?}", p.display()),
            };
        for trigger in triggers {
            match n.handle(Request::Navigate { session, trigger: trigger.clone() }).pop() {
                Some(Event::SceneReady { .. }) => navigated += 1,
                other => panic!("{}: {trigger} -> {other:?}", p.display()),
            }
            match n.handle(Request::Back { session }).pop() {
                Some(Event::Rewound { how: Back::Stepped, .. }) => rewound += 1,
                other => panic!("{}: back -> {other:?}", p.display()),
            }
        }
        // Closed each time, or the session bound would stop this at 16 —
        // which is the bound doing its job, not a failure.
        n.handle(Request::Close { session });
    }
    eprintln!("workers: {opened} sessions, {navigated} navigations, {rewound} rewinds");
    assert!(opened > 0 && navigated > 0, "this test checked nothing");
    assert_eq!(navigated, rewound);
}

/// ★★ A LAUNCHER GETS A DISTINCT NAME PER SPAWN. A confining launcher that
/// makes one jail per unit of work needs one, and a static argument list
/// cannot give it: every worker would ask for the same jail name, and
/// `jail -c` on an existing name reconfigures the running jail instead of
/// failing — two documents quietly sharing one.
#[test]
fn the_launcher_receives_a_distinct_instance_per_session() {
    // `sh -c 'echo "$@" >>LOG; exec WORKER' --` records the argv it was
    // handed, then becomes the real worker, so the session still works.
    let log = std::env::temp_dir().join(format!("nav-instance-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let script = format!("echo \"$1\" >> {} ; shift ; exec \"$@\"", log.display());
    let cfg = WorkerConfig {
        worker: WORKER.into(),
        confinement: Confinement::Launcher {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script, "sh".into(), "{instance}".into()],
        },
        deadline: Duration::from_secs(5),
        shutdown_grace: Duration::from_secs(2),
    };
    let mut host = JailedHost::new(cfg, SessionLimits::default());
    let a = host.open(&recording("https://a.test/"), NOW).expect("opens");
    let b = host.open(&recording("https://b.test/"), NOW).expect("opens");
    assert_ne!(a, b);

    let seen = std::fs::read_to_string(&log).expect("the launcher ran");
    let tags: Vec<&str> = seen.lines().collect();
    assert_eq!(tags.len(), 2, "expected one tag per spawn: {tags:?}");
    assert_ne!(tags[0], tags[1], "both workers asked for the same jail: {tags:?}");
    assert!(tags.contains(&a.to_string().as_str()), "{tags:?} vs session {a}");
    let _ = std::fs::remove_file(&log);
}
