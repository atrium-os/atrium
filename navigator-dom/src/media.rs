//! ★ MEDIA ELEMENTS (fresco-video.md P3; limen.md §2.2): what a `<video>`
//! plays, decided once for the renderer (its embed slot) and the converter
//! (what it records and counts).

use crate::{Dom, Handle};

/// What a video element plays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoSource {
    /// A whole resource: the URL as the document writes it (resolved by the
    /// UI against the document's URL), and its MIME type.
    Url { src: String, mime: String },
    /// Nothing the document lane can play: the page's script would set it
    /// (an empty or `blob:` `src`, a player framework's element), so it needs
    /// an engine island with `MediaSource` (legacy-web §8.1).
    Script,
    /// Sources, none of a type the player plays.
    Unplayable,
}

/// ★ What a `<video>` plays (fresco-video P3; limen.md §2.2): its `src`,
/// else its first `<source>` with a `src` the player plays, by type (or by
/// extension when it has none). A video with no URL at all — an empty or
/// `blob:` `src`, no `<source>` — is the page script's to feed (MSE or a
/// script-set `src`), and needs an engine island (legacy-web §8.1). One with
/// sources of no playable type is `Unplayable`.
pub fn video_source(dom: &Dom, h: Handle) -> VideoSource {
    let real = |v: &str| { let v = v.trim(); !v.is_empty() && !v.starts_with("blob:") };
    // A `src` attribute, even empty, is the element's source (HTML: the
    // `<source>` children are used only when there is no `src`).
    if dom.attr(h, "src").is_some_and(|v| !real(v)) { return VideoSource::Script }
    let mut candidates: Vec<(String, String)> = vec![];
    if let Some(src) = dom.attr(h, "src").filter(|v| real(v)) { candidates.push((src.trim().to_string(), dom.attr(h, "type").unwrap_or("").to_string())) }
    for c in dom.element_children(h) {
        if dom.tag(c) != Some("source") { continue }
        if let Some(src) = dom.attr(c, "src").filter(|v| real(v)) { candidates.push((src.trim().to_string(), dom.attr(c, "type").unwrap_or("").trim().to_string())) }
    }
    if candidates.is_empty() { return VideoSource::Script }
    for (src, ty) in &candidates {
        let mime = if ty.is_empty() { mime_of(src) } else { ty.clone() };
        if playable(&mime) { return VideoSource::Url { src: src.clone(), mime } }
    }
    VideoSource::Unplayable
}

/// A media URL's MIME type from its extension (query and fragment cut).
fn mime_of(src: &str) -> String {
    let path = src.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    match path.rsplit('.').next().unwrap_or("") {
        "webm" => "video/webm", "mp4" | "m4v" => "video/mp4", "m4a" => "audio/mp4", "ogv" | "ogg" => "video/ogg",
        "mov" => "video/quicktime", "mkv" => "video/x-matroska", "m3u8" => "application/vnd.apple.mpegurl", "mpd" => "application/dash+xml",
        _ => "",
    }.to_string()
}

/// ★ Whether the player plays a type (atrium-player.md §3–4): WebM and MP4
/// containers, with AV1, VP8, VP9 or H.264 video and Opus, Vorbis or AAC
/// audio. A container the player does not demux (Ogg, QuickTime,
/// Matroska, HLS and DASH manifests) is not played, whatever its codecs.
pub fn playable(mime: &str) -> bool {
    let m = mime.trim().to_ascii_lowercase();
    let base = m.split(';').next().unwrap_or("").trim();
    if !matches!(base, "video/webm" | "audio/webm" | "video/mp4" | "audio/mp4") { return false }
    match m.split_once("codecs=") {
        None => true,
        Some((_, c)) => c.trim_matches(|ch| ch == '"' || ch == '\'' || ch == ' ').split(',').all(|codec| {
            let c = codec.trim().trim_matches('"');
            ["av01", "vp8", "vp9", "vp09", "avc1", "avc3", "opus", "vorbis", "mp4a"].iter().any(|p| c.starts_with(p))
        }),
    }
}

