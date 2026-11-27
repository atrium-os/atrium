//! The Write real-document sweep (docs/spec/officina.md §16.169): every
//! `.docx`, `.docm`, `.odt` and `.doc` under the given directories is read by
//! our reader, written as `.docx` and as `.odt`, and read back.
//!
//! cargo run --release --example corpus_write -- DIR [DIR ...] > report.tsv
//!
//! Per file it records: read or refused (with the reason), a panic (a failure
//! of the gate), the time, a count of what the document holds (paragraphs,
//! table cells, pictures, shapes, notes, comments, tracked edits, sections,
//! headers and footers, list items, fields, links, styled and formatted
//! characters), and for each written format whether the copy read back holds
//! the same paragraphs' text and the same counts, or what differs first. At
//! the end (on stderr) it sums the readers' fidelity notes over the files, so
//! what the readers drop is ranked by how many documents drop it. The corpus
//! itself is test data and stays out of the repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use officina_convert::{doc, docx, odt};
use officina_text::model::TextDoc;

/// How long one file may take, read and both round trips.
const LIMIT: Duration = Duration::from_secs(120);

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else if matches!(ext(&p).as_str(), "docx" | "docm" | "odt" | "doc") {
            out.push(p);
        }
    }
}

fn ext(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn read(kind: &str, data: &[u8]) -> Result<(TextDoc, Vec<String>), String> {
    match kind {
        "docx" | "docm" => docx::read(data).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string()),
        "odt" => odt::read(data).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string()),
        "doc" => doc::read(data, None).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string()),
        _ => Err("unknown kind".into()),
    }
}

/// What a document holds, counted; two readings of one document should
/// count the same.
#[derive(Debug, Default, Clone, PartialEq)]
struct Counts {
    paras: usize,
    cells: usize,
    pictures: usize,
    shapes: usize,
    floats: usize,
    notes: usize,
    comments: usize,
    edits: usize,
    sections: usize,
    margins: usize,
    list_items: usize,
    fields: usize,
    links: usize,
    toc: usize,
    tabs: usize,
    bold: usize,
    italic: usize,
    underline: usize,
    sized: usize,
    coloured: usize,
    font: usize,
    highlight: usize,
    caps: usize,
    hidden: usize,
    dstrike: usize,
    spaced_letters: usize,
    scaled: usize,
    effects: usize,
    page_breaks: usize,
    styled: usize,
    indented: usize,
    spaced: usize,
}

impl Counts {
    fn of(d: &TextDoc) -> Counts {
        let mut c = Counts::default();
        for b in 0..d.blocks.len() {
            let Some(p) = d.para(b) else { continue };
            // A paragraph with page breaks inside counts as its pieces, as
            // ODF (which has no such break) splits it.
            let k = p.text.split('\u{000C}').count();
            c.paras += p.text.split('\u{000C}').filter(|t| !t.trim().is_empty()).count();
            let pr = &p.props;
            c.cells += k * pr.cell.is_some() as usize;
            c.sections += pr.section.is_some() as usize;
            c.list_items += k * matches!(pr.list, Some(Some(_))) as usize;
            c.toc += k * pr.toc.is_some() as usize;
            c.tabs += k * pr.tabs.as_ref().is_some_and(|t| !t.is_empty()) as usize;
            c.styled += p.text.split('\u{000C}').filter(|t| p.style != 0 && !t.trim().is_empty()).count();
            c.indented += k * (pr.indent_start.unwrap_or(0) != 0 || pr.first_line.unwrap_or(0) != 0) as usize;
            c.spaced += k * (pr.space_before.unwrap_or(0) != 0 || pr.space_after.unwrap_or(0) != 0 || pr.line_spacing.is_some()) as usize;
            // A page break inside a paragraph, or before one (ODF has only that).
            c.page_breaks += p.text.matches('\u{000C}').count() + (b > 0 && pr.page_break_before == Some(true)) as usize;
            for (s, e, a) in p.spans() {
                let n = p.text[s as usize..e as usize].chars().filter(|ch| !ch.is_whitespace()).count();
                let a = d.attrs.get(a);
                if a.deleted.is_some() {
                    continue;
                }
                if let Some(o) = &a.object {
                    if o.shape {
                        c.shapes += 1;
                    } else {
                        c.pictures += 1;
                    }
                    c.floats += o.float.is_some() as usize;
                }
                c.fields += a.field.is_some() as usize;
                c.links += (a.link.is_some() && n > 0) as usize;
                if n > 0 {
                    c.bold += n * (a.bold == Some(true)) as usize;
                    c.italic += n * (a.italic == Some(true)) as usize;
                    c.underline += n * (a.underline == Some(true)) as usize;
                    c.sized += n * a.size.is_some() as usize;
                    c.coloured += n * a.color.is_some() as usize;
                    c.font += n * a.family.is_some() as usize;
                    c.highlight += n * a.highlight.is_some() as usize;
                    c.caps += n * a.caps.is_some_and(|x| x != officina_text::model::Caps::Normal) as usize;
                    c.hidden += n * (a.hidden == Some(true)) as usize;
                    c.dstrike += n * (a.dstrike == Some(true)) as usize;
                    c.spaced_letters += n * a.spacing.is_some_and(|x| x != 0) as usize;
                    c.scaled += n * a.scale.is_some_and(|x| x != 100) as usize;
                    c.effects += n * a.effects.is_some_and(|x| x != 0) as usize;
                }
            }
        }
        c.notes = d.note_numbers().len();
        c.comments = d.live_comments().len();
        c.edits = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.edits().len())).unwrap_or(0);
        c.margins = d.all_margins().filter(|m| m.paras.iter().any(|p| !p.text.trim().is_empty()) || !m.text.trim().is_empty()).count();
        c
    }

    fn fields(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("paras", self.paras),
            ("cells", self.cells),
            ("pictures", self.pictures),
            ("shapes", self.shapes),
            ("floats", self.floats),
            ("notes", self.notes),
            ("comments", self.comments),
            ("edits", self.edits),
            ("sections", self.sections),
            ("margins", self.margins),
            ("list_items", self.list_items),
            ("fields", self.fields),
            ("links", self.links),
            ("toc", self.toc),
            ("tabs", self.tabs),
            ("bold", self.bold),
            ("italic", self.italic),
            ("underline", self.underline),
            ("sized", self.sized),
            ("coloured", self.coloured),
            ("font", self.font),
            ("highlight", self.highlight),
            ("caps", self.caps),
            ("hidden", self.hidden),
            ("dstrike", self.dstrike),
            ("spaced_letters", self.spaced_letters),
            ("scaled", self.scaled),
            ("effects", self.effects),
            ("page_breaks", self.page_breaks),
            ("styled", self.styled),
            ("indented", self.indented),
            ("spaced", self.spaced),
        ]
    }
}

/// Each paragraph's visible text (tracked deletions left out), empty ones
/// dropped: our writers add an empty paragraph where Word needs one.
fn texts(d: &TextDoc) -> Vec<String> {
    // A page break inside a paragraph counts as the end of one (ODF splits there).
    d.visible_text().into_iter().flat_map(|t| t.split('\u{000C}').map(|x| x.replace('\u{FFFC}', "")).collect::<Vec<_>>()).filter(|t| !t.trim().is_empty()).collect()
}

/// "same", or what differs: the text first, then each count that moved.
fn compare(a: &TextDoc, b: &TextDoc) -> String {
    let (ta, tb) = (texts(a), texts(b));
    let mut out = Vec::new();
    if ta != tb {
        let k = ta.iter().zip(&tb).position(|(x, y)| x != y).unwrap_or(ta.len().min(tb.len()));
        let cut = |s: Option<&String>| s.map(|s| s.chars().take(40).collect::<String>()).unwrap_or_default();
        out.push(format!("text {} vs {} paras, first at {k}: {:?} vs {:?}", ta.len(), tb.len(), cut(ta.get(k)), cut(tb.get(k))));
    }
    let (ca, cb) = (Counts::of(a), Counts::of(b));
    for ((name, x), (_, y)) in ca.fields().into_iter().zip(cb.fields()) {
        if x != y {
            out.push(format!("{name} {x}→{y}"));
        }
    }
    if out.is_empty() { "same".into() } else { out.join("; ").replace(['\t', '\n'], " ") }
}

fn one(path: PathBuf) -> (String, Vec<String>) {
    let kind = ext(&path);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => return (format!("unreadable\t{e}"), Vec::new()),
    };
    let t = Instant::now();
    let (d, notes) = match read(&kind, &data) {
        Ok(x) => x,
        Err(e) => return (format!("refused\t{}\t\t{}", t.elapsed().as_millis(), e.replace(['\t', '\n'], " ")), Vec::new()),
    };
    let read_ms = t.elapsed().as_millis();
    let c = Counts::of(&d);
    let held = c.fields().iter().filter(|f| f.1 > 0).map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join(",");
    let mut row = format!("read\t{read_ms}\t{held}");
    for (to, write) in [("docx", docx::write::<Vec<u8>> as fn(&TextDoc, Vec<u8>) -> std::io::Result<Vec<u8>>), ("odt", odt::write::<Vec<u8>>)] {
        let verdict = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(&d, Vec::new()))) {
            Err(_) => "PANIC writing".to_string(),
            Ok(Err(e)) => format!("not written: {}", e.to_string().replace(['\t', '\n'], " ")),
            Ok(Ok(bytes)) => match std::panic::catch_unwind(|| read(to, &bytes)) {
                Err(_) => "PANIC reading back".to_string(),
                Ok(Err(e)) => format!("not read back: {}", e.replace(['\t', '\n'], " ")),
                Ok(Ok((back, _))) => compare(&d, &back),
            },
        };
        row.push('\t');
        row.push_str(&verdict);
    }
    (row, notes)
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let mut list = Vec::new();
    for d in std::env::args().skip(1) {
        files(Path::new(&d), &mut list);
    }
    eprintln!("{} files", list.len());
    println!("file\tresult\tms\tholds\tdocx\todt");
    // (kind, note) → (files, occurrences)
    let mut sum: BTreeMap<(String, String), (usize, u64)> = BTreeMap::new();
    for p in list {
        let (tx, rx) = mpsc::channel();
        let name = p.display().to_string();
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let r = std::panic::catch_unwind(|| one(p)).unwrap_or_else(|e| {
                    let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    (format!("PANIC\t\t\t{}", msg.replace(['\t', '\n'], " ")), Vec::new())
                });
                let _ = tx.send(r);
            })
            .expect("thread");
        let (r, notes) = rx.recv_timeout(LIMIT).unwrap_or_else(|_| ("TIMEOUT".into(), Vec::new()));
        for l in notes {
            let mut it = l.splitn(4, '\t');
            let (k, n, w) = (it.next().unwrap_or(""), it.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(1), it.next().unwrap_or(""));
            let e = sum.entry((k.to_string(), w.to_string())).or_default();
            e.0 += 1;
            e.1 += n;
        }
        println!("{name}\t{r}");
    }
    let mut v: Vec<_> = sum.into_iter().collect();
    v.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    eprintln!("fidelity notes, by files:");
    for ((k, w), (f, n)) in v {
        eprintln!("{f}\t{n}\t{k}\t{w}");
    }
}
