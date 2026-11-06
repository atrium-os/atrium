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

/// A page's WebVTT file: at most this many bytes are read, and this many
/// cues kept.
pub const FILE_MAX: usize = 4 << 20;
pub const FILE_CUES_MAX: usize = 8192;

/// The cues of a WebVTT file (W3C WebVTT §6.1, reduced: no regions, no
/// styles, no cue settings), as `(start_ns, end_ns, text)`. Anything that
/// isn't a well-formed cue is skipped, and a file without the `WEBVTT`
/// signature has none.
pub fn vtt_file(data: &[u8]) -> Vec<(i64, i64, String)> {
    let data = &data[..data.len().min(FILE_MAX)];
    let s = String::from_utf8_lossy(data);
    let s = s.strip_prefix('\u{feff}').unwrap_or(&s).replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = s.split('\n');
    let sig = lines.next().unwrap_or("");
    if !(sig == "WEBVTT" || sig.starts_with("WEBVTT ") || sig.starts_with("WEBVTT\t")) {
        return Vec::new();
    }
    let mut out = Vec::new();
    // The cue being read: its times and text so far.
    let mut cue: Option<(i64, i64, String)> = None;
    let finish = |cue: &mut Option<(i64, i64, String)>, out: &mut Vec<(i64, i64, String)>| {
        if let Some((a, b, t)) = cue.take() {
            if let Some(t) = text(Codec::WebVtt, t.as_bytes()) {
                if out.len() < FILE_CUES_MAX {
                    out.push((a, b, t));
                }
            }
        }
    };
    for line in lines {
        if line.trim().is_empty() {
            finish(&mut cue, &mut out);
            continue;
        }
        if line.contains("-->") {
            // A timing line starts a cue, even without a blank line first.
            finish(&mut cue, &mut out);
            if let Some((a, b)) = timing(line) {
                cue = Some((a, b, String::new()));
            }
            continue;
        }
        match cue.as_mut() {
            Some((_, _, t)) => {
                if t.len() < TEXT_MAX * 2 {
                    if !t.is_empty() {
                        t.push('\n');
                    }
                    t.push_str(line);
                }
            }
            // A cue's id, or a NOTE, STYLE or REGION block, or junk.
            None => {}
        }
    }
    finish(&mut cue, &mut out);
    out
}

/// `00:01.000 --> 00:02.500 align:start` to its times.
fn timing(line: &str) -> Option<(i64, i64)> {
    let (a, b) = line.split_once("-->")?;
    let a = timestamp(a.trim())?;
    let b = timestamp(b.split_whitespace().next()?)?;
    let b = b.min(a.saturating_add(CUE_LONGEST_NS));
    (b > a).then_some((a, b))
}

/// The supervisor looks back one minute for cues that are showing.
const CUE_LONGEST_NS: i64 = 60_000_000_000;

/// `[hh:]mm:ss.ttt` (hours any number of digits, up to ten).
fn timestamp(t: &str) -> Option<i64> {
    let (hms, ms) = t.split_once('.')?;
    if ms.len() != 3 || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, s) = match parts[..] {
        [m, s] => ("0", m, s),
        [h, m, s] if (2..=10).contains(&h.len()) => (h, m, s),
        _ => return None,
    };
    let num = |x: &str| x.bytes().all(|b| b.is_ascii_digit()).then(|| x.parse::<i64>().ok()).flatten();
    if m.len() != 2 || s.len() != 2 {
        return None;
    }
    let (h, m, s, ms) = (num(h)?, num(m)?, num(s)?, num(ms)?);
    if m > 59 || s > 59 {
        return None;
    }
    Some((((h * 60 + m) * 60 + s) * 1000 + ms) * 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_webvtt_file() {
        let f = "\u{feff}WEBVTT - a title\r\n\r\nNOTE a comment\r\nover lines\r\n\r\nSTYLE\n::cue { color: red }\n\n\
                 1\n00:00.500 --> 00:01.250 align:start\nHello, <b>world</b>\nline two\n\n\
                 01:00:02.000 --> 01:00:03.000\nan hour in\n00:04.000 --> 00:05.000\nno blank line before\n\n\
                 00:06.000 --> 00:05.000\nbackwards\n\n00:7.000 --> 00:08.000\nbad time\n\n\
                 00:09.000 --> 00:20:00.000\ncapped at a minute\n\n00:10.000 --> 00:11.000\n\n";
        let c = vtt_file(f.as_bytes());
        assert_eq!(
            c,
            vec![
                (500_000_000, 1_250_000_000, "Hello, world\nline two".to_string()),
                (3_602_000_000_000, 3_603_000_000_000, "an hour in".to_string()),
                (4_000_000_000, 5_000_000_000, "no blank line before".to_string()),
                (9_000_000_000, 69_000_000_000, "capped at a minute".to_string()),
            ]
        );
        assert!(vtt_file(b"1\n00:00.500 --> 00:01.250\nno signature\n").is_empty());
        assert!(vtt_file(b"WEBVTTX\n\n00:00.500 --> 00:01.250\nx\n").is_empty());
        let many: String = std::iter::once("WEBVTT\n\n".to_string()).chain((0..9000).map(|i| format!("00:00.000 --> 00:01.000\n{i}\n\n"))).collect();
        assert_eq!(vtt_file(many.as_bytes()).len(), FILE_CUES_MAX);
        // Junk of every kind: no panic.
        for n in 0..200u32 {
            let junk: Vec<u8> = (0..n * 13).map(|i| (i.wrapping_mul(2654435761u32.wrapping_add(n)) >> 13) as u8).collect();
            let _ = vtt_file(&[b"WEBVTT\n".as_slice(), &junk].concat());
        }
    }

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
