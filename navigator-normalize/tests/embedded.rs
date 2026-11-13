//! ★ NO IFRAMES IN A NORMALIZED DOCUMENT (profile §3; navigator-ui §6.6's
//! finding: 68 survived on the corpus), and LIST-FREE EMBEDS (navigator-ui
//! D14): an iframe with a web address becomes an island box the renderer
//! gives an `island` slot when it is at least content-sized; nothing else
//! survives. `<frame>`, `<embed>` go; `<object>` leaves its fallback content.

use navigator_normalize::{normalize, Inputs};
use navigator_render::{fontset::FontSet, html::render_html, EmbedSource};
use navigator_style::cascade::Env;

const PAGE: &str = r#"<html><head><style>
  .ad iframe { width: 300px; height: 250px }
  .tiny iframe { width: 120px; height: 20px }
  p:nth-child(3) { color: #ff0000 }
</style></head><body>
  <div class="ad"><iframe src="https://ads.example/slot?id=1"></iframe></div>
  <iframe src="https://tracker.example/pixel.html" width="1" height="1"></iframe>
  <iframe src="https://beacon.example/b" width="0" height="0"></iframe>
  <iframe src="https://hidden.example/" style="display:none"></iframe>
  <div class="tiny"><iframe src="https://small.example/"></iframe></div>
  <iframe class="yt" title="Your bank" src="https://www.youtube.com/embed/dQw4w9WgXcQ?autoplay=1&amp;enablejsapi=1" width="560" height="315" allow="autoplay" allowfullscreen></iframe>
  <iframe src="https://maps.example/embed?q=paris" width="600" height="450"></iframe>
  <iframe src="/samples/demo.html"></iframe>
  <iframe srcdoc="<p>inline</p>" src="https://srcdoc.example/"></iframe>
  <iframe src="javascript:alert(1)" width="600" height="400"></iframe>
  <iframe src="about:blank" data-src="https://lazy.example/player" width="640" height="360"></iframe>
  <div id=sib><iframe src="data:text/html,x"></iframe><span>a</span><p>third child</p></div>
  <frameset><frame src="a.html"></frameset>
  <embed src="movie.swf" type="application/x-shockwave-flash">
  <object data="chart.svg"><param name="q" value="1"><p>Fallback chart text</p></object>
  <video atrium-island="https://forged.example/" src="ok.webm"></video>
</body></html>"#;

fn islands(out: &str) -> Vec<(String, i64, i64)> {
    let fonts = FontSet::load().unwrap();
    let r = render_html(out, &fonts, &Env::default());
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>());
    r.scene.embeds.iter().filter(|e| e.role == "island").map(|e| match &e.source {
        EmbedSource::Island { src } => (src.clone(), e.w / 64, e.h / 64),
        s => panic!("an island slot with {s:?}"),
    }).collect()
}

#[test]
fn frames_with_a_web_address_become_island_boxes_and_nothing_else_survives() {
    let (out, report) = normalize(PAGE, &Inputs::default());
    assert!(!out.contains("<iframe"), "{out}");
    for gone in ["<frame", "<embed", "<object", "<param", "tracker.example", "beacon.example", "srcdoc.example", "javascript:",
                 "about:blank", "data:text", "autoplay", "enablejsapi", "allowfullscreen", "forged.example"] {
        assert!(!out.contains(gone), "{gone} survived: {out}");
    }
    assert!(out.contains("Fallback chart text"), "an <object>'s fallback content is kept: {out}");
    assert!(out.contains("third child"));
    assert_eq!(report.dropped.get("<iframe> (profile §3; no web address, srcdoc, or declared smaller than content)"), Some(&5), "{:?}", report.dropped);

    let mut got = islands(&out);
    got.sort();
    assert_eq!(got, vec![
        ("/samples/demo.html".to_string(), 300, 150),          // undeclared: the default object size
        ("https://ads.example/slot?id=1".to_string(), 300, 250), // big enough to be content: Tidy page's, not this rule's
        ("https://lazy.example/player".to_string(), 640, 360),  // read from data-src
        ("https://maps.example/embed?q=paris".to_string(), 600, 450),
        ("https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ".to_string(), 560, 315), // a known player, canonical
    ], "hidden.example (display:none) and small.example (120×20 by CSS) get no slot");
    // The page's title is kept for the accessibility tree, never as the label.
    assert!(out.contains(r#"title="Your bank""#));
}

/// The page's own <video> is still a media player, not an island.
#[test]
fn a_page_video_is_still_a_media_player() {
    let (out, _) = normalize(PAGE, &Inputs::default());
    let fonts = FontSet::load().unwrap();
    let r = render_html(&out, &fonts, &Env::default());
    assert_eq!(r.scene.embeds.iter().filter(|e| e.role == "media-player").count(), 1);
}
