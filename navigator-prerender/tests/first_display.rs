//! The tier-1, first-display conversion (legacy-web §5.4.5.7): no script is
//! fetched or run, the subresources the PARSED document names are collected,
//! and the artifact is tier 1 with a reason saying why.

use navigator_prerender::{artifact, convert_tier1, fetch::Fetcher, TierPolicy, Tier};

/// Serves a stylesheet and refuses (and counts) everything else.
#[derive(Default)]
struct Counting { asked: Vec<String> }
impl Fetcher for Counting {
    fn get(&mut self, url: &str) -> Result<String, String> {
        self.asked.push(url.to_string());
        if url.ends_with(".css") { Ok("p { color: red }".into()) } else { Err(format!("404 {url}")) }
    }
}

const PAGE: &str = r#"<!doctype html><html><head>
<link rel="stylesheet" href="/s.css">
<script src="/app.js"></script>
<script>document.body.innerHTML = '<p>replaced</p>'</script>
</head><body><p>served text</p></body></html>"#;

#[test]
fn no_script_is_fetched_or_run_and_the_artifact_is_tier_one() {
    let mut f = Counting::default();
    let c = convert_tier1(PAGE, Some("https://a.example/"), &mut f);
    assert!(f.asked.iter().all(|u| !u.ends_with(".js")), "fetched a script: {:?}", f.asked);
    assert!(f.asked.iter().any(|u| u == "https://a.example/s.css"), "{:?}", f.asked);
    assert!(c.scripts_held);
    assert_eq!((c.scripts_total, c.scripts_run), (2, 0));
    assert!(c.html.contains("<p>served text</p>"));
    assert_eq!(c.html, c.html_tier1, "the document changed without a script running");
    let d = TierPolicy::default().decide(&c);
    assert_eq!(d.tier, Tier::One);
    assert_eq!(d.reason, "first display: scripts not run yet");
    let json = artifact::emit(Some("https://a.example/"), &c, &TierPolicy::default());
    assert!(json.contains("\"tier\": 1,") && json.contains("s.css"), "{json}");
}

#[test]
fn it_is_reproducible() {
    let a = convert_tier1(PAGE, Some("https://a.example/"), &mut Counting::default());
    let b = convert_tier1(PAGE, Some("https://a.example/"), &mut Counting::default());
    let p = TierPolicy::default();
    assert_eq!(artifact::emit(Some("https://a.example/"), &a, &p), artifact::emit(Some("https://a.example/"), &b, &p));
}

/// ★ Step 4's hints: every stylesheet, image and script of the parsed
/// document, each once, before the first fetch — in the order the
/// conversion needs them.
#[derive(Default)]
struct Hinted { hints: Vec<String>, asked: Vec<String> }
impl Fetcher for Hinted {
    fn get(&mut self, url: &str) -> Result<String, String> { self.asked.push(url.into()); Err("404".into()) }
    fn hint(&mut self, url: &str) { assert!(self.asked.is_empty(), "hint after a fetch: {url}"); self.hints.push(url.into()) }
}

const NAMES: &str = r#"<html><head><script src="/a.js"></script><link rel=stylesheet href="/s.css">
<link rel=icon href="/i.ico"></head><body><img src="/p.png"><img src="/p.png"><img src="data:x">
<script src="https://cdn.example/b.js"></script></body></html>"#;

#[test]
fn a_tier_one_conversion_hints_sheets_and_images_only() {
    let mut f = Hinted::default();
    convert_tier1(NAMES, Some("https://a.example/"), &mut f);
    assert_eq!(f.hints, vec!["https://a.example/s.css", "https://a.example/p.png"]);
}

#[test]
fn a_scripted_conversion_hints_scripts_before_images() {
    let mut f = Hinted::default();
    navigator_prerender::convert_with(NAMES, Some("https://a.example/"), &mut navigator_prerender::boa_impl::BoaEngine::default(), &mut f);
    assert_eq!(f.hints, vec!["https://a.example/s.css", "https://a.example/a.js", "https://cdn.example/b.js", "https://a.example/p.png"]);
}

#[test]
fn a_progressive_conversion_hints_once_for_both_in_first_display_order() {
    let mut f = Hinted::default();
    let mut eng = navigator_prerender::boa_impl::BoaEngine::default();
    navigator_prerender::convert_progressive(NAMES, Some("https://a.example/"), &mut eng, &mut f, false, &mut |_, _| {});
    assert_eq!(f.hints, vec!["https://a.example/s.css", "https://a.example/p.png", "https://a.example/a.js", "https://cdn.example/b.js"]);
}
