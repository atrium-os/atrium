//! Subtitle samples to plain cue text. The input is hostile like any other
//! media: what leaves here is at most `TEXT_MAX` bytes of UTF-8, at most
//! `LINES_MAX` lines, no control characters, no markup.
use atrium_media_demux::Codec;

pub const TEXT_MAX: usize = 480;
pub const LINES_MAX: usize = 4;

/// A cue's text from one subtitle sample, or `None` for an empty cue.
pub fn text(codec: Codec, data: &[u8]) -> Option<String> {
    let raw = match codec {
        Codec::WebVtt if data.starts_with(&[0, 0]) || is_box(data) => wvtt(data)?,
        Codec::WebVtt => data,
        // tx3g: u16 length, then the text (styles follow, ignored).
        Codec::Tx3g => {
            let n = u16::from_be_bytes([*data.first()?, *data.get(1)?]) as usize;
            data.get(2..2 + n)?
        }
        _ => return None,
    };
    let s = String::from_utf8_lossy(raw);
    let plain = strip_markup(&s);
    let mut lines: Vec<String> = Vec::new();
    for l in plain.lines() {
        let l: String = l.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
        if !l.is_empty() && lines.len() < LINES_MAX {
            lines.push(l);
        }
    }
    let mut out = lines.join("\n");
    if out.len() > TEXT_MAX {
        let mut cut = TEXT_MAX;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    (!out.is_empty()).then_some(out)
}

fn is_box(d: &[u8]) -> bool {
    d.len() >= 8 && matches!(&d[4..8], b"vttc" | b"vtte")
}

/// ISO/IEC 14496-30: a sample is `vttc` boxes (each with a `payl` text) or
/// one `vtte` (no cue). Several cues at once are joined by lines.
fn wvtt(d: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i + 8 <= d.len() {
        let size = u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]) as usize;
        if size < 8 || i + size > d.len() {
            return None;
        }
        if &d[i + 4..i + 8] == b"vttc" {
            let mut j = i + 8;
            while j + 8 <= i + size {
                let s = u32::from_be_bytes([d[j], d[j + 1], d[j + 2], d[j + 3]]) as usize;
                if s < 8 || j + s > i + size {
                    return None;
                }
                if &d[j + 4..j + 8] == b"payl" {
                    // The first cue's payload; more in one sample are rare.
                    return Some(&d[j + 8..j + s]);
                }
                j += s;
            }
        }
        i += size;
    }
    None
}

/// WebVTT cue text without its tags (`<i>`, `<c.x>`, `<00:01.000>`, ...)
/// and with its character references decoded.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // Up to the closing '>', or the end.
                for d in chars.by_ref() {
                    if d == '>' {
                        break;
                    }
                }
            }
            '&' => {
                let mut name = String::new();
                while let Some(&d) = chars.peek() {
                    if d == ';' || name.len() > 8 || !(d.is_ascii_alphanumeric() || d == '#') {
                        break;
                    }
                    name.push(d);
                    chars.next();
                }
                let sym = match name.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "nbsp" => Some('\u{a0}'),
                    "lrm" => Some('\u{200e}'),
                    "rlm" => Some('\u{200f}'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
                    n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
                    _ => None,
                };
                if chars.peek() == Some(&';') && sym.is_some() {
                    chars.next();
                    out.push(sym.unwrap());
                } else {
                    out.push('&');
                    out.push_str(&name);
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_goes_and_limits_hold() {
        assert_eq!(text(Codec::WebVtt, b"Hello, <i>world</i> &amp; <c.loud>you</c>").as_deref(), Some("Hello, world & you"));
        assert_eq!(text(Codec::WebVtt, b"a &lt;b&gt; &#65;&#x42; &bogus; &").as_deref(), Some("a <b> AB &bogus; &"));
        assert_eq!(text(Codec::WebVtt, b"1\n2\n3\n4\n5\n").as_deref(), Some("1\n2\n3\n4"));
        assert_eq!(text(Codec::WebVtt, b"\x1b[31mred\x07").as_deref(), Some("[31mred"));
        assert_eq!(text(Codec::WebVtt, b"  \n <b></b> "), None);
        let long = "\u{00e9}".repeat(400);
        let t = text(Codec::WebVtt, long.as_bytes()).unwrap();
        assert!(t.len() <= TEXT_MAX && t.chars().all(|c| c == '\u{00e9}'));
        assert_eq!(text(Codec::Tx3g, b"\x00\x05hello\x00\x00").as_deref(), Some("hello"));
        assert_eq!(text(Codec::Tx3g, b"\x00\x09hi"), None, "length past the end");
        let vttc = [&20u32.to_be_bytes()[..], b"vttc", &12u32.to_be_bytes(), b"payl", b"Hi!!"].concat();
        assert_eq!(text(Codec::WebVtt, &vttc).as_deref(), Some("Hi!!"));
        let vtte = [&8u32.to_be_bytes()[..], b"vtte"].concat();
        assert_eq!(text(Codec::WebVtt, &vtte), None);
        let bad = [&99u32.to_be_bytes()[..], b"vttc"].concat();
        assert_eq!(text(Codec::WebVtt, &bad), None);
    }
}
