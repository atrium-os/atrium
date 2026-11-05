//! An image's NATURAL size, read from its own header — a shared
//! measurement (see the crate comment: "shared measurements here"). The
//! converter records it; the WPT oracle needs the same answer for its test
//! images, and reading it from one place is what makes the two agree.
//! Moved here from navigator-prerender so the oracle does not depend on
//! the converter (which now depends on the normalizer, legacy-web
//! §5.4.5.6e).

/// ★ An image's NATURAL sizing (CSS Images 3 §5.1): a width, a height and
/// a ratio, EACH optional. A raster image has all three. An SVG may state a
/// width alone (no ratio: its height defaults at layout), or only a
/// viewBox (a ratio and no size), or nothing (measured — and natural size
/// none). Collapsing that to a (width, height) pair invented ratios CSS
/// says are not there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Natural { pub width: Option<u32>, pub height: Option<u32>, pub ratio: Option<(u32, u32)> }

impl Natural {
    pub fn sized(w: u32, h: u32) -> Self { Natural { width: Some(w), height: Some(h), ratio: Some((w, h)) } }
}

/// ★ The image's natural sizing, each part optional (see [`Natural`]).
/// `None` only when the bytes are not an image this can read.
pub fn natural_size(b: &[u8]) -> Option<Natural> {
    if let Some((w, h)) = raster_size(b) { return Some(Natural::sized(w, h)) }
    let head = &b[..b.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    let start = text.find("<svg")?;
    // ★ Only the ROOT element's attributes. Searching the whole file took
    // the first `width="` anywhere — a nested rect's, as often as not.
    let tag = &text[start..start + text[start..].find('>').unwrap_or(text.len() - start)];
    let raw = |name: &str| -> Option<String> {
        let at = [format!(" {name}=\""), format!(" {name}='")].iter().find_map(|k| tag.find(k.as_str()).map(|i| i + k.len()))?;
        let rest = &tag[at..];
        Some(rest[..rest.find(['"', '\''])?].trim().to_string())
    };
    // Only absolute px lengths are natural dimensions; `100%` or `2em`
    // depend on where the image is used, so they are no natural size.
    let px = |name: &str| -> Option<f64> {
        let v = raw(name)?;
        let v = v.strip_suffix("px").unwrap_or(&v);
        v.parse::<f64>().ok().filter(|n| *n > 0.0)
    };
    let (w, h) = (px("width"), px("height"));
    let vb: Option<(f64, f64)> = raw("viewBox").and_then(|v| {
        let n: Vec<f64> = v.split([' ', ',']).filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
        (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then(|| (n[2], n[3]))
    });
    // ★ A ratio is EXACT: the numbers themselves (to three decimals),
    // reduced by their gcd — 1000×1300 is 10/13. Scaling the larger side
    // to 1000 and rounding made it 769/1000, and the Rust book's diagrams
    // came out a fraction of a pixel short, shifting every line below.
    let ratio_of = |a: f64, b: f64| -> (u32, u32) {
        let (mut x, mut y) = (((a * 1000.0).round() as u64).max(1), ((b * 1000.0).round() as u64).max(1));
        let (mut p, mut q) = (x, y);
        while q != 0 { let r = p % q; p = q; q = r }
        x /= p; y /= p;
        // Keep it in u32 without losing the proportion.
        while x > u32::MAX as u64 || y > u32::MAX as u64 { x = (x + 1) / 2; y = (y + 1) / 2 }
        (x as u32, y as u32)
    };
    let ratio = match (w, h) { (Some(a), Some(b)) => Some(ratio_of(a, b)), _ => vb.map(|(a, b)| ratio_of(a, b)) };
    Some(Natural { width: w.map(|v| v.round() as u32), height: h.map(|v| v.round() as u32), ratio })
}

fn raster_size(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| -> Option<u32> { Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?])) };
    let be16 = |i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let le16 = |i: usize| -> Option<u32> { Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        // JPEG: walk the segments to the frame header.
        let mut i = 2usize;
        // ★ `i + 9 <= len`: the frame header's last byte read is `i + 8`,
        // and a `<` here made a JPEG whose frame is the LAST segment report
        // no size at all.
        while i + 9 <= b.len() {
            if b[i] != 0xFF { i += 1; continue }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            let seg = be16(i + 2)? as usize;
            if seg < 2 { return None }
            i += 2 + seg;
        }
        return None;
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { return Some((le16(6)?, le16(8)?)) }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16) {
            Some(b"VP8X") => {
                let d = |i: usize| -> Option<u32> { Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0]) + 1) };
                Some((d(24)?, d(27)?))
            }
            Some(b"VP8 ") => Some(((le16(26)? & 0x3FFF), (le16(28)? & 0x3FFF))),
            Some(b"VP8L") => {
                let bits = u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            _ => None,
        };
    }
    None
}

/// The most leading bytes of an image `header_size` will ask for. An image
/// whose size is not known within them stays unsized until it arrives.
/// (Both corpora, 2,737 images: 16 KiB answers 2,720 of them, 64 KiB all
/// but one — a JPEG whose EXIF puts its frame header at 84 KiB.)
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// What an image's LEADING bytes say about its natural size
/// (legacy-web §5.4.5.7, cut A: sizing a pending image from its header).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Header {
    /// ★ Exactly what `natural_size` will say of the whole image, however
    /// it continues: every byte that answer depends on is in hand.
    Sized(Natural),
    /// Not known yet: the first `n` bytes would say more (`n` is more than
    /// was given, and at most `MAX_HEADER_BYTES`).
    More(usize),
    /// Not from this header: not a format read here, malformed, over the
    /// dimension cap, or the answer lies past `MAX_HEADER_BYTES`. The image
    /// stays unsized until its bytes arrive.
    Unreadable(&'static str),
}

/// ★★ AN IMAGE'S SIZE FROM ITS FIRST BYTES, ON HOSTILE INPUT. `prefix` is
/// the start of the image; `complete` says it is the whole image.
///
/// The contract, which the tests check differentially against
/// `natural_size` on every prefix of real and mutated images: `Sized(n)`
/// only when `natural_size` of ANY image that starts with `prefix` (of
/// `prefix` itself, when `complete`) is `Some(n)`. So a box sized from the
/// header is the box the whole image will be given, and its arrival moves
/// nothing. Anything this cannot promise is `More` or `Unreadable`, never
/// a guess.
///
/// Strict where `natural_size` is lenient: zero dimensions, either side over
/// `profile::MAX_IMAGE_DIMENSION`, or a decoded frame over
/// `profile::MAX_DECODED_IMAGE_BYTES` are `Unreadable` — the decoder would
/// refuse that image, so its box is not sized from a claim it will not honour.
/// Reads nothing past `prefix`, allocates nothing but `natural_size`'s
/// bounded text view of an SVG's first 4 KiB, and walks a JPEG in at most
/// one step per byte.
pub fn header_size(prefix: &[u8], complete: bool) -> Header {
    use crate::profile::{MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_DIMENSION};
    let b = &prefix[..prefix.len().min(MAX_HEADER_BYTES)];
    // Bytes past the cap are never read, so a prefix cut at the cap is not
    // the whole image.
    let complete = complete && b.len() == prefix.len();
    let more = |n: usize| -> Header {
        if complete { Header::Unreadable("the image ends inside its header") }
        else if n > MAX_HEADER_BYTES { Header::Unreadable("the header runs past 64 KiB") }
        else { Header::More(n) }
    };
    // Every magic below is in the first 16 bytes and every fixed-position
    // header in the first 30; under 32, ask for 32.
    if b.len() < 32 && !complete { return Header::More(32) }
    let sized = |n: Option<Natural>| -> Header {
        let Some(n) = n else { return Header::Unreadable("no size in the header") };
        let dims = [n.width, n.height];
        if dims.contains(&Some(0)) { return Header::Unreadable("a zero dimension") }
        if dims.iter().flatten().any(|d| *d > MAX_IMAGE_DIMENSION) { return Header::Unreadable("over the dimension cap") }
        if let (Some(w), Some(h)) = (n.width, n.height) {
            if w as usize * h as usize * 4 > MAX_DECODED_IMAGE_BYTES { return Header::Unreadable("over the decode budget") }
        }
        Header::Sized(n)
    };
    let png = b.starts_with(b"\x89PNG\r\n\x1a\n");
    let jpeg = b.starts_with(&[0xFF, 0xD8]);
    let gif = b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a");
    let riff = b.starts_with(b"RIFF");
    // `natural_size` reads each format's fields at fixed offsets, except
    // JPEG's; when they are in hand it reads nothing else, so its answer on
    // the prefix is its answer on the whole.
    if png {
        if b.len() < 24 { return more(24) }
        if b.get(12..16) != Some(b"IHDR") { return Header::Unreadable("a PNG without IHDR first") }
        return sized(natural_size(&b[..24]));
    }
    if gif {
        return if b.len() < 10 { more(10) } else { sized(natural_size(&b[..10])) };
    }
    if riff {
        if b.len() < 16 { return more(16) }
        if b.get(8..12) != Some(b"WEBP") { return Header::Unreadable("RIFF, not WebP") }
        let need = match b.get(12..16) { Some(b"VP8X") | Some(b"VP8 ") => 30, Some(b"VP8L") => 25,
                                         _ => return Header::Unreadable("a WebP chunk not read here") };
        return if b.len() < need { more(need) } else { sized(natural_size(&b[..need])) };
    }
    if jpeg {
        // `raster_size`'s own walk, stopping where it would read past the
        // prefix. Each step advances `i` by at least one.
        let mut i = 2usize;
        loop {
            if i + 9 > b.len() { return more(i + 9) }
            if b[i] != 0xFF { i += 1; continue }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                return sized(natural_size(&b[..i + 9]));
            }
            let seg = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if seg < 2 { return Header::Unreadable("a JPEG segment shorter than its length field") }
            i += 2 + seg;
        }
    }
    // An SVG is read from its first 4 KiB alone (`natural_size`), once no
    // raster reading can apply: none of the magics above starts it.
    if b.len() < 4096 && !complete { return more(4096) }
    let head = &b[..b.len().min(4096)];
    if !String::from_utf8_lossy(head).contains("<svg") { return Header::Unreadable("not an image read here") }
    sized(natural_size(head))
}

/// The first read of an image's header: 16 KiB answers 99.4% of the
/// corpus's images in one request.
pub const FIRST_HEADER_READ: usize = 16 * 1024;

/// How many leading bytes to have after `have`, when `header_size` said
/// `More(want)`: at least `want`, at least double, never past the cap. So
/// an image is asked for at most four times (16, 32, 64 KiB, or fewer when
/// it ends), however many small segments a hostile JPEG strings together.
pub fn next_header_read(have: usize, want: usize) -> usize {
    want.max(have.saturating_mul(2)).max(FIRST_HEADER_READ).min(MAX_HEADER_BYTES)
}
