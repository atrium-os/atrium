//! ★ NO IFRAMES IN A NORMALIZED DOCUMENT (profile §3; navigator-ui §6.6's
//! finding: 68 survived on the corpus). Every `<iframe>` is dropped with its
//! box unless it is a recognised embed, which becomes an island box the
//! renderer gives an `island` embed slot. `<frame>`, `<embed>` go;
//! `<object>` leaves its fallback content.

use navigator_normalize::{normalize, Inputs};
use navigator_render::{fontset::FontSet, html::render_html, EmbedSource};
use navigator_style::cascade::Env;

const PAGE: &str = r#"<html><head><style>
  .ad iframe { width: 300px; height: 250px }
  .player { width: 640px }
  p:nth-child(3) { color: #ff0000 }
</style></head><body>
  <div class="ad"><iframe src="https://ads.example/slot?id=1" width="300" height="250"></iframe></div>
  <iframe src="https://tracker.example/pixel.html" style="display:none"></iframe>
  <div class="player"><iframe class="yt" title="Launch video" src="https://www.youtube.com/embed/dQw4w9WgXcQ?autoplay=1&amp;enablejsapi=1" width="560" height="315" allow="autoplay" allowfullscreen></iframe></div>
  <iframe src="//player.vimeo.com/video/76979871"></iframe>
  <div id=sib><iframe src="https://x.example/"></iframe><span>a</span><p>third child</p></div>
  <frameset><frame src="a.html"></frameset>
  <embed src="movie.swf" type="application/x-shockwave-flash">
  <object data="chart.svg"><param name="q" value="1"><p>Fallback chart text</p></object>
  <video atrium-island="https://www.youtube.com/embed/AAAAAAAAAAA" src="ok.webm"></video>
</body></html>"#;

#[test]
fn only_recognised_embeds_survive_and_they_become_island_boxes() {
    let (out, report) = normalize(PAGE, &Inputs::default());
    assert!(!out.contains("<iframe"), "{out}");
    for gone in ["<frame", "<embed", "<object", "<param", "ads.example", "tracker.example", "x.example", "autoplay", "enablejsapi", "allowfullscreen"] {
        assert!(!out.contains(gone), "{gone} survived: {out}");
    }
    assert!(out.contains("Fallback chart text"), "an <object>'s fallback content is kept: {out}");
    assert!(out.contains(r#"atrium-island="https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ""#), "{out}");
    assert!(out.contains(r#"atrium-island="https://player.vimeo.com/video/76979871""#), "{out}");
    assert!(out.contains(r#"title="Launch video""#), "the title is kept for the accessibility tree: {out}");
    assert!(!out.contains("AAAAAAAAAAA"), "an island the page wrote itself is removed: {out}");
    assert_eq!(report.dropped.get("<iframe> (profile §3; not a recognised embed)"), Some(&3), "{:?}", report.dropped);
    assert_eq!(report.dropped.get("<iframe> recognised as an embed (now an island box)"), Some(&2));

    // The structural selector was matched with the iframe still a sibling,
    // as a browser does: the <p> is the third child.
    assert!(out.contains("third child"));

    // Rendered: no refusals, two island slots, one media-player for the
    // page's own <video>, and the YouTube box at its declared size.
    let fonts = FontSet::load().unwrap();
    let r = render_html(&out, &fonts, &Env::default());
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>());
    let islands: Vec<_> = r.scene.embeds.iter().filter(|e| e.role == "island").collect();
    assert_eq!(islands.len(), 2, "{:?}", r.scene.embeds);
    assert!(islands.iter().any(|e| e.source == EmbedSource::Island { src: "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ".into() }
        && (e.w, e.h) == (560 * 64, 315 * 64)), "{islands:?}");
    assert!(islands.iter().any(|e| matches!(&e.source, EmbedSource::Island { src } if src.contains("vimeo"))
        && (e.w * 9 - e.h * 16).abs() <= 16 * 64), "an undeclared embed is 16/9: {islands:?}");
    assert_eq!(r.scene.embeds.iter().filter(|e| e.role == "media-player").count(), 1);
}

/// A recognised embed that the page hid stays hidden: no slot.
#[test]
fn a_hidden_embed_has_no_slot() {
    let html = r#"<html><body><iframe style="display:none" src="https://www.youtube.com/embed/dQw4w9WgXcQ"></iframe><p>x</p></body></html>"#;
    let (out, _) = normalize(html, &Inputs::default());
    let fonts = FontSet::load().unwrap();
    assert!(render_html(&out, &fonts, &Env::default()).scene.embeds.is_empty(), "{out}");
}
