//! M0 properties (navigator-backend §4.1a). The cross-machine gate itself runs
//! the corpus on two machines; these pin what one machine can check.

use navigator_render::{fontset::FontSet, nsg, render, Options};

fn nsg_of(md: &str, width_px: i64) -> String {
    let fonts = FontSet::load().expect("pinned font set");
    let (scene, _) = render(md, &fonts, &Options { width_px });
    nsg::write(&scene, &fonts)
}

const SAMPLE: &str = include_str!("golden/sample.md");

/// ★ The golden. A diff here is a LAYOUT CHANGE and must be read as one:
/// regenerate with `NSG_BLESS=1 cargo test` only after looking at it.
#[test]
fn sample_matches_its_golden() {
    let got = nsg_of(SAMPLE, 800);
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/sample.nsg");
    if std::env::var_os("NSG_BLESS").is_some() { std::fs::write(path, &got).unwrap(); }
    let want = std::fs::read_to_string(path).expect("golden exists (NSG_BLESS=1 to create)");
    assert!(got == want, "NSG differs from the golden — a layout change; diff it, then bless");
}

#[test]
fn rendering_is_deterministic() {
    assert_eq!(nsg_of(SAMPLE, 800), nsg_of(SAMPLE, 800));
}

#[test]
fn nsg_is_integers_only_in_a_closed_vocabulary() {
    let out = nsg_of(SAMPLE, 800);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("nsg 0.2"));
    for l in lines {
        let kw = l.split(' ').next().unwrap();
        assert!(["viewport", "font", "clip", "group", "xform", "rect", "run", "link", "shadow", "grad", "image"].contains(&kw),
                "unknown node {l:?}");
        // Everything before the first quoted string is structure: no floats.
        let structure = l.split('"').next().unwrap();
        assert!(!structure.contains('.'), "non-integer in {l:?}");
    }
}

#[test]
fn a_link_has_a_hit_region_over_its_text() {
    let out = nsg_of("see [the docs](https://example.com/d) here", 800);
    let link: Vec<&str> = out.lines().find(|l| l.starts_with("link ")).expect("link node").split(' ').collect();
    let run: Vec<&str> = out.lines().find(|l| l.ends_with("\"the docs\"")).expect("link run").split(' ').collect();
    assert_eq!(link[1], run[4], "hit region starts where the link text does");
    assert!(link[3].parse::<i64>().unwrap() > 0);
}

/// Control: layout must respond to the viewport, or the golden above could be
/// a constant and "deterministic" would mean nothing.
#[test]
fn a_narrower_viewport_wraps_into_more_lines() {
    let para = "word ".repeat(200);
    let lines = |w| nsg_of(&para, w).lines().filter(|l| l.starts_with("run ")).count();
    let (narrow, wide) = (lines(400), lines(1600));
    assert!(narrow > wide && wide >= 1, "narrow {narrow} wide {wide}");
}

/// ★ The conformance number cannot fall silently. The rows matched today are
/// listed here; a golden that stops matching, or a fixture that starts
/// raising a diagnostic, fails this test instead of quietly lowering the
/// number. Raise the list when a row is newly matched (after reviewing its
/// golden with nsg-raster).
#[test]
fn matched_conformance_rows_stay_matched() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance");
    let fonts = FontSet::load().expect("pinned font set");
    let rows = navigator_render::conformance::run(&dir, &fonts, false);
    let matched: Vec<u8> = rows.iter().filter(|r| r.matched).map(|r| r.id).collect();
    assert_eq!(matched, (1..=64).collect::<Vec<u8>>(), "every row in the profile is matched");
    assert_eq!(rows.iter().filter(|r| r.exercised).count(), 64);
}
