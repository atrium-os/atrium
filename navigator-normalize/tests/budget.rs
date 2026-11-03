//! ★ The normalizer's work budgets (see `Budget`). Each test drives one
//! budget past its limit and checks three things: the call RETURNS, the
//! report SAYS what was not done, and the output is still a document the
//! profile accepts. Each has a control showing the same input normalizes in
//! full under the default budget, so a budget that bit everything could not
//! pass quietly.

use navigator_normalize::{normalize, Budget, Inputs};
use navigator_render::fontset::FontSet;
use navigator_render::html::render_html;
use navigator_style::cascade::Env;

fn refusals(html: &str) -> Vec<String> {
    let fonts = FontSet::load().expect("pinned font set");
    render_html(html, &fonts, &Env::default()).diagnostics.iter()
        .map(|d| format!("{}: {}", d.code, d.msg)).collect()
}

fn with(b: Budget) -> Inputs { Inputs { budget: b, ..Inputs::default() } }

fn said(r: &navigator_normalize::Report, what: &str) -> bool { r.dropped.keys().any(|k| k.contains(what)) }

/// The "billion laughs" of custom properties: each level references the one
/// below ten times, so nine levels would expand to 10^9 tokens.
#[test]
fn exponential_var_expansion_is_cut_off() {
    let mut css = String::from(":root { --l0: 1px 1px 1px 1px 1px 1px 1px 1px 1px 1px;");
    for i in 1..10 { css.push_str(&format!(" --l{i}:{}", format!(" var(--l{})", i - 1).repeat(10))); }
    css.push_str(" } p { margin-left: var(--l9) } p.ok { color: #cf222e }");
    let src = format!("<html><head><style>{css}</style></head><body><p class=\"ok\">x</p></body></html>");
    let (out, report) = normalize(&src, &Inputs::default());
    assert!(report.spent.var_steps < 64_000_000, "bounded: {:?}", report.spent);
    assert!(out.contains("#cf222e"), "the rest of the page is still normalized: {out}");
    assert!(!out.contains("margin-left: 1px 1px"), "the exploded value is not emitted: {out}");
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}

#[test]
fn the_document_var_budget_stops_substitution_and_says_so() {
    let src = r#"<html><head><style>:root { --a: 4px } p { margin-left: var(--a) } div { padding-top: var(--a) }</style></head>
                 <body><p>x</p><div>y</div></body></html>"#;
    let (out, report) = normalize(src, &with(Budget { var_steps: 1, ..Budget::default() }));
    assert!(said(&report, "var() substitution past"), "{:?}", report.dropped);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    // Control.
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("margin-left: 4px") && out.contains("padding-top: 4px"), "{out}");
    assert!(!said(&report, "var() substitution past"));
}

#[test]
fn the_matching_budget_drops_the_rules_it_did_not_reach() {
    let src = r#"<html><head><style>p { color: #111111 } div p { color: #222222 } .z { margin-left: 3px }</style></head>
                 <body><div><p class="z">x</p></div></body></html>"#;
    let (out, report) = normalize(src, &with(Budget { match_steps: 2, ..Budget::default() }));
    assert!(said(&report, "selector-matching budget"), "{:?}", report.dropped);
    assert!(!out.contains("margin-left: 3px"), "a rule past the budget is not applied: {out}");
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    let (out, report) = normalize(src, &Inputs::default());
    assert!(out.contains("margin-left: 3px"), "{out}");
    assert!(!said(&report, "selector-matching budget"));
}

#[test]
fn past_the_element_budget_the_text_survives_unstyled() {
    let src = r#"<html><head><style>p { margin-left: 5px }</style></head><body><p>kept text</p><p>more</p></body></html>"#;
    let (out, report) = normalize(src, &with(Budget { elements: 3, ..Budget::default() }));
    assert!(said(&report, "element budget"), "{:?}", report.dropped);
    assert!(out.contains("kept text") && !out.contains("margin-left: 5px"), "{out}");
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
    let (out, _) = normalize(src, &Inputs::default());
    assert!(out.contains("margin-left: 5px"));
}

#[test]
fn a_sheet_past_the_css_budget_is_not_read() {
    let src = r#"<html><head><style>p { margin-left: 5px }</style><style>p { margin-top: 6px }</style></head><body><p>x</p></body></html>"#;
    let first = "p { margin-left: 5px }".len();
    let (out, report) = normalize(src, &with(Budget { css_bytes: first, ..Budget::default() }));
    assert!(said(&report, "CSS budget"), "{:?}", report.dropped);
    assert!(out.contains("margin-left: 5px") && !out.contains("margin-top: 6px"), "the first sheet fits, the second does not: {out}");
    assert!(report.spent.css_bytes <= first);
    assert!(refusals(&out).is_empty(), "{:?}", refusals(&out));
}
