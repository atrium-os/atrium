//! ★ The converter fetches script only from the document's own origin
//! (legacy-web §5.4.1f, closing navigator-ui §6.6's finding): a declared
//! third-party script, ad and analytics tags included, is never asked for,
//! not even as a prefetch hint, and the refusal is counted by host.

use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::Fetcher};
use std::collections::HashMap;

/// Serves a table, and records every URL it was asked for or hinted.
#[derive(Default)]
struct Recording { table: HashMap<String, String>, asked: Vec<String>, hinted: Vec<String> }
impl Fetcher for Recording {
    fn get(&mut self, url: &str) -> Result<String, String> {
        self.asked.push(url.to_string());
        self.table.get(url).cloned().ok_or_else(|| format!("404 {url}"))
    }
    fn hint(&mut self, url: &str) { self.hinted.push(url.to_string()) }
    fn hint_header(&mut self, url: &str) { self.hinted.push(url.to_string()) }
}

fn touched(f: &Recording, host: &str) -> Vec<String> {
    f.asked.iter().chain(&f.hinted).filter(|u| u.contains(host)).cloned().collect()
}

#[test]
fn a_third_party_script_is_never_fetched_or_hinted() {
    let mut f = Recording::default();
    f.table.insert("https://news.example/app.js".into(),
        "document.getElementById('r').textContent = 'own script ran';".into());
    f.table.insert("https://ads.example/tag.js".into(),
        "document.body.setAttribute('ad', 'ran');".into());
    let html = r#"<html><head>
        <script src="https://ads.example/tag.js"></script>
        <script async src="//ads.example/tag.js?slot=2"></script>
        <script src="/app.js"></script>
      </head><body><p id=r>before</p></body></html>"#;
    let c = convert_with(html, Some("https://news.example/story.html"), &mut BoaEngine::default(), &mut f);
    assert!(touched(&f, "ads.example").is_empty(), "the ad host was contacted: {:?}", touched(&f, "ads.example"));
    assert!(c.html.contains("own script ran"), "the page's own script must still run: {}", c.html);
    assert!(!c.html.contains("ad=\"ran\""));
    assert_eq!((c.external_total, c.external_fetched, c.external_refused, c.external_failed), (3, 1, 2, 0));
    assert!(c.blocked_hosts.contains(&("cross-origin script ads.example".to_string(), 2)), "{:?}", c.blocked_hosts);
    assert!(c.page_blocked >= 2, "refusals count as page-network refusals");
}

/// The origin is scheme, host and port: a sibling subdomain (the site's own
/// CDN, or its first-party analytics host), another port and plain http
/// are all other origins.
#[test]
fn same_site_other_port_and_other_scheme_are_other_origins() {
    let mut f = Recording::default();
    let html = r#"<html><head>
        <script src="https://cdn.news.example/a.js"></script>
        <script src="https://news.example:8443/b.js"></script>
        <script src="http://news.example/c.js"></script>
      </head><body></body></html>"#;
    let c = convert_with(html, Some("https://news.example/p.html"), &mut BoaEngine::default(), &mut f);
    assert!(f.asked.is_empty() && f.hinted.iter().all(|u| !u.ends_with(".js")), "{:?} {:?}", f.asked, f.hinted);
    assert_eq!(c.external_refused, 3);
}

/// A same-origin module may not pull code from elsewhere by importing it.
#[test]
fn a_module_cannot_import_across_origins() {
    let mut f = Recording::default();
    f.table.insert("https://news.example/js/app.js".into(),
        "import { t } from 'https://tracker.example/sdk.js';\nimport { make } from './helper.js';\nmake('own module ran');".into());
    f.table.insert("https://news.example/js/helper.js".into(),
        "export function make(t){ var p = document.createElement('p'); p.textContent = t; document.body.appendChild(p); }".into());
    let html = r#"<html><body><script type="module" src="/js/app.js"></script></body></html>"#;
    let c = convert_with(html, Some("https://news.example/page.html"), &mut BoaEngine::default(), &mut f);
    assert!(touched(&f, "tracker.example").is_empty(), "{:?}", f.asked);
    assert!(f.asked.contains(&"https://news.example/js/helper.js".to_string()));
    assert!(c.blocked_hosts.iter().any(|(k, _)| k == "cross-origin script tracker.example"), "{:?}", c.blocked_hosts);
}

/// An inline module's imports go through the same gate.
#[test]
fn an_inline_module_cannot_import_across_origins() {
    let mut f = Recording::default();
    let html = r#"<html><body><script type="module">import '//tracker.example/x.js';</script></body></html>"#;
    let _ = convert_with(html, Some("https://news.example/page.html"), &mut BoaEngine::default(), &mut f);
    assert!(touched(&f, "tracker.example").is_empty(), "{:?}", f.asked);
}

/// With no document URL there is no origin, so no script is fetched.
#[test]
fn no_base_no_script() {
    let mut f = Recording::default();
    let html = r#"<html><body><script src="https://a.example/x.js"></script></body></html>"#;
    let c = convert_with(html, None, &mut BoaEngine::default(), &mut f);
    assert!(f.asked.is_empty() && f.hinted.is_empty(), "{:?} {:?}", f.asked, f.hinted);
    assert_eq!(c.external_refused, 1);
}

/// A `data:` script is not a network request: not fetched, not counted as
/// a refusal by host.
#[test]
fn a_data_script_is_not_a_network_refusal() {
    let mut f = Recording::default();
    let html = r#"<html><body><script src="data:text/javascript,1"></script></body></html>"#;
    let c = convert_with(html, Some("https://news.example/p.html"), &mut BoaEngine::default(), &mut f);
    assert!(f.asked.is_empty());
    assert!(c.blocked_hosts.is_empty(), "{:?}", c.blocked_hosts);
}

/// ★ A tracking pixel (declared at most 1 × 1, or `hidden` itself) is
/// never fetched or hinted, whatever its host; a real image, even one CSS
/// hides (a carousel slide), still is.
#[test]
fn a_pixel_is_never_fetched() {
    let mut f = Recording::default();
    let html = r#"<html><body>
        <img src="https://px.example/a.gif" width="1" height="1">
        <img src="https://px.example/b.gif" width="0" height="0">
        <img src="https://px.example/c.gif" hidden>
        <img src="https://cdn.example/slide2.jpg" style="display: none">
        <img src="https://news.example/photo.png">
      </body></html>"#;
    let _ = convert_with(html, Some("https://news.example/p.html"), &mut BoaEngine::default(), &mut f);
    assert!(touched(&f, "px.example").is_empty(), "{:?} {:?}", f.asked, f.hinted);
    for real in ["photo.png", "slide2.jpg"] {
        assert!(f.asked.iter().chain(&f.hinted).any(|u| u.ends_with(real)), "{real}: {:?}", f.asked);
    }
}
