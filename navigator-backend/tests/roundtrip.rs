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
