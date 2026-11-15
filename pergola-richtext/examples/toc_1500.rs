//! O5d gate (officina.md §7.4, §16.28): a table of contents in a document of
//! about 1,500 pages, with the pinned fonts. Inserts it, then inserts a page
//! of text near the start; each time pagination and field resolution (at most
//! three rounds) must settle in under 10 s, every entry must show its
//! heading's page, and only entries whose page changed are laid out again.
//!
//! cargo run --release --example toc_1500

use std::rc::Rc;
use std::time::{Duration, Instant};

use officina_text::fields;
use officina_text::layout::PinnedFonts;
use officina_text::model::TextDoc;
use pergola_richtext::{Editor, Pos};

const WORDS: &[&str] = &["the", "layout", "of", "a", "page", "depends", "on", "every", "paragraph", "before", "it", "and", "pagination", "keeps", "checkpoints", "so", "that", "an", "edit", "repaginates", "only", "what", "changed", "naïve", "café", "contents", "references"];

/// Paginates and resolves fields until settled.
fn settle(e: &mut Editor) -> Duration {
    let t = Instant::now();
    while !e.paginate(Duration::from_millis(16)) {}
    t.elapsed()
}

/// Entries whose shown page is not their heading's.
fn wrong(e: &Editor) -> usize {
    let Some((a, b)) = fields::toc_range(&e.doc, None) else { return usize::MAX };
    (a..b)
        .filter(|&i| {
            let p = e.doc.para(i).unwrap();
            let f = e.doc.fields_in(p)[0].1;
            let at = e.doc.index_of(f.target()).unwrap();
            e.doc.field_text(f) != (e.pages.page_of(at, 0) + 1).to_string()
        })
        .count()
}

fn main() {
    let fonts = Rc::new(PinnedFonts::new().expect("fonts"));
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let h1 = d.styles.by_name("Heading 1").unwrap();
    let h2 = d.styles.by_name("Heading 2").unwrap();
    let target_pages = std::env::var("PAGES").ok().and_then(|s| s.parse().ok()).unwrap_or(1500usize);
    let mut x: u64 = 0x2545_f491;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    d.push_para("A long report", 0, vec![]);
    for i in 0..target_pages * 5 {
        if i % 40 == 0 {
            d.push_para(&format!("Chapter {}", i / 40 + 1), h1, vec![]);
        } else if i % 10 == 0 {
            d.push_para(&format!("Section {}.{}", i / 40 + 1, (i % 40) / 10), h2, vec![]);
        }
        let n = 60 + rnd(80) as usize;
        let s: Vec<&str> = (0..n).map(|_| WORDS[rnd(WORDS.len() as u64) as usize]).collect();
        d.push_para(&(s.join(" ") + "."), 0, vec![]);
    }
    let mut e = Editor::new(d, fonts);
    let t = settle(&mut e);
    println!("{} pages paginated in {t:.2?}", e.pages.pages().len());

    let mut ok = true;
    // Insert the contents after the title.
    e.set_caret(Pos::new(1, 0), false);
    let t = Instant::now();
    assert!(e.insert_toc());
    let built = t.elapsed();
    let t = settle(&mut e);
    let (a, b) = fields::toc_range(&e.doc, None).unwrap();
    let toc_pages = e.pages.page_of(b, 0) - e.pages.page_of(a, 0) + 1;
    let w = wrong(&e);
    println!("insert contents: {} entries over {toc_pages} pages; built in {built:.2?}, settled in {t:.2?}; {w} entries wrong", b - a);
    ok &= t < Duration::from_secs(10) && w == 0;

    // A page of text near the start (just after the contents).
    let before: Vec<String> = (a..b).map(|i| e.doc.field_text(e.doc.fields_in(e.doc.para(i).unwrap())[0].1)).collect();
    e.set_caret(Pos::new(b + 1, 0), false);
    // One action: paste a page of paragraphs.
    let page: Vec<String> = (0..12).map(|_| "inserted words ".repeat(40)).collect();
    let misses = e.cache.misses;
    let t = Instant::now();
    e.insert(&page.join("\n"));
    let typed = t.elapsed();
    let t = settle(&mut e) + typed;
    let (a2, b2) = fields::toc_range(&e.doc, None).unwrap();
    let after: Vec<String> = (a2..b2).map(|i| e.doc.field_text(e.doc.fields_in(e.doc.para(i).unwrap())[0].1)).collect();
    let changed = before.iter().zip(&after).filter(|(x, y)| x != y).count();
    let relaid = e.cache.misses - misses;
    let w = wrong(&e);
    println!("insert a page of text on page {}: settled in {t:.2?}; {changed} of {} entries changed page, {relaid} paragraphs laid out again; {w} entries wrong", e.pages.page_of(b + 1, 0) + 1, after.len());
    // Laid out again: the edited paragraphs, the entries that changed, and
    // nothing else (an entry is one paragraph).
    ok &= t < Duration::from_secs(10) && w == 0 && changed > 0 && relaid <= changed as u64 + 13;

    // Update finds it current; a retitled heading changes one entry.
    ok &= !e.insert_toc();
    let h = e.doc.index_of(e.ref_targets()[3].0).unwrap();
    e.set_caret(Pos::new(h, 0), false);
    e.insert("Renamed ");
    let t = Instant::now();
    let updated = e.insert_toc();
    settle(&mut e);
    println!("rename a heading, Update: {updated} in {:.2?}; entry now “{}”", t.elapsed(), e.doc.para(a2 + 3).unwrap().text.split('\t').next().unwrap());
    ok &= updated && wrong(&e) == 0;
    // The pages match a fresh pagination of the settled document.
    let mut fresh = officina_text::paginate::Paginator::new(&e.doc);
    let mut cache = officina_text::layout::LayoutCache::default();
    fresh.run(&e.doc, &mut cache, e.fonts(), usize::MAX);
    let same = fresh.pages() == e.pages.pages();
    println!("incremental pages match a fresh pagination: {same}");
    ok &= same;
    println!("{}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}
