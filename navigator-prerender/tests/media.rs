//! ★ The converter records a document's video elements (fresco-video P3):
//! each one's source as the document lane would play it, and whether the
//! page's scripts use MSE.

use navigator_prerender::artifact::media_block;
use navigator_prerender::uses_mse;

#[test]
fn videos_are_recorded_and_classified() {
    let doc = r#"<html><body><video src="a.webm" poster="p.jpg" autoplay muted></video><video src=""></video><video><source src="a.mov" type="video/quicktime"></video><video><source src="b.mp4" type="video/mp4"></video></body></html>"#;
    let b = media_block(doc, 0);
    assert!(b.starts_with("  \"media\": { \"mse_scripts\": 0, \"videos\": ["), "{b}");
    assert!(b.contains("{ \"class\": \"plain\", \"src\": \"a.webm\", \"type\": \"video/webm\", \"sources\": 0, \"poster\": true, \"autoplay\": true, \"muted\": true, \"track_elements\": 0, \"tracks\": [] }"), "{b}");
    assert!(b.contains("\"class\": \"script\""), "an empty src with no MSE script");
    assert!(b.contains("\"class\": \"unplayable\""));
    assert!(b.contains("\"src\": \"b.mp4\", \"type\": \"video/mp4\", \"sources\": 1"));
    assert!(media_block(doc, 3).contains("\"class\": \"mse\""), "with MSE scripts, a script-fed video is MSE-driven");
    assert_eq!(media_block("<html><body><p>No video</p></body></html>", 2), "", "nothing written without videos");
    // The block keeps recordings readable: no line a subresource scan reads.
    assert!(!b.lines().any(|l| l.trim_start().starts_with("{ \"src\"")));
}

#[test]
fn a_videos_tracks_are_recorded() {
    let doc = r#"<html><body><video src="a.webm"><track src="en.vtt" srclang="en" label="English" default><track kind="chapters" src="c.vtt"><track kind="captions" src="cc.vtt" srclang="en"></video></body></html>"#;
    let b = media_block(doc, 0);
    assert!(b.contains("\"track_elements\": 3, \"tracks\": [{ \"src\": \"en.vtt\", \"kind\": \"subtitles\", \"srclang\": \"en\", \"label\": \"English\", \"default\": true }, { \"src\": \"cc.vtt\", \"kind\": \"captions\", \"srclang\": \"en\", \"label\": \"\", \"default\": false }]"), "{b}");
    assert!(!b.lines().any(|l| l.trim_start().starts_with("{ \"src\"")), "on the video's line: no line a subresource scan reads");
}

#[test]
fn mse_use_is_found_in_script_text() {
    assert!(uses_mse("if (window.MediaSource && MediaSource.isTypeSupported('video/webm; codecs=\"vp9\"')) {}"));
    assert!(uses_mse("const sb = ms.addSourceBuffer(mime);"));
    assert!(uses_mse("const ms = new ManagedMediaSource();"));
    assert!(!uses_mse("video.src = 'clip.mp4'; video.play();"));
}
