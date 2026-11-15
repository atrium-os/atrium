//! DKIM verification (RFC 6376, RFC 8463 Ed25519), DMARC alignment
//! (RFC 7489 §3.1) and reading a trusted `Authentication-Results`
//! (RFC 8601), for "who sent it" (docs/spec/mail.md §7.5).
//!
//! Runs in the parse jail, which has no network: the key records come from
//! mail-sync, which looks up only `TXT <selector>._domainkey.<domain>` and
//! `_dmarc.<domain>` through the system resolver. The caller passes them in
//! through a lookup function.

use crate::decode::base64;
use crate::header::memchr;
use ring::{digest, signature};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// The body hash or signature didn't match.
    Fail(&'static str),
    /// The signature can't be checked: malformed, unsupported or expired.
    PermError(&'static str),
    /// No key record (yet): the engine asks sync to look it up, then
    /// verifies again.
    NoKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// `d=`, lower case.
    pub domain: String,
    /// `s=`.
    pub selector: String,
    pub outcome: Outcome,
    /// `l=` was used and the body is longer than what was signed: text can
    /// have been added after signing. Counted as a failure for display.
    pub partial_body: bool,
}

/// One header field as it was written, folds and line end included.
struct RawField<'a> {
    name: &'a [u8],
    bytes: &'a [u8],
}

fn raw_fields(raw: &[u8]) -> (Vec<RawField<'_>>, usize) {
    let mut out: Vec<RawField> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let end = memchr(b'\n', &raw[i..]).map(|n| i + n + 1).unwrap_or(raw.len());
        let line = &raw[i..end];
        let bare = line.strip_suffix(b"\n").unwrap_or(line);
        let bare = bare.strip_suffix(b"\r").unwrap_or(bare);
        if bare.is_empty() {
            return (out, end);
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some(f) = out.last_mut() {
                let start = f.bytes.as_ptr() as usize - raw.as_ptr() as usize;
                f.bytes = &raw[start..end];
            }
        } else if let Some(c) = memchr(b':', bare) {
            out.push(RawField { name: &bare[..c], bytes: &raw[i..end] });
        }
        i = end;
        if out.len() > 2000 {
            break;
        }
    }
    (out, raw.len())
}

fn is_wsp(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// Header canonicalization (§3.4.1 simple, §3.4.2 relaxed), with CRLF.
fn canon_header(f: &[u8], relaxed: bool) -> Vec<u8> {
    if !relaxed {
        // simple: as is, but line ends made CRLF
        return crlf(f);
    }
    let colon = memchr(b':', f).unwrap_or(f.len());
    let mut out: Vec<u8> = f[..colon].iter().map(|c| c.to_ascii_lowercase()).filter(|c| !is_wsp(*c)).collect();
    out.push(b':');
    let mut val = Vec::new();
    let mut ws = false;
    for &c in &f[(colon + 1).min(f.len())..] {
        if c == b'\r' || c == b'\n' {
            continue; // unfold
        }
        if is_wsp(c) {
            ws = true;
            continue;
        }
        if ws && !val.is_empty() {
            val.push(b' ');
        }
        ws = false;
        val.push(c);
    }
    out.extend_from_slice(&val);
    out.extend_from_slice(b"\r\n");
    out
}

fn crlf(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() + 8);
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' && (i == 0 || b[i - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(c);
    }
    out
}

/// Body canonicalization (§3.4.3, §3.4.4).
fn canon_body(body: &[u8], relaxed: bool) -> Vec<u8> {
    let body = crlf(body);
    let mut lines: Vec<Vec<u8>> = body.split(|&c| c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec()).collect();
    // the split leaves an empty last element after a final CRLF
    if body.ends_with(b"\n") {
        lines.pop();
    }
    if relaxed {
        for l in lines.iter_mut() {
            let mut out = Vec::with_capacity(l.len());
            let mut ws = false;
            for &c in l.iter() {
                if is_wsp(c) {
                    ws = true;
                } else {
                    if ws {
                        out.push(b' ');
                    }
                    ws = false;
                    out.push(c);
                }
            }
            *l = out; // trailing whitespace dropped
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return if relaxed { Vec::new() } else { b"\r\n".to_vec() };
    }
    let mut out = Vec::with_capacity(body.len());
    for l in lines {
        out.extend_from_slice(&l);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// `tag=value; …` lists (§3.2), whitespace removed from values.
fn tags(v: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(v);
    s.split(';')
        .filter_map(|t| {
            let (k, v) = t.split_once('=')?;
            let k = k.trim().to_string();
            let v: String = v.chars().filter(|c| !c.is_whitespace()).collect();
            (!k.is_empty()).then_some((k, v))
        })
        .take(64)
        .collect()
}

fn tag<'a>(t: &'a [(String, String)], k: &str) -> Option<&'a str> {
    t.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

/// The DKIM-Signature field with its `b=` value removed (§3.7).
fn without_b(field: &[u8]) -> Vec<u8> {
    let colon = memchr(b':', field).unwrap_or(0);
    let mut out = field[..=colon.min(field.len().saturating_sub(1))].to_vec();
    let value = &field[(colon + 1).min(field.len())..];
    let mut i = 0;
    let mut tag_start = true;
    while i < value.len() {
        let c = value[i];
        if tag_start && !is_wsp(c) && c != b'\r' && c != b'\n' {
            // is this the b tag (not bh)?
            let mut j = i;
            if value[j] == b'b' {
                j += 1;
                while j < value.len() && (is_wsp(value[j]) || value[j] == b'\r' || value[j] == b'\n') {
                    j += 1;
                }
                if j < value.len() && value[j] == b'=' {
                    out.extend_from_slice(&value[i..=j]);
                    let end = value[j..].iter().position(|&c| c == b';').map(|p| j + p).unwrap_or(value.len());
                    // keep the field's own line end if b= was the last tag
                    if end == value.len() && value.ends_with(b"\n") {
                        out.extend_from_slice(b"\r\n");
                    }
                    i = end;
                    tag_start = false;
                    continue;
                }
            }
            tag_start = false;
        }
        if c == b';' {
            tag_start = true;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Verifies every DKIM-Signature on the message (at most 8). `key` returns
/// the TXT record for `(selector, domain)` if sync has it.
pub fn verify(raw: &[u8], key: &dyn Fn(&str, &str) -> Option<String>, now: i64) -> Vec<Signature> {
    let (fields, body_at) = raw_fields(raw);
    let body = &raw[body_at.min(raw.len())..];
    let mut out = Vec::new();
    for (idx, f) in fields.iter().enumerate() {
        if !f.name.eq_ignore_ascii_case(b"DKIM-Signature") {
            continue;
        }
        if out.len() >= 8 {
            break;
        }
        let colon = memchr(b':', f.bytes).unwrap_or(0);
        let t = tags(&f.bytes[colon + 1..]);
        let domain = tag(&t, "d").unwrap_or("").to_ascii_lowercase();
        let selector = tag(&t, "s").unwrap_or("").to_string();
        let mut sig = Signature { domain: domain.clone(), selector: selector.clone(), outcome: Outcome::PermError("malformed"), partial_body: false };
        sig.outcome = check(raw, &fields, idx, &t, body, key, now, &mut sig.partial_body);
        out.push(sig);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn check(raw: &[u8], fields: &[RawField], idx: usize, t: &[(String, String)], body: &[u8], key: &dyn Fn(&str, &str) -> Option<String>, now: i64, partial: &mut bool) -> Outcome {
    let _ = raw;
    if tag(t, "v") != Some("1") {
        return Outcome::PermError("version");
    }
    let (Some(d), Some(s), Some(h), Some(bh), Some(b)) = (tag(t, "d"), tag(t, "s"), tag(t, "h"), tag(t, "bh"), tag(t, "b")) else {
        return Outcome::PermError("missing tag");
    };
    if d.is_empty() || s.is_empty() || !d.contains('.') || d.len() > 253 || s.len() > 63 {
        return Outcome::PermError("domain or selector");
    }
    let alg = tag(t, "a").unwrap_or("");
    if alg != "rsa-sha256" && alg != "ed25519-sha256" {
        return Outcome::PermError("algorithm"); // rsa-sha1 is not accepted
    }
    let (hc, bc) = match tag(t, "c").unwrap_or("simple/simple") {
        "simple" | "simple/simple" => (false, false),
        "relaxed" | "relaxed/simple" => (true, false),
        "relaxed/relaxed" => (true, true),
        "simple/relaxed" => (false, true),
        _ => return Outcome::PermError("canonicalization"),
    };
    let names: Vec<String> = h.split(':').map(|n| n.trim().to_ascii_lowercase()).filter(|n| !n.is_empty()).take(64).collect();
    if !names.iter().any(|n| n == "from") {
        return Outcome::PermError("From not signed");
    }
    if let Some(x) = tag(t, "x").and_then(|x| x.parse::<i64>().ok()) {
        if x < now {
            return Outcome::PermError("expired");
        }
    }
    // the body
    let mut cb = canon_body(body, bc);
    if let Some(l) = tag(t, "l") {
        let Ok(l) = l.parse::<usize>() else { return Outcome::PermError("l=") };
        if l > cb.len() {
            return Outcome::Fail("body shorter than l=");
        }
        if l < cb.len() {
            *partial = true;
            cb.truncate(l);
        }
    }
    let got = digest::digest(&digest::SHA256, &cb);
    if base64(bh.as_bytes(), 64) != got.as_ref() {
        return Outcome::Fail("body hash");
    }
    // the key
    let Some(rec) = key(s, &d.to_ascii_lowercase()) else { return Outcome::NoKey };
    let kt = tags(rec.as_bytes());
    if tag(&kt, "v").is_some_and(|v| v != "DKIM1") {
        return Outcome::PermError("key version");
    }
    let p = tag(&kt, "p").unwrap_or("");
    if p.is_empty() {
        return Outcome::Fail("key revoked");
    }
    let k = tag(&kt, "k").unwrap_or("rsa");
    if (k == "rsa") != (alg == "rsa-sha256") {
        return Outcome::PermError("key type");
    }
    let pk = base64(p.as_bytes(), 4096);
    // the signed header data: chosen fields bottom-up, then this signature without b=
    let mut data = Vec::new();
    let mut used = vec![false; fields.len()];
    for n in &names {
        if let Some(j) = (0..fields.len()).rev().find(|&j| !used[j] && j != idx && String::from_utf8_lossy(fields[j].name).trim().eq_ignore_ascii_case(n)) {
            used[j] = true;
            data.extend_from_slice(&canon_header(fields[j].bytes, hc));
        }
    }
    let mut me = canon_header(&without_b(fields[idx].bytes), hc);
    while me.ends_with(b"\n") || me.ends_with(b"\r") {
        me.pop();
    }
    data.extend_from_slice(&me);
    let sigbytes = base64(b.as_bytes(), 1024);
    let ok = if alg == "rsa-sha256" {
        let Some(rsa) = rsa_key(&pk) else { return Outcome::PermError("key") };
        signature::UnparsedPublicKey::new(&signature::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, rsa).verify(&data, &sigbytes).is_ok()
    } else {
        if pk.len() != 32 {
            return Outcome::PermError("key");
        }
        let h = digest::digest(&digest::SHA256, &data);
        signature::UnparsedPublicKey::new(&signature::ED25519, &pk).verify(h.as_ref(), &sigbytes).is_ok()
    };
    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail("signature")
    }
}

/// The PKCS#1 RSAPublicKey inside a SubjectPublicKeyInfo (or the bare
/// RSAPublicKey some records publish).
fn rsa_key(der: &[u8]) -> Option<&[u8]> {
    fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *b.first()?;
        let l0 = *b.get(1)? as usize;
        let (len, hdr) = if l0 < 0x80 {
            (l0, 2)
        } else {
            let n = l0 & 0x7f;
            if n == 0 || n > 3 {
                return None;
            }
            let mut len = 0usize;
            for i in 0..n {
                len = (len << 8) | *b.get(2 + i)? as usize;
            }
            (len, 2 + n)
        };
        let body = b.get(hdr..hdr + len)?;
        Some((tag, body, &b[hdr + len..]))
    }
    let (t, outer, _) = tlv(der)?;
    if t != 0x30 {
        return None;
    }
    let (t1, _first, rest) = tlv(outer)?;
    if t1 == 0x02 {
        return Some(der); // bare RSAPublicKey: SEQUENCE { INTEGER n, INTEGER e }
    }
    // SPKI: SEQUENCE { AlgorithmIdentifier, BIT STRING }
    let (t2, bits, _) = tlv(rest)?;
    if t2 != 0x03 || bits.first() != Some(&0) {
        return None;
    }
    Some(&bits[1..])
}

// ------------------------------------------------------------------ DMARC

/// What Mail shows next to the sender (mail.md §7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The From domain is proven: an aligned DKIM pass, or a DMARC pass in
    /// the account provider's own Authentication-Results.
    Verified { domain: String, by_provider: bool },
    /// The checks failed, or nothing could be checked.
    NotVerified,
    /// Waiting for key records from sync.
    Pending,
}

/// The organizational domain (RFC 7489 §3.2) from the Public Suffix List.
pub fn org_domain(d: &str) -> String {
    let d = d.trim_end_matches('.').to_ascii_lowercase();
    match psl::domain_str(&d) {
        Some(o) => o.to_string(),
        None => d,
    }
}

/// `adkim` from the DMARC record: strict alignment needs the same domain,
/// relaxed (the default) the same organizational domain.
pub fn aligned(from_domain: &str, dkim_domain: &str, strict: bool) -> bool {
    let f = from_domain.trim_end_matches('.').to_ascii_lowercase();
    let k = dkim_domain.trim_end_matches('.').to_ascii_lowercase();
    if strict {
        f == k
    } else {
        org_domain(&f) == org_domain(&k)
    }
}

/// The verdict for one message. `dmarc_record` is the TXT at
/// `_dmarc.<from domain>` (or its organizational domain); `provider` is
/// the Authentication-Results result from the account's own server, if the
/// message carried one (see [`provider_result`]).
pub fn verdict(from_domain: &str, sigs: &[Signature], dmarc_record: Option<&str>, provider: Option<&ProviderResult>) -> Verdict {
    if from_domain.is_empty() {
        return Verdict::NotVerified;
    }
    let strict = dmarc_record.map(|r| tags(r.as_bytes())).map(|t| tag(&t, "adkim") == Some("s")).unwrap_or(false);
    if sigs.iter().any(|s| s.outcome == Outcome::Pass && !s.partial_body && aligned(from_domain, &s.domain, strict)) {
        return Verdict::Verified { domain: from_domain.to_ascii_lowercase(), by_provider: false };
    }
    if let Some(p) = provider {
        if p.dmarc_pass && p.header_from.eq_ignore_ascii_case(from_domain) {
            return Verdict::Verified { domain: from_domain.to_ascii_lowercase(), by_provider: true };
        }
    }
    if sigs.iter().any(|s| s.outcome == Outcome::NoKey && aligned(from_domain, &s.domain, strict)) {
        return Verdict::Pending;
    }
    Verdict::NotVerified
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResult {
    pub dmarc_pass: bool,
    pub header_from: String,
}

/// The topmost `Authentication-Results` whose authserv-id is the account
/// provider's (`authserv_id`, configured per account; RFC 8601 §5). Any
/// other instance, or one below a forged copy, is ignored.
pub fn provider_result(raw: &[u8], authserv_id: &str) -> Option<ProviderResult> {
    let (fields, _) = raw_fields(raw);
    let f = fields.iter().find(|f| f.name.eq_ignore_ascii_case(b"Authentication-Results"))?;
    let colon = memchr(b':', f.bytes)?;
    let v = String::from_utf8_lossy(&f.bytes[colon + 1..]);
    let v: String = v.chars().map(|c| if c == '\r' || c == '\n' { ' ' } else { c }).collect();
    let mut parts = v.split(';');
    let id = parts.next()?.split_whitespace().next()?.to_ascii_lowercase();
    if id != authserv_id.to_ascii_lowercase() {
        return None;
    }
    for p in parts {
        let p = p.trim();
        let mut words = p.split_whitespace();
        let Some(first) = words.next() else { continue };
        if first.to_ascii_lowercase().starts_with("dmarc=") {
            let pass = first.eq_ignore_ascii_case("dmarc=pass");
            let from = words.find_map(|w| w.strip_prefix("header.from=")).unwrap_or("").trim_matches(|c| c == '"' || c == ';').to_string();
            return Some(ProviderResult { dmarc_pass: pass, header_from: from });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_canon_rfc_examples() {
        // RFC 6376 §3.4.5
        let body = b" C \r\nD \t E\r\n\r\n\r\n";
        assert_eq!(canon_body(body, true), b" C\r\nD E\r\n");
        assert_eq!(canon_body(body, false), b" C \r\nD \t E\r\n");
        assert_eq!(canon_body(b"", false), b"\r\n");
        assert_eq!(canon_body(b"", true), b"");
        assert_eq!(canon_header(b"A: X\r\n", true), b"a:X\r\n");
        assert_eq!(canon_header(b"B : Y\t\r\n\tZ  \r\n", true), b"b:Y Z\r\n");
    }

    #[test]
    fn strip_b() {
        let f = b"DKIM-Signature: v=1; a=rsa-sha256; bh=abc=;\r\n b=dGVz\r\n dA==\r\n";
        assert_eq!(without_b(f), b"DKIM-Signature: v=1; a=rsa-sha256; bh=abc=;\r\n b=\r\n");
        let g = b"DKIM-Signature: b=xyz; bh=abc; d=x\r\n";
        assert_eq!(without_b(g), b"DKIM-Signature: b=; bh=abc; d=x\r\n");
    }

    #[test]
    fn alignment() {
        assert!(aligned("mail.example.co.uk", "example.co.uk", false));
        assert!(!aligned("mail.example.co.uk", "example.co.uk", true));
        assert!(!aligned("example.com", "example.net", false));
        assert!(!aligned("a.github.io", "b.github.io", false));
    }

    #[test]
    fn provider_ar() {
        let raw = b"Authentication-Results: mx.google.com;\r\n       dkim=pass header.i=@example.com;\r\n       dmarc=pass (p=REJECT) header.from=example.com\r\nAuthentication-Results: evil.example; dmarc=pass header.from=bank.example\r\nFrom: a@example.com\r\n\r\nx";
        assert_eq!(provider_result(raw, "mx.google.com"), Some(ProviderResult { dmarc_pass: true, header_from: "example.com".into() }));
        assert_eq!(provider_result(raw, "evil.example"), None);
    }
}
