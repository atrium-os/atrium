//! ★ THE VIDEO ELEMENT IN THE DOCUMENT LANE (fresco-video.md P3; limen.md
//! §2.2): a `<video>` is a replaced box holding one embed slot, with a
//! placeholder under it until the slot binds; its source is chosen as the
//! player would play it; the slot belongs to its band.

use navigator_render::{bands::Banded, fontset::FontSet, html::{playable, render_html, render_html_with}, nsg, EmbedSource, Scene};
use navigator_style::cascade::Env;

const PX: i64 = 64;

fn scene(body: &str) -> Scene {
    let fonts = FontSet::load().unwrap();
    render_html(&format!("<html><head><style>body {{ margin: 0px }} p {{ margin: 0px }}</style></head><body>{body}</body></html>"), &fonts, &Env::default()).scene
}

#[test]
fn a_video_file_is_an_embed_slot_at_its_box_over_a_placeholder() {
    let s = scene(r#"<p>Before</p><video src="https://a.test/clip.webm" width="320" height="180" controls loop></video><p>After</p>"#);
    assert_eq!(s.embeds.len(), 1);
    let e = &s.embeds[0];
    assert_eq!((e.w, e.h), (320 * PX, 180 * PX), "the declared size");
    assert_eq!(e.role, "media-player");
    assert_eq!(e.source, EmbedSource::Url { src: "https://a.test/clip.webm".into(), mime: "video/webm".into() });
    assert_eq!(e.flags, navigator_render::embed_flags::CONTROLS | navigator_render::embed_flags::LOOP);
    // A dark placeholder fills the same box, painted just before the slot.
    let at = s.order.iter().position(|&(k, _)| k == 7).unwrap();
    let (k, i) = s.order[at - 1];
    assert_eq!(k, 0);
    let r = &s.rects[i];
    assert_eq!((r.x, r.y, r.w, r.h, r.rgba), (e.x, e.y, e.w, e.h, 0x000000ff));
    // "After" is laid out below the video, which takes its room inline.
    let after = s.runs.iter().find(|r| r.text == "After").unwrap();
    assert!(after.y > e.y + e.h, "the next line is below the video");
}

#[test]
fn an_undeclared_video_is_the_default_object_size_and_its_children_are_not_content() {
    let s = scene(r#"<video><source src="/a.mp4" type="video/mp4">Your browser cannot play this video.</video>"#);
    assert_eq!((s.embeds[0].w, s.embeds[0].h), (300 * PX, 150 * PX), "CSS's default object size");
    assert!(!s.runs.iter().any(|r| r.text.contains("browser")), "fallback content is not laid out");
    assert_eq!(s.embeds[0].source, EmbedSource::Url { src: "/a.mp4".into(), mime: "video/mp4".into() });
}

#[test]
fn the_source_is_the_first_the_player_plays() {
    let s = scene(r#"<video width="10" height="10"><source src="a.mov" type="video/quicktime"><source src="b.webm" type='video/webm; codecs="vp9, opus"'></video>
        <video width="10" height="10" src=""></video>
        <video width="10" height="10" src="blob:https://a.test/0f1e"></video>
        <video width="10" height="10"><source src="a.ogv"><source src="a.mov"></video>
        <video width="10" height="10"><source src="x.m3u8"></video>"#);
    let src: Vec<&EmbedSource> = s.embeds.iter().map(|e| &e.source).collect();
    assert_eq!(src[0], &EmbedSource::Url { src: "b.webm".into(), mime: "video/webm; codecs=\"vp9, opus\"".into() });
    assert_eq!(src[1], &EmbedSource::Script, "an empty src is the script's to set");
    assert_eq!(src[2], &EmbedSource::Script, "a blob: URL is an MSE feed");
    assert_eq!(src[3], &EmbedSource::Unplayable);
    assert_eq!(src[4], &EmbedSource::Unplayable, "an HLS manifest needs a script (or a native HLS player)");
    assert!(playable("video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\""));
    assert!(playable("audio/webm; codecs=opus"));
    assert!(!playable("video/mp4; codecs=\"hev1.1.6.L93.B0\""), "HEVC is not decoded");
    assert!(!playable("video/ogg"));
}

#[test]
fn a_poster_paints_under_the_slot_when_its_bytes_are_supplied() {
    let fonts = FontSet::load().unwrap();
    let mut subs = navigator_render::Subresources::new();
    subs.insert("p.jpg".into(), ("blake3:00".into(), 640 * PX, 360 * PX));
    let s = render_html_with(r#"<html><body><video src="v.mp4" poster="p.jpg" width="320" height="180"></video></body></html>"#, &fonts, &Env::default(), &subs).scene;
    assert_eq!(s.images.len(), 1, "the poster");
    assert!(!s.rects.iter().any(|r| r.rgba == 0x000000ff), "no placeholder under a poster");
    let at = s.order.iter().position(|&(k, _)| k == 7).unwrap();
    assert_eq!(s.order[at - 1].0, 5, "the slot over the poster");
}

#[test]
fn the_slot_is_in_the_nsg_and_reads_back() {
    let fonts = FontSet::load().unwrap();
    let s = scene(r#"<video src="https://a.test/a b.webm" width="320" height="180" autoplay muted></video><video width="8" height="8"></video>"#);
    let text = nsg::write(&s, &fonts);
    assert!(text.contains("embed media-player k"), "{text}");
    let e = nsg::embeds(&text);
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].2, (s.embeds[0].x, s.embeds[0].y, 320 * PX, 180 * PX));
    assert_eq!(e[0].4, EmbedSource::Url { src: "https://a.test/a b.webm".into(), mime: "video/webm".into() });
    assert_eq!(e[0].3, navigator_render::embed_flags::AUTOPLAY | navigator_render::embed_flags::MUTED);
    assert_eq!(e[1].4, EmbedSource::Script);
    // Out-of-shape lines are skipped, never half read.
    assert!(nsg::embeds("embed media-player kx 0 0 1 1 a0 script\nembed media-player k1 0 0 1 1 a0 url \"unterminated").is_empty());
}

#[test]
fn a_slot_belongs_to_its_band_and_goes_with_it() {
    let fonts = FontSet::load().unwrap();
    let mut body = String::new();
    for i in 0..60 { body.push_str(&format!("<p>Paragraph {i}, with some words to make a line or two of it at this width.</p>")) }
    body.push_str(r#"<video src="v.webm" width="320" height="180"></video>"#);
    for i in 0..60 { body.push_str(&format!("<p>Later paragraph {i}, also a line or two.</p>")) }
    let doc = format!("<html><head><style>body {{ margin: 8px }} p {{ margin: 0px 0px 8px }}</style></head><body>{body}</body></html>");
    let mut b = Banded::new(&doc, &fonts, &Env::default(), &Default::default(), 600 * PX);
    b.settle();
    let holding: Vec<usize> = (0..b.laid_out()).filter(|&k| { b.ensure(k); b.scene(k).unwrap().embeds.len() == 1 }).collect();
    assert!(!holding.is_empty() && holding.len() <= 2, "the slot is in the band(s) it is laid out in: {holding:?}");
    let k = holding[0];
    assert!(b.evict(k));
    assert!(b.scene(k).is_none(), "evicting the band drops the slot with it");
}
