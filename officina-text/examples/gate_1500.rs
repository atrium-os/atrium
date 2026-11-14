//! O3a gate (docs/spec/officina.md §7.4, §15): a generated document of about
//! 1,500 pages, shaped with the pinned fonts. Reports full pagination time,
//! time to the first page, and how many pages a mid-document edit relays.
//!
//! cargo run --release --example gate_1500

use std::time::Instant;

use officina_text::layout::{LayoutCache, PinnedFonts};
use officina_text::model::{CharAttrs, TextDoc};
use officina_text::ops::{Fragment, TextOp};
use officina_text::paginate::{Edit, Paginator};
use officina_core::history::Op;

const WORDS: &[&str] = &[
    "the", "layout", "of", "a", "page", "depends", "on", "every", "paragraph", "before", "it", "and", "pagination", "keeps", "checkpoints",
    "so", "that", "an", "edit", "repaginates", "only", "what", "changed", "documents", "with", "thousands", "pages", "should", "feel", "like",
    "letter", "once", "open", "Officina", "writes", "reads", "OpenDocument", "Office", "Open", "XML", "files", "without", "running", "macros",
    "tables", "figures", "references", "headings", "footnotes", "numbers", "in", "is", "to", "for", "from", "by", "naïve", "café", "größer",
];

fn main() {
    let t = Instant::now();
    let fonts = PinnedFonts::new().expect("fonts");
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let h1 = d.styles.by_name("Heading 1").unwrap();
    let h2 = d.styles.by_name("Heading 2").unwrap();
    let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
    let target_pages = std::env::var("PAGES").ok().and_then(|s| s.parse().ok()).unwrap_or(1500usize);
    let mut x: u64 = 0x2545_f491;
    let mut rnd = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    // About five body paragraphs of 60–140 words per page.
    let paras = target_pages * 5;
    for i in 0..paras {
        if i % 40 == 0 {
            d.push_para(&format!("Chapter {}", i / 40 + 1), h1, vec![]);
        } else if i % 10 == 0 {
            d.push_para(&format!("Section {}.{}", i / 40 + 1, (i % 40) / 10), h2, vec![]);
        }
        let n = 60 + rnd(80) as usize;
        let mut s = String::new();
        for k in 0..n {
            if k > 0 {
                s.push(' ');
            }
            s.push_str(WORDS[rnd(WORDS.len() as u64) as usize]);
        }
        s.push('.');
        let b = s.find(' ').unwrap_or(0) as u32;
        d.push_para(&s, 0, vec![(0, bold), (b, 0)]);
    }
    let built = t.elapsed();
    let chars = d.blocks.counts().chars;
    println!("built {} blocks, {} characters in {:.2?}", d.blocks.len(), chars, built);

    // Find (§15: first result under 100 ms, all under 1 s) and word count.
    use officina_text::find::{Query, Search};
    for (what, q) in [("a rare word", Query { text: "größer".into(), ..Default::default() }), ("a common word, whole words", Query { text: "the".into(), whole_word: true, ..Default::default() }), ("absent text", Query { text: "zebra".into(), ..Default::default() })] {
        let t = Instant::now();
        let first = d.find(&q, d.blocks.len() / 2, 0, true);
        let first_t = t.elapsed();
        let t = Instant::now();
        let mut s = Search::new(&q);
        s.step(&d, usize::MAX);
        println!("find {what}: first {:?} in {:.2?}; all {} in {:.2?}", first.map(|m| m.block), first_t, s.matches.len(), t.elapsed());
    }
    let t = Instant::now();
    let c = d.counts();
    println!("word count: {} words, {} characters in {:.2?}", c.words, c.chars, t.elapsed());
    let t = Instant::now();
    let ops = d.replace_all_ops(&Query { text: "café".into(), ..Default::default() }, "coffee shop");
    let n = ops.len();
    let mut h = officina_core::history::History::new();
    h.run(&mut d, "Replace All", ops);
    let replaced = t.elapsed();
    let t = Instant::now();
    h.undo(&mut d);
    println!("replace all: {n} replacements in {:.2?}, one undo in {:.2?}", replaced, t.elapsed());

    let mut cache = LayoutCache::default();
    let mut p = Paginator::new(&d);
    let t = Instant::now();
    p.run(&d, &mut cache, &fonts, 1);
    let first = t.elapsed();
    p.run(&d, &mut cache, &fonts, usize::MAX);
    let full = t.elapsed();
    let pages = p.pages().len();
    println!("first page {:.2?}; full pagination {pages} pages in {:.2?} ({:.0} pages/s)", first, full, pages as f64 / full.as_secs_f64());

    // Repaginating with every paragraph cached (e.g. after a page-setup-free reset).
    let t = Instant::now();
    p.reset(&d);
    p.run(&d, &mut cache, &fonts, usize::MAX);
    println!("repaginate with layouts cached: {:.2?}", t.elapsed());

    // Typing in the middle: one character at a time into a paragraph on page ~750.
    let mid = p.pages()[pages / 2].slices[0].block + 1;
    let mut worst = 0u64;
    let mut total = 0u64;
    let mut worst_t = std::time::Duration::ZERO;
    for k in 0..40u32 {
        let op = TextOp::Splice { block: mid as u32, at: k, del: 0, ins: Fragment { text: "q".into(), runs: vec![(0, 0)] } };
        op.apply(&mut d);
        let t = Instant::now();
        let before = p.laid;
        p.edit(&d, &mut cache, &fonts, Edit { first: mid, last: mid + 1, delta: 0 });
        p.run(&d, &mut cache, &fonts, usize::MAX);
        worst_t = worst_t.max(t.elapsed());
        let n = p.laid - before;
        worst = worst.max(n);
        total += n;
    }
    println!("typing 40 characters on page {}: worst {worst} pages, {:.1} pages per keystroke, worst {:.2?}", pages / 2, total as f64 / 40.0, worst_t);

    // Enter in the middle (block numbers shift).
    let id = d.new_id();
    TextOp::Split { block: mid as u32, at: 10, id, style: 0, props: Default::default() }.apply(&mut d);
    let t = Instant::now();
    let before = p.laid;
    p.edit(&d, &mut cache, &fonts, Edit { first: mid, last: mid + 2, delta: 1 });
    p.run(&d, &mut cache, &fonts, usize::MAX);
    println!("Enter on page {}: {} pages in {:.2?}", pages / 2, p.laid - before, t.elapsed());

    // Inserting a page of text near the start shifts breaks until a paragraph soaks it up.
    let blocks: Vec<_> = (0..7).map(|_| {
        let id = d.new_id();
        officina_text::model::Block::Para(officina_text::model::Para::new(id, &"inserted text ".repeat(60), 0))
    }).collect();
    TextOp::Insert { at: 5, blocks }.apply(&mut d);
    let t = Instant::now();
    let before = p.laid;
    p.edit(&d, &mut cache, &fonts, Edit { first: 5, last: 12, delta: 7 });
    p.run(&d, &mut cache, &fonts, usize::MAX);
    println!("insert a page of text on page 1: {} pages in {:.2?}", p.laid - before, t.elapsed());

    // Check against a fresh pagination.
    let mut fresh = Paginator::new(&d);
    fresh.run(&d, &mut cache, &fonts, usize::MAX);
    assert_eq!(fresh.pages(), p.pages(), "incremental pages match a fresh pagination");
    println!("incremental pages match a fresh pagination ({} pages); layouts cached: {}", p.pages().len(), cache.len());
}
