//! media_count <recording.json>…: the video elements of recorded documents,
//! by class (fresco-video P3) — `plain` (a file the document lane plays),
//! `script` (fed by the page's script: MSE or a script-set `src`; whether
//! MSE is known only for recordings made since `media` was recorded),
//! `unplayable` (sources of no type the player plays). One TSV line per
//! document with videos, then totals.
use navigator_dom::media::{video_source, VideoSource};
fn main() {
    let (mut docs, mut plain, mut script, mut unplayable, mut mse_docs) = (0, 0, 0, 0, 0);
    let mut types = std::collections::BTreeMap::<String, usize>::new();
    for p in std::env::args().skip(1) {
        let Ok(json) = std::fs::read_to_string(&p) else { continue };
        let Ok(r) = navigator_normalize::recording::parse(&json) else { continue };
        let dom = navigator_dom::parse(&r.document);
        let vids = dom.by_tag_anywhere("video");
        if vids.is_empty() { continue }
        docs += 1;
        // A recording made since the converter records `media` says how many
        // of its scripts use MSE.
        let mse = json.split("\"mse_scripts\": ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|v| v.parse::<usize>().ok());
        if mse.is_some_and(|n| n > 0) { mse_docs += 1 }
        let (mut p0, mut s0, mut u0) = (0, 0, 0);
        for v in &vids {
            match video_source(&dom, *v) {
                VideoSource::Url { mime, .. } => { p0 += 1; *types.entry(mime.split(';').next().unwrap_or("").to_string()).or_default() += 1 }
                VideoSource::Script => s0 += 1,
                VideoSource::Unplayable => u0 += 1,
            }
        }
        plain += p0; script += s0; unplayable += u0;
        println!("{}\t{}\tplain {p0}\tscript {s0}\tunplayable {u0}\tmse_scripts {}", p.rsplit('/').next().unwrap(), vids.len(), mse.map(|n| n.to_string()).unwrap_or("-".into()));
    }
    println!("total\tdocuments {docs}\tvideos {}\tplain {plain}\tscript {script}\tunplayable {unplayable}\tdocuments_with_mse_scripts {mse_docs}\tplain_types {types:?}", plain + script + unplayable);
}
