//! ★ A deterministic mutation fuzzer for the normalizer. Its input is
//! hostile by definition (any page on the web), so two properties must hold
//! for EVERY input, not just the corpus:
//!
//! 1. it returns, within its work budget, without panicking;
//! 2. what it returns is a document the profile accepts (the renderer
//!    refuses nothing), which is the normalizer's whole contract (§6).
//!
//! It runs the lane's own entry point, `normalize_and_measure`: tables
//! arrive measured, and plain `normalize` leaves that to its caller.
//!
//! Fixed seed, so a failure reproduces. `NORMALIZE_FUZZ_ITERS` runs longer and `NORMALIZE_FUZZ_SEED` elsewhere
//! (the default keeps `cargo test` quick); a failing case is printed whole.

use navigator_normalize::{normalize_and_measure, Budget, Inputs};
use navigator_render::fontset::FontSet;
use navigator_render::html::render_html;
use navigator_style::cascade::Env;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
}

/// Fragments that steer mutations into the interesting code: every
/// construct the normalizer rewrites, half-written.
const DICT: &[&str] = &[
    "{", "}", ";", ":", "(", ")", "[", "]", ",", "/", "!important", "\\", "\"", "'", "*", ">", "+", "~",
    "var(--a", "var(--a, ", "--a:", "--b: var(--a) var(--a);", "calc(", "min(", "max(", "light-dark(",
    ":not(", ":is(", ":has(", ":where(", ":hover", ":focus", ":checked", "::before", ":nth-child(2n+1)",
    "@media (", "@media (min-width:600px)and (max-width:", "@media print{", "@import url(", "@layer a{", "@supports (",
    "@font-face{", "grid-column: 1 / -1;", "grid-column: span 3 / auto;", "grid-template-columns: [a] 1fr [b c] repeat(3, 10px) [d];",
    "grid-column: a-start / b;", "grid-template-areas: \"a b\" \"a c\";", "grid-area: a;", "grid-gap: 3px;",
    "clip: rect(0 0 0 0);", "clip: rect(1px,", "clip-path: inset(50%);", "clip-path: circle(", "position: absolute;",
    "display: contents;", "display: grid;", "display: flex;", "float: left;", "box-sizing: content-box;",
    "font-family: 'Material Icons';", "font: 12px/1 x;", "margin: auto;", "min-width: 1024px;", "mask: url(x);",
    "<style>", "</style>", "<p>", "</p>", "<div class=\"a b\">", "</div>", "<table><tr><td>", "<svg><rect width=\"3\"/></svg>",
    "<span style=\"color: var(--a)\">", "<br>", "&amp;", "<!--", "-->", "<a href=\"#x\">",
];

const SEEDS: &[&str] = &[
    r#"<html><head><style>:root{--a:4px;--b:var(--a)} .g{display:grid;grid-template-columns:[full-start] 1rem [content-start] 1fr [content-end] 1rem [full-end]} .g>*{grid-column:content-start/content-end} p:not(:focus){margin:var(--b) auto} @media (min-width:600px)and (max-width:900px){p{color:#123456}}</style></head><body><div class="g"><p>a</p><p class="x">b</p></div></body></html>"#,
    r#"<html><head><style>.vh{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)} .vh:focus{clip:auto} .i{font-family:'Material Icons'} td{float:left} .c{display:contents}</style></head><body><a class="vh" href="/">skip</a><span class="i">menu</span><table><tr><td>1</td><td class="c"><b>2</b></td></tr></table></body></html>"#,
    r#"<html><head><style>.a{grid-template-areas:"h h" "s m";display:grid} .h{grid-area:h} .s{grid-area:s} .m{grid-area:m} @layer base{p{color:light-dark(#000,#fff)}}</style></head><body><div class="a"><div class="h">h</div><div class="s">s</div><div class="m">m</div></div><p style="margin:1px 2px">p</p></body></html>"#,
];

fn mutate(r: &mut Rng, s: &str) -> String {
    let mut b: Vec<u8> = s.as_bytes().to_vec();
    for _ in 0..1 + r.below(6) {
        let at = r.below(b.len() + 1);
        match r.below(6) {
            0 => { let f = DICT[r.below(DICT.len())]; b.splice(at..at, f.bytes()); }
            1 => { let n = r.below(16).min(b.len() - at.min(b.len())); b.drain(at..at + n); }
            2 => if at < b.len() { b[at] = (r.next() & 0x7f) as u8 },
            3 => { let n = r.below(64).min(b.len().saturating_sub(at)); let piece: Vec<u8> = b[at..at + n].to_vec(); b.splice(at..at, piece); }
            4 => { let f = DICT[r.below(DICT.len())]; let g = DICT[r.below(DICT.len())]; b.splice(at..at, format!("{f}{g}").bytes()); }
            _ => b.truncate(at),
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

#[test]
fn any_input_normalizes_to_a_document_the_profile_accepts() {
    let iters: usize = std::env::var("NORMALIZE_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let fonts = FontSet::load().expect("pinned font set");
    // A small budget, so the fuzzer also walks the over-budget paths.
    let tight = Inputs { budget: Budget { match_steps: 2_000, var_steps: 2_000, var_tokens: 256, ..Budget::default() }, ..Inputs::default() };
    let seed: u64 = std::env::var("NORMALIZE_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut r = Rng(seed | 1);
    let mut corpus: Vec<String> = SEEDS.iter().map(|s| s.to_string()).collect();
    for i in 0..iters {
        let parent = corpus[r.below(corpus.len())].clone();
        let input = mutate(&mut r, &parent);
        let inputs = if i % 3 == 0 { &tight } else { &Inputs::default() };
        let env = Env::default();
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| normalize_and_measure(&input, inputs, &fonts, &env)));
        let (out, _) = match run {
            Ok(x) => x,
            Err(_) => panic!("normalize panicked on iteration {i}; input:\n{input}"),
        };
        let refused: Vec<String> = render_html(&out, &fonts, &Env::default()).diagnostics.iter()
            .map(|d| format!("{}: {}", d.code, d.msg)).collect();
        assert!(refused.is_empty(), "iteration {i}: the profile refused the output {refused:?}\ninput:\n{input}\noutput:\n{out}");
        // Keep mutants that still look like documents, so depth accumulates.
        if out.len() < 20_000 && corpus.len() < 64 { corpus.push(input) }
    }
}
