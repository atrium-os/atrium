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
    /// ★ A RECOGNISED EMBED (legacy-web §8.1): an `<iframe>` the normalizer
    /// recognised as a known video player and turned into a `<video>` box
    /// carrying `atrium-island`. Its `src` is the canonical player URL
    /// ([`recognised_embed`]): it plays only as a live part, an
    /// origin-scoped engine island, never in the document lane.
    Island { src: String },
}

/// The attribute that marks a `<video>` box as a recognised embed. Only the
/// normalizer writes it (it drops any the page wrote), and [`video_source`]
/// reads it back through [`recognised_embed`] again, so even a forged one
/// names nothing an `<iframe>` could not.
pub const ISLAND_ATTR: &str = "atrium-island";

/// ★ THE RECOGNISED EMBEDS: an iframe `src` that is one of these players
/// becomes a live part; every other iframe is dropped (profile §3 excludes
/// `<iframe>`). Returns the CANONICAL URL, rebuilt from the video id alone:
/// the page's query (autoplay, tracking, `enablejsapi`, origin) never
/// reaches the island. A typed list, not a pattern match on hosts:
///
/// - YouTube: `youtube.com/embed/<id>` or `youtube-nocookie.com/embed/<id>`
///   (any `www.`/`m.`), `<id>` 11 of `[A-Za-z0-9_-]`. Canonical:
///   `https://www.youtube-nocookie.com/embed/<id>`, the variant that sets
///   no cookie until play.
/// - Vimeo: `player.vimeo.com/video/<digits>`. Canonical:
///   `https://player.vimeo.com/video/<digits>`.
///
/// `src` may be absolute (`https:`, `http:`) or scheme-relative (`//`).
pub fn recognised_embed(src: &str) -> Option<String> {
    let s = src.trim();
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).or_else(|| s.strip_prefix("//"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].to_ascii_lowercase();
    let path = rest[end..].split(['?', '#']).next().unwrap_or("");
    let host = host.strip_suffix(".").unwrap_or(&host);
    let seg = |prefix: &str| path.strip_prefix(prefix).map(|r| r.trim_end_matches('/')).filter(|r| !r.contains('/'));
    match host {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtube-nocookie.com" | "www.youtube-nocookie.com" => {
            let id = seg("/embed/")?;
            (id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
                .then(|| format!("https://www.youtube-nocookie.com/embed/{id}"))
        }
        "player.vimeo.com" => {
            let id = seg("/video/")?;
            (!id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit()))
                .then(|| format!("https://player.vimeo.com/video/{id}"))
        }
        _ => None,
    }
}

/// ★ What a `<video>` plays (fresco-video P3; limen.md §2.2): its `src`,
/// else its first `<source>` with a `src` the player plays, by type (or by
/// extension when it has none). A video with no URL at all — an empty or
/// `blob:` `src`, no `<source>` — is the page script's to feed (MSE or a
/// script-set `src`), and needs an engine island (legacy-web §8.1). One with
/// sources of no playable type is `Unplayable`.
pub fn video_source(dom: &Dom, h: Handle) -> VideoSource {
    if let Some(v) = dom.attr(h, ISLAND_ATTR) {
        return match recognised_embed(v) { Some(src) => VideoSource::Island { src }, None => VideoSource::Unplayable };
    }
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


/// What a `<track>` is for (only these two are shown as text).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackKind { Subtitles, Captions }

impl TrackKind {
    pub fn as_str(self) -> &'static str { match self { TrackKind::Subtitles => "subtitles", TrackKind::Captions => "captions" } }
    pub fn parse(s: &str) -> Option<TrackKind> {
        match s { "subtitles" => Some(TrackKind::Subtitles), "captions" => Some(TrackKind::Captions), _ => None }
    }
}

/// One `<track>` of a video, as its player's subtitle menu lists it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextTrack {
    /// The player's id for it: its place among the video's tracks.
    pub id: u8,
    /// As the document writes it (resolved by the UI, like the video's).
    pub src: String,
    pub kind: TrackKind,
    /// BCP 47, and the label the menu shows: at most [`TEXT_NAME_MAX`] bytes,
    /// no control characters (limen's `TextTrack` check).
    pub srclang: String,
    pub label: String,
    pub default: bool,
}

/// Most tracks per video, and the length of a language or label: limen's
/// `TEXT_TRACKS_MAX` and `TEXT_NAME_MAX`.
pub const TEXT_TRACKS_MAX: usize = 16;
pub const TEXT_NAME_MAX: usize = 64;

/// A name as the player takes it: trimmed, control characters dropped, cut
/// at a character boundary to [`TEXT_NAME_MAX`] bytes.
fn clean_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > TEXT_NAME_MAX { break }
        out.push(c);
    }
    out
}

/// ★ A `<video>`'s text tracks (fresco-video P3, captions): its `<track>`
/// children of kind `subtitles` (also the kind a missing `kind` means) or
/// `captions`, with a `src`, in document order, at most
/// [`TEXT_TRACKS_MAX`]. An unknown `kind` is HTML's `metadata`, and
/// `descriptions`, `chapters` and `metadata` are not shown as text: left
/// out. Only the first `default` one is the default (HTML: at most one per
/// video among subtitles and captions).
pub fn text_tracks(dom: &Dom, h: Handle) -> Vec<TextTrack> {
    let mut out: Vec<TextTrack> = vec![];
    for c in dom.element_children(h) {
        if dom.tag(c) != Some("track") || out.len() == TEXT_TRACKS_MAX { continue }
        let kind = match dom.attr(c, "kind").map(|k| k.trim().to_ascii_lowercase()) {
            None => TrackKind::Subtitles,
            Some(k) if k.is_empty() => TrackKind::Subtitles,
            Some(k) => match TrackKind::parse(&k) { Some(k) => k, None => continue },
        };
        let Some(src) = dom.attr(c, "src").map(str::trim).filter(|s| !s.is_empty() && !s.starts_with("blob:")) else { continue };
        let default = dom.attr(c, "default").is_some() && !out.iter().any(|t| t.default);
        out.push(TextTrack { id: out.len() as u8, src: src.to_string(), kind,
                             srclang: clean_name(dom.attr(c, "srclang").unwrap_or("")), label: clean_name(dom.attr(c, "label").unwrap_or("")), default });
    }
    out
}

#[cfg(test)]
mod embed_tests {
    use super::*;

    #[test]
    fn recognised_embeds_are_canonical_and_nothing_else_is() {
        let yt = Some("https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ".to_string());
        assert_eq!(recognised_embed("https://www.youtube.com/embed/dQw4w9WgXcQ?autoplay=1&origin=x"), yt);
        assert_eq!(recognised_embed("//www.youtube-nocookie.com/embed/dQw4w9WgXcQ"), yt);
        assert_eq!(recognised_embed("http://YouTube.com/embed/dQw4w9WgXcQ/#t=3"), yt);
        assert_eq!(recognised_embed("https://player.vimeo.com/video/76979871?h=8272103f6e"),
                   Some("https://player.vimeo.com/video/76979871".to_string()));
        for no in ["https://www.youtube.com/watch?v=dQw4w9WgXcQ", "https://www.youtube.com/embed/short",
                   "https://www.youtube.com/embed/dQw4w9WgXcQ/../x", "https://youtube.com.evil.example/embed/dQw4w9WgXcQ",
                   "https://evil.example/www.youtube.com/embed/dQw4w9WgXcQ", "javascript:alert(1)//www.youtube.com/embed/dQw4w9WgXcQ",
                   "https://player.vimeo.com/video/12ab", "https://ads.example/frame.html", "/embed/dQw4w9WgXcQ", ""] {
            assert_eq!(recognised_embed(no), None, "{no}");
        }
    }

    #[test]
    fn an_island_attribute_is_read_back_through_the_list() {
        let d = crate::parse(r#"<video atrium-island="https://www.youtube.com/embed/dQw4w9WgXcQ"></video><video atrium-island="https://evil.example/x"></video>"#);
        let v = d.by_tag_anywhere("video");
        assert_eq!(video_source(&d, v[0]), VideoSource::Island { src: "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ".into() });
        assert_eq!(video_source(&d, v[1]), VideoSource::Unplayable, "a forged island names nothing");
    }
}

#[cfg(test)]
mod track_tests {
    use super::*;

    fn video(inner: &str) -> (Dom, Handle) {
        let d = crate::parse(&format!("<video src=a.webm>{inner}</video>"));
        let v = d.by_tag_anywhere("video")[0];
        (d, v)
    }

    #[test]
    fn subtitles_and_captions_with_a_src_in_order() {
        let (d, v) = video(r#"<track src=en.vtt srclang=en label=English default><track kind=captions src=cc.vtt srclang="en-US" label=" CC "><track kind=chapters src=ch.vtt><track kind=bogus src=x.vtt><track kind=subtitles><track kind=SUBTITLES src=fr.vtt srclang=fr default>"#);
        let t = text_tracks(&d, v);
        assert_eq!(t.len(), 3);
        assert_eq!((t[0].id, t[0].kind, t[0].src.as_str(), t[0].default), (0, TrackKind::Subtitles, "en.vtt", true));
        assert_eq!((t[1].id, t[1].kind, t[1].label.as_str(), t[1].srclang.as_str()), (1, TrackKind::Captions, "CC", "en-US"));
        assert_eq!((t[2].id, t[2].srclang.as_str(), t[2].default), (2, "fr", false), "only the first default is the default");
    }

    #[test]
    fn at_most_sixteen_and_names_within_the_players_limits() {
        let many: String = (0..20).map(|i| format!("<track src=t{i}.vtt>")).collect();
        let (d, v) = video(&many);
        assert_eq!(text_tracks(&d, v).len(), TEXT_TRACKS_MAX);
        let long = format!("<track src=a.vtt label=\"{}\u{e9}tail\" srclang=\"en\u{7}x\">", "x".repeat(63));
        let (d, v) = video(&long);
        let t = &text_tracks(&d, v)[0];
        assert_eq!(t.label.len(), 63, "cut before a two-byte character that would pass 64");
        assert_eq!(t.srclang, "enx", "control characters dropped");
    }
}
