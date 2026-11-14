//! The O3 real-world corpus run (docs/spec/officina.md §7.6): every `.docx` and
//! `.odt` under the given directories is read by our reader and compared with
//! LibreOffice's reading of the same file, as text.
//!
//! cargo run --release --example corpus_text -- SRC=TXT [SRC=TXT ...]
//!
//! Each `SRC` is a directory of documents; `TXT` holds LibreOffice's text for
//! them, made beforehand with
//! `soffice --headless --convert-to "txt:Text (encoded):UTF8" --outdir TXT SRC/*`.
//! Per file it records: read or refused (a panic is a failure of the gate),
//! time, the fidelity notes, how much of LibreOffice's text we have (word
//! recall) and how much of ours LibreOffice has (precision), and whether our
//! own writer and reader give the text back unchanged. The corpus itself is
//! test data and stays out of the repository.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use officina_convert::{docx, odt};
use officina_text::model::TextDoc;

/// Words for comparison: list labels, bullets and lone punctuation left out
/// (LibreOffice's text has them, our paragraph text doesn't).
fn words(t: &str) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for w in t.split(|c: char| c.is_whitespace() || c == '\u{FFFC}') {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        if w.is_empty() {
            continue;
        }
        let label = w.len() <= 4 && (w.chars().all(|c| c.is_ascii_digit()) || (w.len() == 1 && w.chars().all(|c| c.is_ascii_lowercase())) || ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"].contains(&w.to_ascii_lowercase().as_str()));
        // Outline labels such as 1.2.3 (our paragraph text has no labels).
        let outline = w.contains('.') && w.chars().all(|c| c.is_ascii_digit() || c == '.');
        if label || outline {
            continue;
        }
        *m.entry(w.to_string()).or_default() += 1;
    }
    m
}

/// Our text with each note anchor shown as its number (as LibreOffice's text has it).
fn labelled(d: &TextDoc) -> String {
    let mut out = String::new();
    for (i, b) in d.blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let Some(p) = b.as_para() else { continue };
        for (at, ch) in p.text.char_indices() {
            match d.attrs.get(p.attr_at(at as u32)).note {
                Some(n) if ch == '\u{FFFC}' => out.push_str(&d.note_label(n)),
                _ => out.push(ch),
            }
        }
    }
    out
}

/// The notes' text in number order, then the comments' (what a round trip
/// must keep).
fn notes_text(d: &TextDoc) -> String {
    notes_only(d) + &d.live_comments().iter().filter_map(|&id| d.comments.get(id)).map(|c| format!("\n[{}] {}", c.author, c.text)).collect::<String>()
}

fn notes_only(d: &TextDoc) -> String {
    let mut v: Vec<(String, String)> = d.note_numbers().iter().map(|(&id, &(k, n))| (format!("{k:?}{n:08}"), d.notes.get(id).map(|n| n.paras.iter().map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")).unwrap_or_default())).collect();
    v.sort();
    v.into_iter().map(|x| x.1).collect::<Vec<_>>().join("\n--\n")
}

fn overlap(a: &HashMap<String, usize>, b: &HashMap<String, usize>) -> usize {
    a.iter().map(|(w, n)| (*n).min(*b.get(w).unwrap_or(&0))).sum()
}

struct Row {
    file: String,
    ok: Result<(), String>,
    took: Duration,
    recall: f64,
    precision: f64,
    lo_words: usize,
    roundtrip: bool,
    missing: Vec<String>,
    notes: usize,
    comments: usize,
}

fn read(path: &Path, data: &[u8]) -> Result<(TextDoc, Vec<String>), String> {
    let odt = path.extension().is_some_and(|e| e == "odt");
    let r = std::panic::catch_unwind(|| {
        if odt {
            odt::read(data).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string())
        } else {
            docx::read(data).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string())
        }
    });
    r.unwrap_or_else(|_| Err("PANIC".into()))
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut rows: Vec<Row> = Vec::new();
    let mut notes: BTreeMap<String, usize> = BTreeMap::new();
    for arg in std::env::args().skip(1) {
        let (src, txt) = arg.split_once('=').expect("SRC=TXT");
        let mut files: Vec<PathBuf> = std::fs::read_dir(src).expect("src").flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "docx" || e == "odt")).collect();
        files.sort();
        for p in files {
            let data = std::fs::read(&p).unwrap_or_default();
            let t = Instant::now();
            let got = read(&p, &data);
            let took = t.elapsed();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let lo = std::fs::read_to_string(Path::new(txt).join(Path::new(&name).with_extension("txt"))).ok();
            let mut row = Row { file: format!("{src}/{name}"), ok: Ok(()), took, recall: f64::NAN, precision: f64::NAN, lo_words: 0, roundtrip: false, missing: Vec::new(), notes: 0, comments: 0 };
            match got {
                Err(e) => row.ok = Err(e),
                Ok((doc, ns)) => {
                    for l in ns {
                        let what = l.split('\t').nth(2).unwrap_or("").to_string();
                        *notes.entry(what).or_default() += 1;
                    }
                    let ours = doc.plain_text();
                    row.notes = doc.note_numbers().len();
                    row.comments = doc.live_comments().len();
                    if let Some(lo) = &lo {
                        let ours = labelled(&doc);
                        let (a, b) = (words(lo), words(&ours));
                        let (na, nb) = (a.values().sum::<usize>(), b.values().sum::<usize>());
                        let o = overlap(&a, &b);
                        row.lo_words = na;
                        row.recall = if na == 0 { 1.0 } else { o as f64 / na as f64 };
                        row.precision = if nb == 0 { 1.0 } else { o as f64 / nb as f64 };
                        let mut miss: Vec<(usize, String)> = a.iter().filter_map(|(w, n)| { let d = n.saturating_sub(*b.get(w).unwrap_or(&0)); (d > 0).then(|| (d, w.clone())) }).collect();
                        miss.sort_by(|x, y| y.cmp(x));
                        row.missing = miss.into_iter().take(8).map(|(n, w)| format!("{w}×{n}")).collect();
                    }
                    // Our writer, then our reader: the same text.
                    let odt_file = p.extension().is_some_and(|e| e == "odt");
                    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if odt_file {
                            odt::write(&doc, Vec::new()).ok().and_then(|b| odt::read(&b).ok()).map(|(d, _)| d.plain_text() + &notes_text(&d))
                        } else {
                            docx::write(&doc, Vec::new()).ok().and_then(|b| docx::read(&b).ok()).map(|(d, _)| d.plain_text() + &notes_text(&d))
                        }
                    }));
                    // Empty paragraphs aside: Word needs one after a table that ends the
                    // document or cell, and one between two tables, and our writer adds them.
                    let full = |t: &str| t.split('\n').filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
                    let ours_all = ours.clone() + &notes_text(&doc);
                    row.roundtrip = matches!(again, Ok(Some(t)) if full(&t) == full(&ours_all));
                }
            }
            rows.push(row);
        }
    }
    let n = rows.len();
    let panics: Vec<&Row> = rows.iter().filter(|r| matches!(&r.ok, Err(e) if e == "PANIC")).collect();
    let refused: Vec<&Row> = rows.iter().filter(|r| matches!(&r.ok, Err(e) if e != "PANIC")).collect();
    let compared: Vec<&Row> = rows.iter().filter(|r| r.ok.is_ok() && !r.recall.is_nan() && r.lo_words > 0).collect();
    let mut rec: Vec<f64> = compared.iter().map(|r| r.recall).collect();
    rec.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let total_lo: usize = compared.iter().map(|r| r.lo_words).sum();
    let total_hit: f64 = compared.iter().map(|r| r.recall * r.lo_words as f64).sum();
    let full = compared.iter().filter(|r| r.recall >= 0.99 && r.precision >= 0.99).count();
    let rt = rows.iter().filter(|r| r.ok.is_ok()).filter(|r| r.roundtrip).count();
    let read_ok = rows.iter().filter(|r| r.ok.is_ok()).count();
    let slowest = rows.iter().max_by_key(|r| r.took).map(|r| format!("{} {:.0?}", r.file, r.took)).unwrap_or_default();
    println!("{n} files: {read_ok} read, {} refused, {} panics; slowest {slowest}", refused.len(), panics.len());
    println!("text against LibreOffice on {} files with text: {:.2} % of all its words; median file {:.1} %; {} files ≥ 99 % both ways", compared.len(), 100.0 * total_hit / total_lo.max(1) as f64, 100.0 * rec.get(rec.len() / 2).copied().unwrap_or(0.0), full);
    println!("our writer then our reader keep the text (and notes) of {rt} of {read_ok}");
    println!("{} notes in {} files", rows.iter().map(|r| r.notes).sum::<usize>(), rows.iter().filter(|r| r.notes > 0).count());
    println!("{} comments in {} files", rows.iter().map(|r| r.comments).sum::<usize>(), rows.iter().filter(|r| r.comments > 0).count());
    for r in &panics {
        println!("PANIC {}", r.file);
    }
    for r in &refused {
        println!("refused {}: {}", r.file, r.ok.as_ref().unwrap_err());
    }
    let mut worst: Vec<&&Row> = compared.iter().filter(|r| r.recall < 0.95).collect();
    worst.sort_by(|a, b| (a.recall * a.lo_words as f64 - a.lo_words as f64).partial_cmp(&(b.recall * b.lo_words as f64 - b.lo_words as f64)).unwrap());
    println!("{} files under 95 % recall; the most words missed first:", worst.len());
    for r in worst.iter().take(40) {
        println!("  {:.0} % of {} words, precision {:.0} %: {}  missing {}", 100.0 * r.recall, r.lo_words, 100.0 * r.precision, r.file.rsplit('/').next().unwrap_or(""), r.missing.join(" "));
    }
    let lowp: Vec<&&Row> = compared.iter().filter(|r| r.precision < 0.95).collect();
    println!("{} files under 95 % precision (text we show that LibreOffice doesn't)", lowp.len());
    for r in lowp.iter().take(15) {
        println!("  precision {:.0} %: {}", 100.0 * r.precision, r.file.rsplit('/').next().unwrap_or(""));
    }
    let no_rt: Vec<&Row> = rows.iter().filter(|r| r.ok.is_ok() && !r.roundtrip).collect();
    for r in no_rt.iter().take(15) {
        println!("round trip changed the text: {}", r.file.rsplit('/').next().unwrap_or(""));
    }
    let mut nv: Vec<(usize, String)> = notes.into_iter().map(|(k, v)| (v, k)).collect();
    nv.sort_by(|a, b| b.cmp(a));
    println!("notes (files): {}", nv.iter().take(30).map(|(v, k)| format!("{k} {v}")).collect::<Vec<_>>().join("; "));
}
