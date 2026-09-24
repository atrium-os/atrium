//! Reading a converter recording (`atrium-navigator-recording/3`).
//!
//! ★ This is the seam the normalizer's "no capabilities" rule (§6) rests on:
//! the document AND everything it needs — its stylesheets, its images'
//! intrinsic sizes and content addresses — arrive in one file, so the
//! normalizer never fetches and never measures what it cannot see.
//!
//! The parse is deliberately small and strict about what it takes: a
//! recording is produced by the converter, but the converter's INPUT is a
//! hostile document, so nothing here is trusted beyond being well-formed
//! JSON of the shape v3 defines.

use crate::Inputs;

#[derive(Debug, Default)]
pub struct Recording {
    pub format: String,
    pub url: String,
    pub document: String,
    pub inputs: Inputs,
    /// `src` → content address, for the renderer to paint from CAS.
    pub addresses: std::collections::BTreeMap<String, String>,
}

pub fn parse(json: &str) -> Result<Recording, String> {
    let mut r = Recording::default();
    r.format = string_field(json, "\"format\"").unwrap_or_default();
    if !r.format.starts_with("atrium-navigator-recording/") {
        return Err(format!("not a recording: {:?}", r.format));
    }
    r.url = string_field(json, "\"url\"").unwrap_or_default();
    r.document = string_field(json, "\"document\"").ok_or("recording has no document")?;
    for obj in objects_in_array(json, "\"stylesheets\"") {
        let (Some(href), Some(text)) = (string_field(&obj, "\"href\""), string_field(&obj, "\"text\"")) else { continue };
        let media = string_field(&obj, "\"media\"").unwrap_or_default();
        // A sheet with a media condition is stored under a key the
        // normalizer recognises, since `Inputs` carries text alone.
        r.inputs.stylesheets.insert(href.clone(), text);
        if !media.is_empty() { r.inputs.stylesheet_media.insert(href, media); }
    }
    for obj in objects_in_array(json, "\"images\"") {
        let Some(src) = string_field(&obj, "\"src\"") else { continue };
        // Either dimension may be `null`; the ratio is `W/H` or `none`, and
        // an older recording without the field implies it from both sizes.
        let (w, h) = (number_field(&obj, "\"width\"").map(|v| v as u32), number_field(&obj, "\"height\"").map(|v| v as u32));
        let ratio = match string_field(&obj, "\"ratio\"").as_deref() {
            Some("none") => None,
            Some(r) => r.split_once('/').and_then(|(a, b)| Some((a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?))),
            None => w.zip(h),
        };
        r.inputs.images.insert(src.clone(), crate::ImageSize { width: w, height: h, ratio });
        if let Some(a) = string_field(&obj, "\"address\"").filter(|a| !a.is_empty()) { r.addresses.insert(src, a); }
    }
    Ok(r)
}

/// The value of a `"key": "…"` pair, with JSON escapes undone.
fn string_field(json: &str, key: &str) -> Option<String> {
    let at = json.find(key)? + key.len();
    let rest = &json[at..];
    let start = rest.find('"')?;
    let bytes: Vec<char> = rest[start + 1..].chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            '"' => return Some(out),
            '\\' => {
                i += 1;
                match bytes.get(i) {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('u') => {
                        let hex: String = bytes.get(i + 1..i + 5)?.iter().collect();
                        let n = u32::from_str_radix(&hex, 16).ok()?;
                        out.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    Some(c) => out.push(*c),
                    None => return None,
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    None
}

fn number_field(json: &str, key: &str) -> Option<f64> {
    let at = json.find(key)? + key.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))?;
    rest[..end].parse().ok()
}

/// Each `{…}` of a named array, as text — enough to read fields out of.
fn objects_in_array(json: &str, key: &str) -> Vec<String> {
    let Some(at) = json.find(key) else { return vec![] };
    let rest = &json[at + key.len()..];
    let Some(open) = rest.find('[') else { return vec![] };
    let chars: Vec<char> = rest[open..].chars().collect();
    let (mut out, mut depth, mut start, mut in_str, mut esc) = (vec![], 0i32, 0usize, false, false);
    for (i, c) in chars.iter().enumerate() {
        if in_str {
            if esc { esc = false } else if *c == '\\' { esc = true } else if *c == '"' { in_str = false }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => { if depth == 0 { start = i } depth += 1 }
            '}' => { depth -= 1; if depth == 0 { out.push(chars[start..=i].iter().collect()) } }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// Everything the lane needs from one recording: the normalized document.
pub fn normalize_recording(json: &str, fonts: &navigator_render::fontset::FontSet,
                           env: &navigator_style::cascade::Env) -> Result<(String, crate::Report), String> {
    let r = parse(json)?;
    Ok(crate::normalize_and_measure(&r.document, &r.inputs, fonts, env))
}
