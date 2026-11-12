//! The `pssh` boxes a DASH manifest carries (hospes.md §8.3, H4 open item).
//!
//! WebM has no place for a `pssh`: its encrypted tracks name only a key id, and some licence
//! services (Shaka's proxy among them) refuse a request made from a bare key id. The page or
//! app that reads the manifest passes the `<cenc:pssh>` it finds there, as a browser page
//! passes EME's `initData`. This is the narrow part of that: a scan for
//! `<ContentProtection schemeIdUri="urn:uuid:SYSTEM">` and the `<cenc:pssh>` inside it. Not an
//! XML parser; the manifest is untrusted, so everything is bounded and each box is checked
//! before it is kept.

/// Widevine's DRM system id.
pub const WIDEVINE: [u8; 16] = [0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed];
/// The W3C common system id (Clear Key).
pub const COMMON: [u8; 16] = [0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02, 0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b];

/// Largest manifest scanned.
pub const MANIFEST_MAX: usize = 4 << 20;
/// Largest `pssh` box kept (the player's init data cap is 64 KiB).
pub const PSSH_MAX: usize = 16 << 10;
/// Distinct boxes kept.
pub const BOXES_MAX: usize = 8;

/// The DRM system a key system's `pssh` boxes are for.
pub fn system_id(key_system: &str) -> Option<[u8; 16]> {
    match key_system {
        "com.widevine.alpha" => Some(WIDEVINE),
        "org.w3.clearkey" => Some(COMMON),
        _ => None,
    }
}

/// ★ The distinct, well-formed `pssh` boxes for `system` in `mpd`, in manifest order.
pub fn pssh_boxes(mpd: &[u8], system: [u8; 16]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    if mpd.len() > MANIFEST_MAX {
        return out;
    }
    let want = uuid_text(system);
    let mut rest = mpd;
    while let Some(i) = find(rest, b"<ContentProtection") {
        rest = &rest[i + b"<ContentProtection".len()..];
        // The element's start tag, then its body up to the closing tag (none if `/>`).
        let Some(gt) = rest.iter().position(|&b| b == b'>') else { break };
        let tag = &rest[..gt];
        let body = if tag.last() == Some(&b'/') {
            &rest[..0]
        } else {
            let after = &rest[gt + 1..];
            &after[..find(after, b"</ContentProtection").unwrap_or(after.len())]
        };
        let scheme = attr(tag, b"schemeIdUri").map(|s| s.to_ascii_lowercase());
        if scheme.as_deref() == Some(want.as_bytes()) {
            let mut b = body;
            while let Some(s) = find_pssh_open(b) {
                b = &b[s..];
                let Some(e) = find(b, b"</") else { break };
                if let Some(bx) = decode_b64(&b[..e]).filter(|bx| valid_pssh(bx, system)) {
                    if !out.contains(&bx) && out.len() < BOXES_MAX {
                        out.push(bx);
                    }
                }
                b = &b[e..];
            }
        }
    }
    out
}

/// One base64 `pssh` box for `system`, as a caller passes it (EME's `initData`).
pub fn pssh_from_base64(s: &str, system: [u8; 16]) -> Option<Vec<u8>> {
    decode_b64(s.as_bytes()).filter(|b| valid_pssh(b, system))
}

/// After `<cenc:pssh>` or `<pssh>` (any prefix, attributes allowed): the text's start.
fn find_pssh_open(b: &[u8]) -> Option<usize> {
    let mut at = 0;
    while let Some(i) = find(&b[at..], b"pssh") {
        let i = at + i;
        let lt = b[..i].iter().rposition(|&c| c == b'<');
        let name_ok = lt.is_some_and(|lt| {
            let name = &b[lt + 1..i];
            name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-' || *c == b'.' || *c == b':')
                && (name.is_empty() || name.last() == Some(&b':'))
        });
        let tail = &b[i + 4..];
        if name_ok && matches!(tail.first(), Some(b'>' | b' ' | b'\t' | b'\r' | b'\n')) {
            let gt = tail.iter().position(|&c| c == b'>')?;
            if tail[..gt].last() != Some(&b'/') {
                return Some(i + 4 + gt + 1);
            }
        }
        at = i + 4;
    }
    None
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// `name="value"` or `name='value'` in a start tag, whole attribute names only.
fn attr<'a>(tag: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut at = 0;
    while let Some(i) = find(&tag[at..], name) {
        let i = at + i;
        let before_ok = i == 0 || tag[i - 1].is_ascii_whitespace();
        let mut j = i + name.len();
        while j < tag.len() && tag[j].is_ascii_whitespace() {
            j += 1;
        }
        if before_ok && tag.get(j) == Some(&b'=') {
            j += 1;
            while j < tag.len() && tag[j].is_ascii_whitespace() {
                j += 1;
            }
            let q = *tag.get(j)?;
            if q == b'"' || q == b'\'' {
                let v = &tag[j + 1..];
                return Some(&v[..v.iter().position(|&c| c == q)?]);
            }
            return None;
        }
        at = i + name.len();
    }
    None
}

fn uuid_text(id: [u8; 16]) -> String {
    let h: String = id.iter().map(|b| format!("{b:02x}")).collect();
    format!("urn:uuid:{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Standard base64, whitespace ignored, padding optional; `None` past [`PSSH_MAX`] bytes.
fn decode_b64(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut n, mut pad) = (0u32, 0u32, false);
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        if pad {
            return None;
        }
        acc = acc << 6 | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((acc >> n) as u8);
            if out.len() > PSSH_MAX {
                return None;
            }
        }
    }
    Some(out)
}

/// One whole `pssh` box (version 0 or 1) for `system`, its sizes consistent.
fn valid_pssh(b: &[u8], system: [u8; 16]) -> bool {
    if b.len() < 32 || &b[4..8] != b"pssh" || b[8] > 1 || b[12..28] != system {
        return false;
    }
    if u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize != b.len() {
        return false;
    }
    let mut at = 28;
    if b[8] == 1 {
        let Some(k) = b.get(28..32).map(|k| u32::from_be_bytes([k[0], k[1], k[2], k[3]]) as usize) else { return false };
        at = match k.checked_mul(16).and_then(|x| x.checked_add(32)) {
            Some(x) => x,
            None => return false,
        };
    }
    let Some(d) = b.get(at..at + 4).map(|d| u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize) else { return false };
    at.checked_add(4).and_then(|x| x.checked_add(d)) == Some(b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaka's angel-one manifest: the same Widevine box on every protected set.
    const BOX: &str = "AAAAPnBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAAB4iFnNoYWthX2NlYzJmNjRhYTc4OTBhMTFI49yVmwY=";

    fn mpd(body: &str) -> Vec<u8> {
        format!(r#"<?xml version="1.0"?><MPD xmlns:cenc="urn:mpeg:cenc:2013"><Period>{body}</Period></MPD>"#).into_bytes()
    }

    #[test]
    fn angel_one() {
        let m = mpd(&format!(
            r#"<AdaptationSet>
  <ContentProtection value="cenc" schemeIdUri="urn:mpeg:dash:mp4protection:2011" cenc:default_KID="800aacaa-5229-58ae-8880-62b5695db6bf"/>
  <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed">
    <cenc:pssh>{BOX}</cenc:pssh>
  </ContentProtection></AdaptationSet>
<AdaptationSet><ContentProtection schemeIdUri="urn:uuid:EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED" cenc:default_KID="x"><cenc:pssh>{BOX}</cenc:pssh></ContentProtection></AdaptationSet>"#
        ));
        let b = pssh_boxes(&m, WIDEVINE);
        assert_eq!(b.len(), 1, "one distinct box");
        assert_eq!(b[0].len(), 0x3e);
        assert!(valid_pssh(&b[0], WIDEVINE));
        assert!(pssh_boxes(&m, COMMON).is_empty());
    }

    #[test]
    fn other_systems_and_bad_boxes_are_skipped() {
        let pr = "urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95";
        let m = mpd(&format!(
            r#"<ContentProtection schemeIdUri="{pr}"><cenc:pssh>{BOX}</cenc:pssh></ContentProtection>
<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"><cenc:pssh>not base64!</cenc:pssh><cenc:pssh>AAAA</cenc:pssh><cenc:pssh/></ContentProtection>
<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>"#
        ));
        assert!(pssh_boxes(&m, WIDEVINE).is_empty());
    }

    #[test]
    fn a_box_for_another_system_inside_widevines_element_is_refused() {
        let mut b = decode_b64(BOX.as_bytes()).unwrap();
        b[12..28].copy_from_slice(&COMMON);
        let enc = encode(&b);
        let m = mpd(&format!(r#"<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"><pssh>{enc}</pssh></ContentProtection>"#));
        assert!(pssh_boxes(&m, WIDEVINE).is_empty());
    }

    #[test]
    fn size_fields_must_agree() {
        let good = decode_b64(BOX.as_bytes()).unwrap();
        assert!(valid_pssh(&good, WIDEVINE));
        for cut in [1, 4, 20] {
            assert!(!valid_pssh(&good[..good.len() - cut], WIDEVINE));
        }
        let mut v1 = good.clone();
        v1[8] = 1; // version 1 with no KID count: the data size no longer lines up
        assert!(!valid_pssh(&v1, WIDEVINE));
    }

    #[test]
    fn truncated_manifests_end_quietly() {
        let m = mpd(&format!(r#"<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"><cenc:pssh>{BOX}</cenc:pssh></ContentProtection>"#));
        for n in 0..m.len() {
            let _ = pssh_boxes(&m[..n], WIDEVINE);
        }
    }

    fn encode(b: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    }
}
