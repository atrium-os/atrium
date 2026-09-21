//! ★ THE INTERFACE IS ONLY REAL IF BOTH ENDS AGREE. Unit tests over
//! hand-written JSON prove the validator's rules; they prove nothing about
//! whether the converter's actual output satisfies them. This reads
//! recordings emitted by `navigator-prerender` from disk — set
//! `NAVIGATOR_RECORDINGS` to a directory of them — and ingests every one.
//!
//! Skipped when the directory is absent, and it SAYS so rather than passing
//! quietly: a test that silently checks nothing is the failure mode this
//! project keeps rediscovering.

use navigator_backend::{ingest, Limits};

#[test]
fn every_emitted_recording_ingests() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    let mut n = 0;
    let mut notes = 0;
    let mut failures = vec![];
    let entries = std::fs::read_dir(&dir).expect("readable directory");
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable file");
        n += 1;
        match ingest(&bytes, &Limits::default()) {
            Ok(r) => {
                notes += r.notes.len();
                // A recording the converter produced must carry a document
                // and a coherent tier; anything else means the two ends have
                // drifted apart.
                assert!(!r.document.is_empty(), "{}: empty document", p.display());
                for t in &r.transitions {
                    assert!(!t.trigger.is_empty(), "{}: empty trigger", p.display());
                }
            }
            Err(why) => failures.push(format!("{}: {why}", p.display())),
        }
    }
    assert!(n > 0, "NAVIGATOR_RECORDINGS was set but contained no .json files");
    assert!(failures.is_empty(), "{} of {n} recordings refused:\n  {}",
        failures.len(), failures.join("\n  "));
    eprintln!("ingested {n} real recordings, {notes} notes");
}

/// ★★ THE POINT OF THE SHARED CRATE, TESTED. The converter serialized this
/// document; the backend re-parses it with the SAME parser. If a trigger the
/// converter recorded cannot be resolved here, the two ends have drifted —
/// which is the failure a shared parser exists to prevent, and it would
/// otherwise surface as menus that silently do nothing.
#[test]
fn triggers_recorded_by_the_converter_resolve_in_the_backend() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    use navigator_backend::document::Document;
    let (mut total, mut resolved, mut docs) = (0usize, 0usize, 0usize);
    let mut worst: Vec<String> = vec![];
    for e in std::fs::read_dir(&dir).expect("readable").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        if r.transitions.is_empty() { continue }
        docs += 1;
        let doc = Document::parse(&r.document);
        let mut missed = 0;
        for t in &r.transitions {
            total += 1;
            if doc.resolve(&t.trigger).is_some() { resolved += 1 } else { missed += 1 }
        }
        if missed > 0 {
            worst.push(format!("{}: {missed}/{} unresolved",
                p.file_name().unwrap().to_string_lossy(), r.transitions.len()));
        }
    }
    eprintln!("resolved {resolved}/{total} triggers across {docs} documents");
    for w in worst.iter().take(5) { eprintln!("   {w}"); }
    assert!(total > 0, "no transitions to resolve");
    // Not 100%: a recording demoted to tier 1 keeps only id-addressed
    // triggers, and positional paths were recorded against the tier 2 tree.
    let pct = resolved * 100 / total;
    assert!(pct >= 90, "only {pct}% of triggers resolve — the ends have drifted");
}

/// ★★ THE ACCEPTANCE ARM AT CORPUS SCALE.
///
/// `tests/profile.rs` proves the backend refuses documents outside the
/// profile. On its own that is the cheaper half of the claim: a validator
/// that refused everything would pass it. This is the half that costs
/// something — every document the converter actually emitted must be one the
/// backend will render.
///
/// A failure here is not "tighten the test". It means either the converter is
/// emitting documents its own renderer refuses, or a ceiling is wrong; three
/// of the profile's ceilings were corrected because a measurement like this
/// one disagreed with the prose.
#[test]
fn every_emitted_document_is_inside_the_profile() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    use navigator_backend::document::Document;
    let mut n = 0;
    let mut refused = vec![];
    for e in std::fs::read_dir(&dir).expect("readable directory").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable file");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        n += 1;
        if let Err(v) = Document::accept(&r.document) {
            let why: Vec<String> = v.iter().map(|v| v.to_string()).collect();
            refused.push(format!("{}: {}", p.display(), why.join(", ")));
        }
    }
    assert!(n > 0, "no recordings found in {dir} — this test checked nothing");
    eprintln!("{n} emitted documents, {} refused by the profile", refused.len());
    assert!(refused.is_empty(),
        "the backend refuses documents its own converter emits:\n  {}",
        refused.join("\n  "));
}

/// ★★ EVERY RECORDED TRANSITION MUST APPLY TO ITS OWN DOCUMENT.
///
/// `tests/apply.rs` proves the refusals fire on hand-built cases. This is the
/// arm that can falsify the design: these transitions were recorded by the
/// converter against exactly these documents, so every precondition must
/// hold, every path must resolve, and the result must stay inside the
/// profile. A failure here means the replay model disagrees with what the
/// converter actually observed — which is a bug in one of them, never a
/// reason to loosen the check.
#[test]
fn every_recorded_transition_applies_to_its_own_document() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    use navigator_backend::document::Document;
    let (mut applied, mut skipped_unanchored, mut skipped_incomplete) = (0, 0, 0);
    let mut failures = vec![];
    for e in std::fs::read_dir(&dir).expect("readable directory").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable file");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let base = Document::accept(&r.document).expect("inside the profile");
        for t in &r.transitions {
            // Unanchored transitions name nodes the page's own scripts made;
            // they are reported by the converter precisely because they
            // cannot be replayed against a tier 1 document.
            if !t.anchored { skipped_unanchored += 1; continue }
            if t.effects.iter().any(|e| matches!(e, navigator_backend::Effect::Truncated { .. })) {
                skipped_incomplete += 1;
                continue;
            }
            match base.applied(t) {
                Ok(_) => applied += 1,
                Err(why) => failures.push(format!("{}: {} -> {why}", p.display(), t.trigger)),
            }
        }
    }
    eprintln!("applied {applied} recorded transitions \
               ({skipped_unanchored} unanchored, {skipped_incomplete} incomplete, skipped)");
    assert!(applied > 0, "no transitions were applied — this test checked nothing");
    assert!(failures.is_empty(),
        "transitions failed against the document they were recorded on:\n  {}",
        failures.join("\n  "));
}

/// ★★★ EVERY REAL TRANSITION MUST REVERSE EXACTLY.
///
/// Apply it, undo it, and the document must serialize to the bytes it had
/// before. This is the claim a reader's "back" depends on, checked against
/// recordings the converter actually produced.
///
/// ★ It once reported a FRACTION — 257 of 264 reversed for free, 7 needed a
/// whole-document snapshot — because the inverse was read out of the
/// recording, which does not carry what a removal destroyed. Deriving the
/// undo from the document instead made the fraction 1. A number that stopped
/// being interesting because the design stopped needing it is worth saying
/// out loud; the alternative was to record more in the file and keep the
/// fallback.
#[test]
fn every_recorded_transition_reverses_exactly() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    use navigator_backend::document::Document;
    let mut reversed = 0usize;
    let mut failures = vec![];
    for e in std::fs::read_dir(&dir).expect("readable directory").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable file");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let base = Document::accept(&r.document).expect("inside the profile");
        let before = base.dom.serialize();
        for t in &r.transitions {
            if !t.anchored { continue }
            let Ok((after, undo)) = base.applied_with_undo(t) else { continue };
            match after.undone(&undo) {
                Ok(back) if back.dom.serialize() == before => reversed += 1,
                Ok(_) => failures.push(format!(
                    "{}: {} undid to a DIFFERENT document", p.display(), t.trigger)),
                Err(why) => failures.push(format!(
                    "{}: {} would not undo: {why}", p.display(), t.trigger)),
            }
        }
    }
    assert!(reversed + failures.len() > 0, "no transitions examined — this checked nothing");
    eprintln!("{reversed}/{} recorded transitions reverse exactly",
              reversed + failures.len());
    assert!(failures.is_empty(), "reversal did not restore the document:\n  {}",
        failures.join("\n  "));
}

/// ★★ THE DEFAULT SESSION BOUND AGAINST REAL DOCUMENTS.
///
/// The bound was derived from the corpus's document sizes, so the corpus is
/// where it has to be checked. Opening the LARGEST recordings first is the
/// adversarial ordering: if a realistic set of heavy documents cannot be held
/// at once, the default is wrong — and a bound that only works on the median
/// is a bound that fails when a reader has several news sites open.
#[test]
fn the_default_session_bound_holds_the_corpus_heaviest_documents() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else {
        eprintln!("SKIPPED: set NAVIGATOR_RECORDINGS to a directory of emitted recordings");
        return;
    };
    use navigator_backend::session::{SessionLimits, Sessions};
    let mut recs: Vec<navigator_backend::Recording> = vec![];
    for e in std::fs::read_dir(&dir).expect("readable directory").flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) { continue }
        let bytes = std::fs::read(&p).expect("readable file");
        if let Ok(r) = ingest(&bytes, &Limits::default()) { recs.push(r) }
    }
    assert!(!recs.is_empty(), "no recordings — this test checked nothing");
    recs.sort_by_key(|r| std::cmp::Reverse(r.document.len()));

    let limits = SessionLimits::default();
    let mut s = Sessions::new(limits);
    let mut opened = 0;
    let mut refused = None;
    for r in recs.iter().take(limits.max_sessions) {
        match s.open(r, 0) {
            Ok(_) => opened += 1,
            Err(why) => { refused = Some(format!("{}: {why}", r.url)); break }
        }
    }
    eprintln!("{opened} of the heaviest documents open at once, {} bytes \
               ({}% of the budget)",
              s.bytes(), s.bytes() * 100 / limits.max_total_bytes);
    assert!(refused.is_none(),
        "the default bound cannot hold {} heavy documents: {}",
        limits.max_sessions, refused.unwrap());
    assert_eq!(opened, limits.max_sessions.min(recs.len()));
}
