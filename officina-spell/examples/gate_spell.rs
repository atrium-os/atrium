//! O3f spell-check gate: (1) every form the dictionary allows checks as
//! right (the reader against its own word list); (2) common misspellings are
//! caught and the intended word is among the suggestions; (3) the misspelled
//! words in this repository's own docs, most frequent first, for a person to
//! read through for false alarms; (4) speed.
//!
//! cargo run --release --example gate_spell [DOCS_DIR]

use std::collections::HashMap;
use std::time::Instant;

use officina_spell::{words, Dictionary, Speller};

const MISSPELLINGS: &[(&str, &str)] = &[
    ("recieve", "receive"), ("seperate", "separate"), ("definately", "definitely"), ("occured", "occurred"), ("untill", "until"),
    ("wierd", "weird"), ("acheive", "achieve"), ("beleive", "believe"), ("calender", "calendar"), ("commited", "committed"),
    ("embarass", "embarrass"), ("enviroment", "environment"), ("goverment", "government"), ("independant", "independent"), ("existance", "existence"),
    ("neccessary", "necessary"), ("noticable", "noticeable"), ("occurence", "occurrence"), ("persistant", "persistent"), ("posession", "possession"),
    ("publically", "publicly"), ("refered", "referred"), ("relevent", "relevant"), ("succesful", "successful"), ("tommorow", "tomorrow"),
    ("truely", "truly"), ("wich", "which"), ("teh", "the"), ("adress", "address"), ("begining", "beginning"),
    ("buisness", "business"), ("concious", "conscious"), ("foriegn", "foreign"), ("grammer", "grammar"), ("happend", "happened"),
    ("knowlege", "knowledge"), ("libary", "library"), ("medecine", "medicine"), ("millenium", "millennium"), ("mispell", "misspell"),
    ("paralel", "parallel"), ("prefered", "preferred"), ("reccomend", "recommend"), ("rythm", "rhythm"), ("sieze", "seize"),
    ("thier", "their"), ("tounge", "tongue"), ("wether", "whether"), ("writting", "writing"), ("yeild", "yield"),
];

fn main() {
    let t = Instant::now();
    let d = Dictionary::en_us();
    println!("loaded {} roots in {:.2?}; unsupported options: {:?}", d.len(), t.elapsed(), d.unsupported());

    let forms = d.forms();
    let t = Instant::now();
    let bad: Vec<&String> = forms.iter().filter(|f| !d.check(f)).collect();
    let per = t.elapsed().as_secs_f64() * 1e6 / forms.len() as f64;
    println!("{} {} forms check right ({} wrong) at {per:.1} µs a word", if bad.is_empty() { "PASS" } else { "FAIL" }, forms.len() - bad.len(), bad.len());
    for b in bad.iter().take(20) {
        println!("  wrong: {b}");
    }

    let mut caught = 0;
    let mut suggested = 0;
    let t = Instant::now();
    for (w, want) in MISSPELLINGS {
        if !d.check(w) {
            caught += 1;
        }
        let s = d.suggest(w);
        if s.iter().any(|x| x == want) {
            suggested += 1;
        } else {
            println!("  no “{want}” for “{w}”: {s:?}");
        }
    }
    let per = t.elapsed().as_secs_f64() * 1e3 / MISSPELLINGS.len() as f64;
    println!("{} {caught} of {} misspellings caught; intended word suggested for {suggested} ({per:.1} ms a suggestion)", if caught == MISSPELLINGS.len() && suggested * 10 >= MISSPELLINGS.len() * 9 { "PASS" } else { "FAIL" }, MISSPELLINGS.len());

    let dir = std::env::args().nth(1).unwrap_or_else(|| "../docs".into());
    let sp = Speller::new(d);
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut total = 0;
    let t = Instant::now();
    let mut stack = vec![std::path::PathBuf::from(&dir)];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "md") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                // Code spans and blocks aren't prose.
                let prose: String = text.split("```").step_by(2).collect::<Vec<_>>().join(" ");
                let prose: String = prose.split('`').step_by(2).collect::<Vec<_>>().join(" ");
                for line in prose.lines() {
                    total += words(line).len();
                    for (s, e) in sp.misspelled(line) {
                        *counts.entry(line[s as usize..e as usize].to_string()).or_default() += 1;
                    }
                }
            }
        }
    }
    let flagged: usize = counts.values().sum();
    println!("docs: {total} words in {:.2?}, {flagged} flagged ({} distinct)", t.elapsed(), counts.len());
    let mut v: Vec<(usize, String)> = counts.into_iter().map(|(w, n)| (n, w)).collect();
    v.sort_by(|a, b| b.cmp(a));
    let top: Vec<String> = v.iter().take(60).map(|(n, w)| format!("{w} {n}")).collect();
    println!("most flagged: {}", top.join(", "));
}
