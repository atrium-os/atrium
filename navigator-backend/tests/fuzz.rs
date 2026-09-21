//! ★★★ A FUZZER THAT RUNS IS NOT A FUZZER THAT TESTS.
//!
//! This project has already been lied to by an execution count: 27.8M execs at
//! coverage 2, a harness that never reached the code it was pointed at. So
//! this one does not report how many inputs it tried. It reports what it
//! REACHED, and **fails if the set is short** — every rejection the validator
//! can emit and every note it can raise must have been produced by a mutated
//! input, or the run proved nothing about that path.
//!
//! That assertion is load-bearing in both directions:
//!
//!   - A variant the fuzzer never reaches is either dead code or a gap in the
//!     generator. Both are findings; neither should pass quietly.
//!   - A variant that stops being reachable after a refactor fails here
//!     rather than silently narrowing the fuzzer's coverage.
//!
//! ★ Deterministic, seeded, and dependency-free, in the spirit of
//! `scripts/core-fuzz.sh`'s REPLAY half: the value of a fuzz run you cannot
//! re-run is a story, not a test. A failing input is printed as bytes so it
//! can be pasted straight into a regression test.
//!
//! What is under test is everything that touches hostile input: `ingest`
//! (untrusted JSON), `Document::accept` (untrusted HTML), `applied` and the
//! undo path (untrusted effects), and `wire::read_frame` (a length prefix
//! written by a worker that is jailed precisely because it may be
//! compromised).

use navigator_backend::document::Document;
use navigator_backend::{ingest, Effect, Limits, Note, Reject};
use std::collections::BTreeSet;

/// xorshift64*, so a failure is reproducible from its seed alone.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
    fn byte(&mut self) -> u8 { (self.next() >> 24) as u8 }
}

fn seed_corpus() -> Vec<String> {
    vec![
        // A minimal valid recording.
        r##"{"format":"atrium-navigator-recording/2","url":"https://a.test/","tier":1,
             "tier_reason":"","measurements":{},"transitions":[],
             "document":"<html><body>hi</body></html>"}"##.into(),
        // One with every effect kind, so mutations start near the interesting code.
        r##"{"format":"atrium-navigator-recording/2","url":"https://b.test/","tier":2,
             "tier_reason":"x","measurements":{"elements":3},
             "transitions":[{"trigger":"#t","event":"click","anchored":true,
               "attribute_only":true,
               "effects":[{"kind":"attribute","target":"#m","name":"class","from":"a","to":"b"},
                          {"kind":"insert","parent":"#m","index":0,"html":"<i>x</i>"},
                          {"kind":"remove","target":"#m"},
                          {"kind":"remove-range","parent":"#m","index":0,"count":1},
                          {"kind":"truncated","dropped":3},
                          {"kind":"a-kind-from-the-future","what":1}]}],
             "document":"<html><body><button id=\"t\">t</button><nav id=\"m\" class=\"a\"><i>y</i></nav></body></html>"}"##.into(),
        // Version 1, still accepted.
        r##"{"format":"atrium-navigator-recording/1","url":"","tier":1,"tier_reason":"",
             "measurements":{},"transitions":[],"document":"<p>v1</p>"}"##.into(),
        // ★ One transition that CLEANLY APPLIES and cleanly undoes. Without
        // it the effect paths were entered only by accident: the seed above
        // has effects that legitimately fail (it removes #m and then operates
        // on it), and apply is all-or-nothing, so the whole transition is
        // refused and the undo path is never reached at all.
        r##"{"format":"atrium-navigator-recording/2","url":"https://c.test/","tier":2,
             "tier_reason":"","measurements":{},
             "transitions":[{"trigger":"#t","event":"click","anchored":true,
               "effects":[{"kind":"attribute","target":"#m","name":"class","from":"a","to":"b"}]}],
             "document":"<html><body><button id=\"t\">t</button><nav id=\"m\" class=\"a\"></nav></body></html>"}"##.into(),
    ]
}

/// A recording whose transition table exceeds `max_transitions`.
///
/// ★ Built rather than mutated, because byte-level mutation cannot produce
/// 4097 well-formed transitions — the reach assertion demanded the truncation
/// path and the generator could not get there. That is the assertion working:
/// the honest response is to teach the generator, not to stop asking.
fn oversized_transition_table() -> Vec<u8> {
    let t = r##"{"trigger":"#t","event":"click","anchored":true,"effects":[]}"##;
    let many: Vec<&str> = std::iter::repeat(t).take(5000).collect();
    format!(
        r##"{{"format":"atrium-navigator-recording/2","url":"https://d.test/","tier":2,
             "tier_reason":"","measurements":{{}},"transitions":[{}],
             "document":"<html><body><button id=\"t\">t</button></body></html>"}}"##,
        many.join(","))
        .into_bytes()
}

/// Mutations chosen to hit the shapes a validator actually gets wrong:
/// truncation mid-structure, corrupted numbers, oversized fields, deep
/// nesting, and bytes that are not UTF-8 at all.
fn mutate(rng: &mut Rng, src: &str) -> Vec<u8> {
    let mut b = src.as_bytes().to_vec();
    match rng.below(12) {
        // ★ WHOLE-VALUE REPLACEMENT, added because the reach assertion caught
        // its absence: every seed is a JSON object, and byte-level mutation
        // essentially never turns one into a valid non-object. `NotAnObject`
        // was unreachable — not dead code, a blind generator. This is what
        // the assertion is for.
        10 => return b"[1,2,3]".to_vec(),
        11 => return b"\"just a string\"".to_vec(),
        0 => { if !b.is_empty() { let i = rng.below(b.len()); b[i] = rng.byte(); } }
        1 => { let i = rng.below(b.len() + 1); b.truncate(i); }
        2 => { let i = rng.below(b.len() + 1); b.splice(i..i, b"9999999999999999999".iter().copied()); }
        3 => { let i = rng.below(b.len() + 1); b.splice(i..i, b"-1".iter().copied()); }
        4 => { let i = rng.below(b.len() + 1);
               b.splice(i..i, std::iter::repeat(b'[').take(2000)); }
        5 => { let i = rng.below(b.len() + 1);
               b.splice(i..i, std::iter::repeat(b'<').take(4000)); }
        6 => { let i = rng.below(b.len() + 1); b.splice(i..i, [0xff, 0xfe, 0x00].into_iter()); }
        7 => { // oversize a string field
               let i = rng.below(b.len() + 1);
               b.splice(i..i, std::iter::repeat(b'A').take(200_000)); }
        8 => { // swap two chunks
               if b.len() > 8 { let i = rng.below(b.len() / 2); let j = b.len() / 2 + rng.below(b.len() / 2);
                               b.swap(i, j); } }
        _ => { // nested element storm, aimed at the depth ceiling
               let i = rng.below(b.len() + 1);
               let deep: Vec<u8> = "<div>".repeat(400).into_bytes();
               b.splice(i..i, deep); }
    }
    b
}

fn reject_name(r: &Reject) -> &'static str {
    match r {
        Reject::NotJson(_) => "NotJson",
        Reject::NotAnObject => "NotAnObject",
        Reject::WrongFormat { .. } => "WrongFormat",
        Reject::MissingField(_) => "MissingField",
        Reject::BadField { .. } => "BadField",
        Reject::TooLarge { .. } => "TooLarge",
    }
}

fn note_name(n: &Note) -> &'static str {
    match n {
        Note::DerivedFieldDisagreed { .. } => "DerivedFieldDisagreed",
        Note::UnknownEffectKind { .. } => "UnknownEffectKind",
        Note::TransitionsTruncated { .. } => "TransitionsTruncated",
    }
}

/// ★ Everything hostile in one pass, with reach asserted at the end.
#[test]
fn ingesting_mutated_recordings_never_panics_and_reaches_every_refusal() {
    let seeds = seed_corpus();
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut rejects: BTreeSet<&'static str> = BTreeSet::new();
    let mut notes: BTreeSet<&'static str> = BTreeSet::new();
    let mut accepted = 0usize;
    let mut applied = 0usize;

    let limits = Limits::default();
    for round in 0..4000usize {
        let src = &seeds[rng.below(seeds.len())];
        // ★ Every so often, feed a PRISTINE seed. Without it the apply and
        // undo paths are reached only by luck — a mutated document rarely
        // still contains the nodes its transitions name — and a fuzzer that
        // only ever sends garbage tests the front door and nothing behind it.
        let input = if round % 101 == 0 { oversized_transition_table() }
                    else if round % 7 == 0 { src.as_bytes().to_vec() }
                    else { mutate(&mut rng, src) };

        // ★ catch_unwind so ONE panic does not end the run: the point is to
        // learn everything this seed reaches, and the failing bytes are
        // printed so the case becomes a regression test rather than a rerun.
        let bytes = input.clone();
        let out = std::panic::catch_unwind(move || ingest(&bytes, &Limits::default()));
        let Ok(result) = out else {
            panic!("PANIC in ingest, round {round}\ninput ({} bytes): {:?}",
                   input.len(), String::from_utf8_lossy(&input));
        };

        match result {
            Err(r) => { rejects.insert(reject_name(&r)); }
            Ok(rec) => {
                accepted += 1;
                for n in &rec.notes { notes.insert(note_name(n)); }

                // The document is untrusted HTML; accept() may refuse it.
                let doc = {
                    let d = rec.document.clone();
                    match std::panic::catch_unwind(move || Document::accept(&d)) {
                        Ok(v) => v,
                        Err(_) => panic!("PANIC in Document::accept, round {round}\n\
                                          document: {:?}", rec.document),
                    }
                };
                let Ok(doc) = doc else { continue };

                // And the effects are untrusted: apply, then undo.
                for t in rec.transitions.iter().take(4) {
                    let t = t.clone();
                    let d = Document::parse(&rec.document);
                    let r = std::panic::catch_unwind(move || {
                        d.applied_with_undo(&t).map(|(next, undo)| {
                            let _ = next.undone(&undo);
                        })
                    });
                    match r {
                        Ok(Ok(())) => applied += 1,
                        Ok(Err(_)) => {}
                        Err(_) => panic!("PANIC applying a transition, round {round}\n\
                                          trigger {:?}", t_debug(&rec.transitions)),
                    }
                }
                let _ = doc;
            }
        }
        let _ = limits;
    }

    eprintln!("fuzz: {accepted} accepted, {applied} transitions applied");
    eprintln!("fuzz: rejections reached: {rejects:?}");
    eprintln!("fuzz: notes reached:      {notes:?}");

    // ★★ THE ASSERTION THAT MAKES THIS A TEST. Not "it ran" — what it
    // touched. A variant missing here is dead code or a blind generator.
    let want_rejects = ["NotJson", "NotAnObject", "WrongFormat", "MissingField",
                        "BadField", "TooLarge"];
    for w in want_rejects {
        assert!(rejects.contains(w),
            "the fuzzer never produced {w}: either the path is unreachable or \
             the generator cannot make an input that reaches it. Reached: {rejects:?}");
    }
    // ★ EVERY note, not just the ones that were easy to reach. Asking only
    // for what the generator already produced would make this assertion a
    // description of the fuzzer rather than a requirement on it.
    for w in ["UnknownEffectKind", "DerivedFieldDisagreed", "TransitionsTruncated"] {
        assert!(notes.contains(w),
            "the fuzzer never raised {w}: unreachable, or the generator cannot \
             build an input that reaches it. Reached: {notes:?}");
    }
    // And it must not have rejected everything: a run that accepted nothing
    // exercised the validator's front door and none of the rest.
    assert!(accepted > 50, "only {accepted} inputs survived ingest — \
        the mutations are too destructive to reach the interesting code");
    assert!(applied > 100, "only {applied} transitions applied and undone — \
        the effect and undo paths were barely entered");
}

fn t_debug(ts: &[navigator_backend::Transition]) -> String {
    ts.iter().map(|t| t.trigger.clone()).collect::<Vec<_>>().join(",")
}

/// ★★ THE PIPE FROM A JAILED WORKER IS UNTRUSTED INPUT to the one process
/// holding capabilities, and its length prefix is attacker-controlled. A host
/// that allocated whatever a worker announced would have handed it the
/// broker's memory.
#[test]
fn frame_reading_survives_hostile_headers() {
    use navigator_backend::wire::{read_frame, MAX_FRAME};
    let mut rng = Rng(0xF00D_0000_0000_0007);

    let hostile: Vec<Vec<u8>> = vec![
        b"OK 99999999999999999999\nx".to_vec(),      // length beyond u64
        format!("OK {}\n", MAX_FRAME + 1).into_bytes(),
        format!("OK {}\n", usize::MAX).into_bytes(),
        b"OK -1\nxxxx".to_vec(),
        b"OK\n".to_vec(),                             // no length at all
        b"OK 10\nshort".to_vec(),                     // truncated payload
        vec![b'A'; 1024],                             // no newline, ever
        b"\n\n\n\n".to_vec(),
        b"OK 0\n".to_vec(),                           // legitimately empty
        vec![0xff, 0xfe, b' ', b'5', b'\n', 1, 2, 3, 4, 5],
    ];

    for (i, case) in hostile.iter().enumerate() {
        let c = case.clone();
        let r = std::panic::catch_unwind(move || {
            let mut r = std::io::BufReader::new(std::io::Cursor::new(c));
            read_frame(&mut r)
        });
        assert!(r.is_ok(), "PANIC reading hostile frame {i}: {:?}", case);
    }

    // And the same under random mutation of a well-formed frame.
    for round in 0..2000 {
        let mut f = b"OK 5\nhello".to_vec();
        let n = rng.below(f.len());
        f[n] = rng.byte();
        let c = f.clone();
        let r = std::panic::catch_unwind(move || {
            let mut r = std::io::BufReader::new(std::io::Cursor::new(c));
            read_frame(&mut r)
        });
        assert!(r.is_ok(), "PANIC on mutated frame, round {round}: {:?}", f);
    }
}

/// ★ A fuzz run whose seeds no longer parse is a fuzz run that tests the
/// mutator and nothing else. Pinned separately so that failure is legible.
#[test]
fn every_seed_is_a_valid_recording() {
    for (i, s) in seed_corpus().iter().enumerate() {
        ingest(s.as_bytes(), &Limits::default())
            .unwrap_or_else(|e| panic!("seed {i} is not a valid recording: {e}"));
    }
}

/// The unknown-effect seed must actually carry one, or the forward-compat
/// assertion above passes on an accident.
#[test]
fn the_seed_corpus_contains_a_future_effect_kind() {
    let r = ingest(seed_corpus()[1].as_bytes(), &Limits::default()).expect("ingests");
    assert!(r.notes.iter().any(|n| matches!(n, Note::UnknownEffectKind { .. })),
        "the seed lost its future effect kind: {:?}", r.notes);
    assert!(r.transitions[0].effects.iter().any(|e| matches!(e, Effect::RemoveRange { .. })),
        "the seed lost its remove-range effect");
}
