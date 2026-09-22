//! The M2 conformance number (profile §5.2; navigator-backend §8.2).
//!
//! Denominator: the profile's 64 property rows. Per row:
//! - **exercised** — the row's fixtures (`conformance/NN-*.html`) declare
//!   EVERY value the row admits: each keyword, and at least one value of each
//!   non-keyword kind. Checked against the grammar table mechanically, never
//!   asserted by hand.
//! - **matched** — exercised, AND every fixture renders with zero
//!   diagnostics and zero unimplemented counts, AND its NSG equals the
//!   reviewed golden beside it (`NN-*.nsg`).
//!
//! Nothing else moves the number: a row the layout reads partially is
//! exercised-but-not-matched, which is the honest state of most rows today.

use crate::fontset::FontSet;
use crate::html::render_html;
use crate::nsg;
use navigator_style::cascade::Env;
use navigator_style::sheet::parse_sheet;
use navigator_style::values::{Specified, A, ROWS, V};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct RowResult {
    pub id: u8,
    pub name: &'static str,
    pub fixtures: Vec<String>,
    /// Admitted values no fixture declares.
    pub missing: Vec<String>,
    pub problems: Vec<String>,
    pub exercised: bool,
    pub matched: bool,
}

/// What a fixture must declare for an atom to count as covered.
fn needs(a: &A) -> Vec<String> {
    match a {
        A::Kw(list) => list.iter().map(|k| format!("`{k}`")).collect(),
        // The profile limits font-weight to the weights the stack ships
        // (§3.7): the shipped set is 400 and 700.
        A::FontWeight => vec!["weight 400".into(), "weight 700".into()],
        other => vec![format!("{other:?}")],
    }
}

fn covers(a: &A, v: &V) -> Vec<String> {
    match (a, v) {
        (A::Kw(list), V::Kw(k)) if list.contains(k) => vec![format!("`{k}`")],
        (A::FontWeight, V::Int(w)) => vec![format!("weight {w}")],
        (A::Len | A::LenNonNeg, V::Len(_) | V::Calc(_)) => vec![format!("{a:?}")],
        (A::Pct | A::PctNonNeg, V::Pct(_)) => vec![format!("{a:?}")],
        (A::Num | A::NumNonNeg | A::Opacity | A::Ratio, V::Num(_)) => vec![format!("{a:?}")],
        (A::Int, V::Int(_)) => vec![format!("{a:?}")],
        (A::Color, V::Color(_)) => vec![format!("{a:?}")],
        (A::Url, V::Url(_)) => vec![format!("{a:?}")],
        (A::Gradient, V::Gradient { .. }) => vec![format!("{a:?}")],
        (A::Shadow, V::Shadow { .. }) => vec![format!("{a:?}")],
        (A::Transform, V::Transform(_)) => vec![format!("{a:?}")],
        (A::TrackList, V::Tracks(_)) => vec![format!("{a:?}")],
        (A::TrackSize, V::Track(_)) => vec![format!("{a:?}")],
        (A::GridLine, V::Line(_)) => vec![format!("{a:?}")],
        (A::Family, V::Family { .. }) => vec![format!("{a:?}")],
        (A::PairLenPct | A::PairLen, V::Pair(..)) => vec![format!("{a:?}")],
        _ => vec![],
    }
}

/// The admitted values of a row that `css` does not declare.
pub fn missing_coverage(row_id: u8, css: &[String]) -> Vec<String> {
    let row = ROWS.iter().find(|r| r.id == row_id).expect("row");
    let mut want: Vec<String> = vec![];
    for p in row.props {
        let (atoms, _, _) = navigator_style::values::grammar(p).expect("grammar");
        for a in atoms { for n in needs(a) { if !want.contains(&n) { want.push(n) } } }
    }
    let mut have: Vec<String> = vec![];
    for sheet in css {
        for rule in parse_sheet(sheet).sheet.rules {
            for d in rule.decls.iter().filter(|d| row.props.contains(&d.prop)) {
                let Specified::Value(v) = &d.value else { continue };
                let (atoms, _, _) = navigator_style::values::grammar(d.prop).expect("grammar");
                for a in atoms { have.extend(covers(a, v)) }
            }
        }
    }
    want.into_iter().filter(|w| !have.contains(w)).collect()
}

fn style_text(html: &str) -> Vec<String> {
    let dom = navigator_dom::parse(html);
    dom.by_tag_anywhere("style").into_iter().map(|h| dom.text_content(h)).collect()
}

pub fn run(dir: &Path, fonts: &FontSet, bless: bool) -> Vec<RowResult> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "html")).collect()).unwrap_or_default();
    files.sort();
    ROWS.iter().map(|row| {
        let prefix = format!("{:02}-", row.id);
        let mine: Vec<&PathBuf> = files.iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix))).collect();
        let mut css = vec![];
        let mut problems = vec![];
        let mut goldens_ok = !mine.is_empty();
        for f in &mine {
            let html = std::fs::read_to_string(f).unwrap_or_default();
            css.extend(style_text(&html));
            let o = render_html(&html, fonts, &Env::default());
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            for d in &o.diagnostics { problems.push(format!("{name}: diagnostic {} {}", d.code, d.msg)) }
            for (k, n) in &o.unimplemented { problems.push(format!("{name}: unimplemented ×{n} {k}")) }
            let out = nsg::write(&o.scene, fonts);
            let golden = f.with_extension("nsg");
            if bless && o.diagnostics.is_empty() && o.unimplemented.is_empty() { let _ = std::fs::write(&golden, &out); }
            match std::fs::read_to_string(&golden) {
                Ok(g) if g == out => {}
                Ok(_) => { goldens_ok = false; problems.push(format!("{name}: NSG differs from its golden")) }
                Err(_) => { goldens_ok = false; problems.push(format!("{name}: no golden")) }
            }
        }
        let missing = if mine.is_empty() { vec!["no fixture".into()] } else { missing_coverage(row.id, &css) };
        let exercised = missing.is_empty();
        let matched = exercised && problems.is_empty() && goldens_ok;
        RowResult { id: row.id, name: row.props[0], fixtures: mine.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect(),
                    missing, problems, exercised, matched }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_needs_every_keyword() {
        let full = "p { text-align: start } p { text-align: end } p { text-align: center } p { text-align: justify }";
        assert!(missing_coverage(39, &[full.into()]).is_empty());
        // Control: drop one keyword and the row is no longer exercised.
        let short = "p { text-align: start } p { text-align: end } p { text-align: center }";
        assert_eq!(missing_coverage(39, &[short.into()]), vec!["`justify`".to_string()]);
    }

    #[test]
    fn coverage_needs_each_value_kind() {
        assert_eq!(missing_coverage(9, &["p { padding-top: 4px }".into()]), vec!["PctNonNeg".to_string()]);
        assert!(missing_coverage(9, &["p { padding-top: 4px; padding-left: 10% }".into()]).is_empty());
    }
}
