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
