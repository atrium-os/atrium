//! Who decodes pictures (officina.md §5.2, app-composition.md §6).
//!
//! A picture in a document came from outside, so the process that holds the
//! window and the person's files doesn't decode it: the app installs a
//! [`Decoder`] that asks its `convert` worker (`officina_convert::pictures`),
//! and the painters ([`pergola_richtext`], `officina-pdf`, Slides) ask
//! [`decoder`]. Without one installed they fall back to their own in-process
//! decoder where they were built with it (tests, other apps), or draw a
//! placeholder.
//!
//! [`mime`] only looks at the first bytes, as a file manager would.

use std::sync::OnceLock;

/// Decoded pixels, 8-bit RGBA, rows top to bottom.
#[derive(Clone)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pixels {}x{}", self.width, self.height)
    }
}

/// Decodes pictures somewhere other than the caller's process.
pub trait Decoder: Send + Sync {
    /// `bytes` decoded at about `target` (never above the natural size), or
    /// at the natural size; None when it isn't a picture that can be drawn.
    fn decode(&self, bytes: &[u8], target: Option<(u32, u32)>) -> Option<Pixels>;
    /// The natural size, from the headers.
    fn size(&self, bytes: &[u8]) -> Option<(u32, u32)>;
}

static DECODER: OnceLock<Box<dyn Decoder>> = OnceLock::new();

/// Makes `d` the process's decoder; false if one was installed already.
pub fn install(d: Box<dyn Decoder>) -> bool {
    DECODER.set(d).is_ok()
}

/// The installed decoder, if any.
pub fn decoder() -> Option<&'static dyn Decoder> {
    DECODER.get().map(|b| b.as_ref())
}

/// The MIME type of a picture Officina takes, from its first bytes.
pub fn mime(b: &[u8]) -> Option<&'static str> {
    let head = |s: &[u8]| b.starts_with(s);
    Some(if head(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if head(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if head(b"GIF87a") || head(b"GIF89a") {
        "image/gif"
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else if let Some(k) = officina_pict::sniff(b) {
        match k {
            officina_pict::Kind::Wmf => "image/wmf",
            officina_pict::Kind::Emf => "image/emf",
            officina_pict::Kind::Bmp => "image/bmp",
        }
    } else if svg(b) {
        "image/svg+xml"
    } else {
        return None;
    })
}

/// SVG as the Navigator's decoder takes it: `<svg` near the start, and not
/// an HTML page that holds one.
fn svg(b: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&b[..b.len().min(1024)]).to_ascii_lowercase();
    head.contains("<svg") && !head.contains("<html") && !head.contains("<!doctype html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_what_officina_takes() {
        assert_eq!(mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(mime(b"<?xml version=\"1.0\"?>\n<!-- x -->\n<svg xmlns=\"\"/>"), Some("image/svg+xml"));
        assert_eq!(mime(b"<html><svg/></html>"), None);
        let mut bmp = b"BM".to_vec();
        bmp.resize(54, 0);
        bmp[10] = 54;
        bmp[14] = 40; // BITMAPINFOHEADER, 1 x 1, one plane, 24 bits
        bmp[18] = 1;
        bmp[22] = 1;
        bmp[26] = 1;
        bmp[28] = 24;
        bmp.extend_from_slice(&[0, 0, 255, 0]);
        assert_eq!(mime(&bmp), Some("image/bmp"));
        assert_eq!(mime(b"hello"), None);
    }
}
