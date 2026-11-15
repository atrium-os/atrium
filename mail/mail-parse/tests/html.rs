//! HTML mail into a profile document (mail.md §7.3, §7.4).
#![cfg(feature = "html")]

use mail_parse::html::{Converted, Source, Use};
use navigator_render::fontset::FontSet;
use navigator_render::html::render_html;
use navigator_style::cascade::Env;
use std::collections::BTreeMap;

fn convert(html: &str, loaded: &BTreeMap<String, (u32, u32)>) -> Converted {
    let fonts = FontSet::load().expect("pinned font set");
    mail_parse::html::convert(html, loaded, &fonts, &Env::default())
}

fn refusals(html: &str) -> Vec<String> {
    let fonts = FontSet::load().expect("pinned font set");
    render_html(html, &fonts, &Env::default()).diagnostics.iter().map(|d| format!("{}: {}", d.code, d.msg)).collect()
}

const NEWSLETTER: &str = r#"<!doctype html><html lang="en"><head>
<meta http-equiv="refresh" content="0;url=https://evil.example/">
<base href="https://evil.example/">
<link rel="stylesheet" href="https://cdn.example/news.css">
<style>
  @import url("https://cdn.example/more.css");
  @font-face { font-family: Brand; src: url(https://cdn.example/brand.woff2) }
  .hero { background: #123456 url('https://cdn.example/hero.jpg') no-repeat }
  .pix { display: none }
  table.outer td p { color: #333333; margin: 0 0 8px }
</style>
<script>alert(1)</script>
</head><body onload="steal()">
<table class="outer" width="600"><tr><td background="https://cdn.example/bg.png">
  <div class="hero"><h1>Autumn sale</h1></div>
  <p>Hello <a href="https://shop.example/sale?u=42">see the sale</a> or
     <a href="javascript:steal()">this</a>.</p>
  <img src="https://cdn.example/product.jpg" width="300" height="200" alt="A coat">
  <img src="cid:logo@shop" width="120" height="40" alt="Logo">
  <img src="https://track.example/open.gif?u=42" width="1" height="1">
  <img src="https://track.example/hidden.gif?u=42" class="pix">
  <div style="opacity:0"><img src="https://track.example/opacity.gif" width="50" height="50"></div>
  <img src="https://track.example/offscreen.gif" width="40" height="40" style="position:absolute;left:-9999px">
  <form action="https://shop.example/subscribe"><input name="email"><button>Subscribe</button></form>
  <iframe src="https://evil.example/frame"></iframe>
  <video poster="https://evil.example/poster.jpg" src="https://evil.example/v.mp4"></video>
  <img src="https://cdn.example/footer.png" srcset="https://cdn.example/footer@2x.png 2x">
</td></tr></table>
</body></html>"#;

#[test]
fn the_document_is_accepted_by_the_profile_and_keeps_its_content() {
    let c = convert(NEWSLETTER, &BTreeMap::new());
    let r = refusals(&c.document);
    assert!(r.is_empty(), "converted mail must render with no refusals:\n{r:#?}\n\n{}", c.document);
    assert!(c.document.contains("Autumn sale"));
    assert!(c.document.contains("see the sale"));
    assert!(c.document.contains("Subscribe"), "a form's content is kept, the form is not");
}

#[test]
fn nothing_that_acts_or_fetches_survives() {
    let c = convert(NEWSLETTER, &BTreeMap::new());
    let d = c.document.to_ascii_lowercase();
    for bad in ["<script", "<iframe", "<form", "<input", "<video", "<meta", "<base", "<link", "onload", "javascript:",
                "evil.example", "cdn.example", "track.example", "srcset", "@import", "@font-face"] {
        assert!(!d.contains(bad), "{bad} survived:\n{}", c.document);
    }
    // The real link survives for the link bar.
    assert!(c.document.contains("https://shop.example/sale?u=42"));
}

#[test]
fn every_picture_is_listed_once_by_an_opaque_id() {
    let c = convert(NEWSLETTER, &BTreeMap::new());
    let remote = |u: &str| c.resources.iter().find(|r| r.source == Source::Remote(u.to_string())).unwrap_or_else(|| panic!("{u} not listed: {:#?}", c.resources));
    let product = remote("https://cdn.example/product.jpg");
    assert_eq!(product.used_as, Use::Image);
    assert!(product.loadable());
    assert!(c.document.contains(&format!("src=\"{}\"", product.id)), "{}", c.document);
    assert!(c.resources.iter().any(|r| r.source == Source::Cid("logo@shop".into())));
    assert_eq!(remote("https://cdn.example/hero.jpg").used_as, Use::Background);
    assert_eq!(remote("https://cdn.example/bg.png").used_as, Use::Background);
    // A font is never fetched, even when pictures are loaded.
    let font = remote("https://cdn.example/brand.woff2");
    assert_eq!(font.used_as, Use::Other);
    assert!(!font.loadable());
}

#[test]
fn tracking_pixels_are_marked_and_removed() {
    let c = convert(NEWSLETTER, &BTreeMap::new());
    for u in ["https://track.example/open.gif?u=42", "https://track.example/hidden.gif?u=42",
              "https://track.example/opacity.gif", "https://track.example/offscreen.gif"] {
        let r = c.resources.iter().find(|r| r.source == Source::Remote(u.into())).unwrap_or_else(|| panic!("{u} not listed"));
        assert!(r.hidden && !r.loadable(), "{u} must be hidden: {r:?}\n{}", c.document);
        assert!(!c.document.contains(&format!("\"{}\"", r.id)), "{u} still in the document");
    }
    // product, footer, hero and the td background: what the bar counts.
    assert_eq!(c.remote_pictures(), 4, "{:#?}", c.resources);
}

#[test]
fn the_same_html_gives_the_same_ids_and_a_loaded_size_is_used() {
    let a = convert(NEWSLETTER, &BTreeMap::new());
    let b = convert(NEWSLETTER, &BTreeMap::new());
    assert_eq!(a.resources, b.resources);
    assert_eq!(a.document, b.document);
    // The footer declared no size; once loaded its real one is used.
    let footer = a.resources.iter().find(|r| r.source == Source::Remote("https://cdn.example/footer.png".into())).unwrap();
    let loaded = [(footer.id.clone(), (640u32, 80u32))].into_iter().collect();
    let c = convert(NEWSLETTER, &loaded);
    assert!(c.document.contains(&format!("src=\"{}\" width=\"640\" height=\"80\"", footer.id))
        || (c.document.contains("width=\"640\"") && c.document.contains("height=\"80\"")), "{}", c.document);
    assert!(refusals(&c.document).is_empty());
}

#[test]
fn hostile_input_does_not_panic() {
    for s in ["", "<", "<img src=", "<style>url(", "<style>@import", "<p style=\"background:url(\">x",
              "<img src=\"cid:%zz%4\">", "<svg><use href=\"https://x/y.svg#a\"/></svg>", "<td background=\"//x.example/a\">"] {
        let c = convert(s, &BTreeMap::new());
        assert!(refusals(&c.document).is_empty(), "{s:?} → {:?}\n{}", refusals(&c.document), c.document);
    }
}
