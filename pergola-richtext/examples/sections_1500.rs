//! O5d gate (officina.md §16.29): sections and columns in a document of about
//! 1,500 pages, with the pinned fonts. Each chapter starts a page in one
//! column; its body runs in two columns, some in three with a line between,
//! each section balanced above the next continuous break. Then: a page of
//! text pasted into a two-column section near the start, that section set to
//! three columns, and a new continuous section break. Each must settle in
//! under 10 s, lay out again only the paragraphs whose column width changed
//! (and the edited ones), and match a fresh pagination.
//!
//! cargo run --release --example sections_1500

use std::rc::Rc;
use std::time::{Duration, Instant};

use officina_text::layout::{LayoutCache, PinnedFonts};
use officina_text::model::{Section, SectionStart, TextDoc};
use officina_text::paginate::Paginator;
use pergola_richtext::{Editor, Pos};

const WORDS: &[&str] = &["the", "layout", "of", "a", "page", "depends", "on", "every", "paragraph", "before", "it", "and", "columns", "fill", "side", "by", "side", "then", "balance", "above", "a", "continuous", "break", "naïve", "café", "sections"];

fn settle(e: &mut Editor) -> Duration {
    let t = Instant::now();
    while !e.paginate(Duration::from_millis(16)) {}
    t.elapsed()
}

fn fresh_matches(e: &Editor) -> bool {
    let mut fresh = Paginator::new(&e.doc);
    let mut cache = LayoutCache::default();
    fresh.run(&e.doc, &mut cache, e.fonts(), usize::MAX);
    fresh.pages() == e.pages.pages()
}

/// The paragraphs of the section `b` is in: from its mark to the next.
fn section_len(d: &TextDoc, b: usize) -> usize {
    let mark = |k: usize| d.para(k).is_some_and(|p| p.props.section.is_some());
    let start = (0..=b).rev().find(|&k| mark(k)).unwrap_or(0);
    (start + 1..d.blocks.len()).find(|&k| mark(k)).unwrap_or(d.blocks.len()) - start
}

fn main() {
    let fonts = Rc::new(PinnedFonts::new().expect("fonts"));
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let h1 = d.styles.by_name("Heading 1").unwrap();
    let target_pages = std::env::var("PAGES").ok().and_then(|s| s.parse().ok()).unwrap_or(1500usize);
    let mut x: u64 = 0x2545_f491;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let mut marks = Vec::new();
    // Narrow columns hold less: about 7.5 body paragraphs a page.
    for i in 0..target_pages * 15 / 2 {
        if i % 60 == 0 {
            marks.push((d.blocks.len(), Section { start: SectionStart::NextPage, ..Section::default() }));
            d.push_para(&format!("Chapter {}", i / 60 + 1), h1, vec![]);
        }
        if i % 20 == 0 {
            let three = (i / 20) % 3 == 2;
            marks.push((d.blocks.len(), Section { start: SectionStart::Continuous, columns: if three { 3 } else { 2 }, gap: 360, rule: three }));
        }
        let n = 40 + rnd(60) as usize;
        let s: Vec<&str> = (0..n).map(|_| WORDS[rnd(WORDS.len() as u64) as usize]).collect();
        d.push_para(&(s.join(" ") + "."), 0, vec![]);
    }
    for (k, s) in &marks {
        d.blocks.update(*k, |b| b.as_para_mut().unwrap().props.section = Some(*s));
    }
    let mut e = Editor::new(d, fonts);
    let t = settle(&mut e);
    let pages = e.pages.pages().len();
    let rules: usize = e.pages.pages().iter().map(|p| p.rules.len()).sum();
    println!("{pages} pages, {} sections, {rules} lines between columns; paginated in {t:.2?}", marks.len());
    let mut ok = pages >= target_pages * 9 / 10 && rules > 0 && fresh_matches(&e);

    // A page of text pasted into the second two-column section of chapter 2.
    let at = marks.iter().filter(|m| m.1.columns == 2).nth(3).unwrap().0 + 5;
    let misses = e.cache.misses;
    e.set_caret(Pos::new(at, 0), false);
    let t = Instant::now();
    let page: Vec<String> = (0..6).map(|_| "pasted words ".repeat(30)).collect();
    e.insert(&page.join("\n"));
    let typed = t.elapsed();
    let t = settle(&mut e) + typed;
    let relaid = e.cache.misses - misses;
    let same = fresh_matches(&e);
    println!("paste a page into a two-column section on page {}: settled in {t:.2?}; {relaid} paragraphs laid out again; matches fresh: {same}", e.pages.page_of(at, 0) + 1);
    ok &= t < Duration::from_secs(10) && relaid <= 8 && same;

    // That section to three columns: its paragraphs, and only they, rewrap.
    let n = section_len(&e.doc, at);
    let misses = e.cache.misses;
    let t = Instant::now();
    assert!(e.set_columns(3, true));
    let typed = t.elapsed();
    let t = settle(&mut e) + typed;
    let relaid = e.cache.misses - misses;
    let same = fresh_matches(&e);
    println!("set that section ({n} paragraphs) to three columns: settled in {t:.2?}; {relaid} paragraphs laid out again; matches fresh: {same}");
    ok &= t < Duration::from_secs(10) && relaid <= n as u64 && same;

    // A continuous break in the middle of it, back to one column after.
    let mid = at + n / 2;
    e.set_caret(Pos::new(mid, 0), false);
    let misses = e.cache.misses;
    let t = Instant::now();
    assert!(e.insert_section_break(SectionStart::Continuous));
    assert!(e.set_columns(1, false));
    let typed = t.elapsed();
    let t = settle(&mut e) + typed;
    let relaid = e.cache.misses - misses;
    let same = fresh_matches(&e);
    println!("continuous break mid-section, one column after: settled in {t:.2?}; {relaid} paragraphs laid out again; matches fresh: {same}");
    ok &= t < Duration::from_secs(10) && relaid <= n as u64 && same;

    // Undo all three: back to the pages before.
    for _ in 0..4 {
        e.undo();
    }
    settle(&mut e);
    let same = fresh_matches(&e);
    println!("undo: {} pages; matches fresh: {same}", e.pages.pages().len());
    ok &= same;
    println!("{}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}
