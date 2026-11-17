//! The MIME tree (RFC 2045, 2046), text extraction, dates and previews.

use crate::decode::{base64, quoted_printable, to_utf8};
use crate::header::{self, Field, Params};
use crate::{Limits, Refused, Summary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Multipart,
    /// `text/plain` shown as the body.
    Text,
    /// `text/html` shown as the body.
    Html,
    /// `text/calendar` (an invite, mail.md §14.1).
    Calendar,
    /// An image or other part referenced by `cid:` from the HTML.
    Inline,
    /// `message/rfc822`: a forwarded message, shown as an attachment and
    /// parsed on its own when opened.
    Message,
    Attachment,
    /// An S/MIME signature (`application/pkcs7-signature`): checked and
    /// shown as the "Signed by" line, never listed as a file.
    Signature,
}

#[derive(Debug, Clone)]
pub struct Part {
    /// "1", "1.2", … as in IMAP section numbers; "" for a single-part message.
    pub path: String,
    pub kind: PartKind,
    /// `type/subtype`, lower case.
    pub content_type: String,
    pub charset: Option<String>,
    pub filename: Option<String>,
    pub content_id: Option<String>,
    pub transfer_encoding: String,
    /// Byte range of the encoded body in the raw message.
    pub body: (usize, usize),
    /// Decoded size, estimated from the encoding.
    pub size: usize,
}

/// The decoded bytes of one part (for attachments, quick look and saving).
pub fn part_bytes(raw: &[u8], part: &Part) -> Vec<u8> {
    let (a, b) = part.body;
    let body = &raw[a.min(raw.len())..b.min(raw.len())];
    decode_body(body, &part.transfer_encoding, usize::MAX)
}

pub(crate) fn decode_body(body: &[u8], cte: &str, cap: usize) -> Vec<u8> {
    match cte {
        "base64" => base64(body, cap),
        "quoted-printable" => quoted_printable(body, false, cap),
        _ => body[..body.len().min(cap)].to_vec(),
    }
}

struct Walk<'a> {
    raw: &'a [u8],
    limits: &'a Limits,
    count: usize,
}

pub(crate) fn walk(raw: &[u8], fields: &[Field], body_at: usize, limits: &Limits, s: &mut Summary) -> Result<(), Refused> {
    let mut w = Walk { raw, limits, count: 0 };
    w.entity(fields, (body_at, raw.len()), String::new(), 0, false, s)
}

impl Walk<'_> {
    fn entity(&mut self, fields: &[Field], body: (usize, usize), path: String, depth: usize, in_digest: bool, s: &mut Summary) -> Result<(), Refused> {
        self.count += 1;
        if self.count > self.limits.max_parts {
            return Err(Refused::TooManyParts);
        }
        if depth > self.limits.max_depth {
            return Err(Refused::TooDeep);
        }
        let ct = header::get(fields, "content-type")
            .map(|v| header::params(v, 256))
            .filter(|p| p.value.contains('/'))
            .unwrap_or_else(|| Params {
                value: if in_digest { "message/rfc822".into() } else { "text/plain".into() },
                params: vec![],
            });
        let cte = header::get(fields, "content-transfer-encoding")
            .map(|v| String::from_utf8_lossy(v).trim().to_ascii_lowercase())
            .unwrap_or_else(|| "7bit".into());
        let disp = header::get(fields, "content-disposition").map(|v| header::params(v, 256));
        let attachment = disp.as_ref().map(|d| d.value == "attachment").unwrap_or(false);
        let filename = disp
            .as_ref()
            .and_then(|d| d.get("filename"))
            .or_else(|| ct.get("name"))
            .map(|f| sanitize_filename(f))
            .filter(|f| !f.is_empty());
        let content_id = header::get(fields, "content-id").map(|v| {
            let t = String::from_utf8_lossy(v);
            header::clean(t.trim().trim_start_matches('<').trim_end_matches('>'), 256)
        });

        if ct.value.starts_with("multipart/") {
            if let Some(boundary) = ct.get("boundary").filter(|b| !b.is_empty() && b.len() <= 200) {
                let boundary = boundary.to_string();
                let digest = ct.value == "multipart/digest";
                s.parts.push(Part {
                    path: path.clone(),
                    kind: PartKind::Multipart,
                    content_type: ct.value.clone(),
                    charset: None,
                    filename: None,
                    content_id: None,
                    transfer_encoding: cte,
                    body,
                    size: body.1 - body.0,
                });
                let children = split_multipart(self.raw, body, boundary.as_bytes());
                // S/MIME (RFC 8551 §3.5): the first part exactly as sent,
                // headers and all, is what the second part signs.
                let protocol = ct.get("protocol").map(|p| p.to_ascii_lowercase());
                if ct.value == "multipart/signed" && s.signed.is_none() && children.len() == 2
                    && matches!(protocol.as_deref(), Some("application/pkcs7-signature" | "application/x-pkcs7-signature"))
                {
                    let sig_path = if path.is_empty() { "2".to_string() } else { format!("{path}.2") };
                    s.signed = Some(crate::Signed { content: children[0], signature: sig_path });
                }
                for (n, child) in children.into_iter().enumerate() {
                    let child_path = if path.is_empty() { format!("{}", n + 1) } else { format!("{}.{}", path, n + 1) };
                    let slice = &self.raw[child.0..child.1];
                    let (cf, at) = if slice.first() == Some(&b'\n') || slice.starts_with(b"\r\n") {
                        // no headers: the default type applies
                        (Vec::new(), if slice[0] == b'\n' { 1 } else { 2 })
                    } else {
                        header::split(slice, self.limits.max_header_block, self.limits.max_header_fields)?
                    };
                    self.entity(&cf, (child.0 + at, child.1), child_path, depth + 1, digest, s)?;
                }
                return Ok(());
            }
            // a multipart without a boundary can't be split: an attachment
        }

        let size = match cte.as_str() {
            "base64" => (body.1 - body.0) / 4 * 3,
            _ => body.1 - body.0,
        };
        let charset = ct.get("charset").map(|c| header::clean(c, 64));
 if depth == 0 && matches!(ct.value.as_str(), "application/pkcs7-mime" | "application/x-pkcs7-mime") {
            let st = ct.get("smime-type").map(|t| t.to_ascii_lowercase());
            s.encrypted = match st.as_deref() {
                Some("enveloped-data") | Some("authenveloped-data") => true,
                None => !filename.as_deref().is_some_and(|f| !f.eq_ignore_ascii_case("smime.p7m")),
                _ => false,
            };
        }
        let kind = if matches!(ct.value.as_str(), "application/pkcs7-signature" | "application/x-pkcs7-signature") {
            PartKind::Signature
        } else if ct.value == "message/rfc822" || ct.value == "message/global" {
            PartKind::Message
        } else if !attachment && ct.value == "text/plain" && s.text.is_none() {
            PartKind::Text
        } else if !attachment && ct.value == "text/html" && s.html.is_none() {
            PartKind::Html
        } else if ct.value == "text/calendar" || ct.value == "application/ics" {
            PartKind::Calendar
        } else if !attachment && content_id.is_some() && ct.value.starts_with("image/") {
            PartKind::Inline
        } else {
            PartKind::Attachment
        };
        if matches!(kind, PartKind::Text | PartKind::Html) {
            let cap = self.limits.max_text;
            // decode a little more than the cap: charsets can shrink
            let bytes = decode_body(&self.raw[body.0..body.1], &cte, cap.saturating_mul(2));
            let (mut text, known) = to_utf8(&bytes, charset.as_deref());
            if !known {
                s.unknown_charset = true;
            }
            if text.len() > cap {
                let mut cut = cap;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                s.truncated = true;
            }
            if kind == PartKind::Text {
                if ct.get("format").is_some_and(|f| f.eq_ignore_ascii_case("flowed")) {
                    text = unflow(&text, ct.get("delsp").is_some_and(|d| d.eq_ignore_ascii_case("yes")));
                }
                s.text = Some(text);
            } else {
                s.html = Some(text);
            }
        }
        s.parts.push(Part {
            path,
            kind,
            content_type: ct.value.clone(),
            charset,
            filename,
            content_id,
            transfer_encoding: cte,
            body,
            size,
        });
        Ok(())
    }
}

/// Byte ranges of each body part between the boundary delimiters (RFC 2046
/// §5.1.1). The line break before a delimiter belongs to the delimiter.
/// A missing close delimiter ends the last part at the end of the body.
fn split_multipart(raw: &[u8], body: (usize, usize), boundary: &[u8]) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = body.0;
    while i < body.1 {
        let end = raw[i..body.1].iter().position(|&c| c == b'\n').map(|p| i + p).unwrap_or(body.1);
        let line = &raw[i..end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() >= 2 + boundary.len() && line.starts_with(b"--") && &line[2..2 + boundary.len()] == boundary {
            let rest = &line[2 + boundary.len()..];
            let close = rest.starts_with(b"--");
            let rest = if close { &rest[2..] } else { rest };
            if rest.iter().all(|c| *c == b' ' || *c == b'\t') {
                if let Some(s) = start {
                    // drop the CRLF/LF that precedes this delimiter line
                    let mut e = i;
                    if e > s && raw[e - 1] == b'\n' {
                        e -= 1;
                        if e > s && raw[e - 1] == b'\r' {
                            e -= 1;
                        }
                    }
                    parts.push((s, e));
                }
                if close {
                    return parts;
                }
                start = Some((end + 1).min(body.1));
            }
        }
        i = end + 1;
    }
    if let Some(s) = start {
        if s < body.1 {
            parts.push((s, body.1));
        }
    }
    parts
}

/// A file name safe to show and to offer to the picker: no path, no leading
/// dots, no control or invisible characters, at most 255 bytes.
pub fn sanitize_filename(f: &str) -> String {
    let base = f.rsplit(['/', '\\']).next().unwrap_or(f);
    let mut s = header::clean(base, 255);
    s = s.trim().trim_start_matches('.').to_string();
    s.retain(|c| !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'));
    s
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// RFC 5322 §3.3 date-time, with the obsolete zone names (§4.3).
pub fn parse_date(s: &str) -> Option<i64> {
    let s = match s.find(',') {
        Some(p) => &s[p + 1..],
        None => s,
    };
    // drop comments like "(UTC)"
    let s: String = {
        let mut out = String::new();
        let mut depth = 0;
        for c in s.chars() {
            match c {
                '(' => depth += 1,
                ')' if depth > 0 => depth -= 1,
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    };
    let t: Vec<&str> = s.split_whitespace().collect();
    if t.len() < 4 {
        return None;
    }
    let day: i64 = t[0].parse().ok()?;
    let mon = MONTHS.iter().position(|m| t[1].len() >= 3 && t[1][..3].eq_ignore_ascii_case(m))? as i64 + 1;
    let mut year: i64 = t[2].parse().ok()?;
    if t[2].len() <= 2 {
        year += if year < 50 { 2000 } else { 1900 };
    } else if t[2].len() == 3 {
        year += 1900;
    }
    let hms: Vec<&str> = t[3].split(':').collect();
    let h: i64 = hms.first()?.parse().ok()?;
    let mi: i64 = hms.get(1)?.parse().ok()?;
    let sec: i64 = hms.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
    if !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 || !(1900..=9999).contains(&year) {
        return None;
    }
    let zone = t.get(4).copied().unwrap_or("-0000");
    let off = match zone.to_ascii_uppercase().as_str() {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "EDT" => -4 * 60,
        "EST" | "CDT" => -5 * 60,
        "CST" | "MDT" => -6 * 60,
        "MST" | "PDT" => -7 * 60,
        "PST" => -8 * 60,
        z if (z.starts_with('+') || z.starts_with('-')) && z.len() == 5 => {
            let n: i64 = z[1..].parse().ok()?;
            let m = (n / 100) * 60 + n % 100;
            if z.starts_with('-') { -m } else { m }
        }
        _ => 0,
    };
    Some(days_from_civil(year, mon, day) * 86400 + h * 3600 + mi * 60 + sec - off * 60)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// One line for the list: the new text (quoted lines and the signature
/// dropped), whitespace collapsed, cut on a character boundary.
pub fn preview(text: Option<&str>, html: Option<&str>, cap: usize) -> String {
    let src = match (text, html) {
        (Some(t), _) if !t.trim().is_empty() => t.to_string(),
        (_, Some(h)) => html_text(h, cap * 8),
        _ => return String::new(),
    };
    let mut out = String::new();
    for line in src.lines() {
        let l = line.trim();
        if l == "--" || l == "-- " && line.starts_with("-- ") {
            break;
        }
        if l.starts_with('>') {
            continue;
        }
        for w in l.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(w);
            if out.len() >= cap * 2 {
                break;
            }
        }
        if out.len() >= cap * 2 {
            break;
        }
    }
    header::clean(&out, cap)
}

/// Visible text of HTML, roughly: tags dropped, `<head>`, `<style>`,
/// `<script>` and `<title>` contents dropped, a few entities decoded. Only
/// for the preview line; the real rendering is navigator-normalize's.
fn html_text(h: &str, cap: usize) -> String {
    let mut out = String::new();
    let lower = h.to_ascii_lowercase();
    let b = h.as_bytes();
    let mut i = 0;
    while i < b.len() && out.len() < cap {
        if b[i] == b'<' {
            let rest = &lower[i..];
            let skip_to = ["<head", "<style", "<script", "<title"]
                .iter()
                .find(|t| rest.starts_with(*t))
                .map(|t| format!("</{}", &t[1..]));
            if let Some(close) = skip_to {
                i = match rest.find(&close) {
                    Some(p) => i + p + close.len(),
                    None => b.len(),
                };
            }
            match lower[i.min(lower.len())..].find('>') {
                Some(p) => i += p + 1,
                None => break,
            }
            out.push(' ');
        } else if b[i] == b'&' {
            let rest = &h[i..];
            let end = rest.as_bytes()[..rest.len().min(12)].iter().position(|&c| c == b';');
            let (txt, used) = match end.map(|e| &rest[1..e]) {
                Some("amp") => ("&".to_string(), 5),
                Some("lt") => ("<".to_string(), 4),
                Some("gt") => (">".to_string(), 4),
                Some("quot") => ("\"".to_string(), 6),
                Some("nbsp") => (" ".to_string(), 6),
                Some("#39") | Some("apos") => ("'".to_string(), end.unwrap() + 1),
                Some(e) if e.starts_with('#') => {
                    let n = if let Some(x) = e[1..].strip_prefix(['x', 'X']) { u32::from_str_radix(x, 16).ok() } else { e[1..].parse().ok() };
                    (n.and_then(char::from_u32).map(|c| c.to_string()).unwrap_or_default(), e.len() + 2)
                }
                _ => ("&".to_string(), 1),
            };
            out.push_str(&txt);
            i += used;
        } else {
            let c = h[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// RFC 3676 `format=flowed`: a line ending in a space continues on the next
/// line of the same quote depth (with `delsp=yes` that space goes too);
/// space-stuffing is removed; the signature separator `-- ` stays a line.
/// Quote markers are normalised to `> ` per level.
pub fn unflow(text: &str, delsp: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut open: Option<usize> = None; // depth of a paragraph still flowing
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let depth = line.chars().take_while(|&c| c == '>').count();
        let mut body = &line[depth..];
        if body.starts_with(' ') {
            body = &body[1..]; // space-stuffing (or the space after the quote marks)
        }
        let flowed = body.ends_with(' ') && body != "-- ";
        if open != Some(depth) {
            if open.is_some() {
                out.push('\n');
            }
            for _ in 0..depth {
                out.push_str("> ");
            }
        }
        let piece = if flowed && delsp { &body[..body.len() - 1] } else { body };
        out.push_str(piece);
        if flowed {
            open = Some(depth);
        } else {
            out.push('\n');
            open = None;
        }
    }
    if out.ends_with('\n') && !text.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod flowed_tests {
    use super::unflow;

    #[test]
    fn flowed() {
        let t = "Here is a long \r\nparagraph that flows.\r\n\r\n> quoted \r\n> on.\r\n>> deeper\r\n From the start\r\n-- \r\nPriya";
        assert_eq!(unflow(t, false), "Here is a long paragraph that flows.\n\n> quoted on.\n> > deeper\nFrom the start\n-- \nPriya");
        assert_eq!(unflow("Zusammen \r\ngeschrieben", true), "Zusammengeschrieben");
        // A flowing line at a different depth ends the paragraph.
        assert_eq!(unflow("a \r\n> b", false), "a \n> b");
    }
}
