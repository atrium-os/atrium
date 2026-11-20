//! DKIM verification (RFC 6376, RFC 8463 Ed25519), DMARC alignment
//! (RFC 7489 §3.1) and reading a trusted `Authentication-Results`
//! (RFC 8601), for "who sent it" (docs/spec/mail.md §7.5).
//!
//! Runs in the parse jail, which has no network: the key records come from
//! mail-sync, which looks up only `TXT <selector>._domainkey.<domain>` and
//! `_dmarc.<domain>` through the system resolver. The caller passes them in
//! through a lookup function.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
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

const MAX_RAW_FIELDS: usize = 2000;

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
        if out.len() > MAX_RAW_FIELDS {
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
    let mut out = Vec::new();
    let mut blank_lines = 0;
    for raw in body.split_inclusive(|&byte| byte == b'\n') {
        let line = raw.strip_suffix(b"\n").unwrap_or(raw);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = if relaxed {
            let end = line.iter().rposition(|&byte| !is_wsp(byte)).map_or(0, |i| i + 1);
            &line[..end]
        } else {
            line
        };
        if line.is_empty() {
            blank_lines += 1;
            continue;
        }
        if out.is_empty() {
            out.reserve(body.len());
        }
        // A later nonempty line makes these internal, rather than trailing.
        for _ in 0..blank_lines {
            out.extend_from_slice(b"\r\n");
        }
        blank_lines = 0;
        if relaxed {
            let mut was_space = false;
            for &byte in line {
                let space = is_wsp(byte);
                if !space || !was_space {
                    out.push(if space { b' ' } else { byte });
                }
                was_space = space;
            }
        } else {
            out.extend_from_slice(line);
        }
        out.extend_from_slice(b"\r\n");
    }
    if out.is_empty() && !relaxed {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Keep whole-body hashes while examining the signatures on one message.
struct BodyHashes<'a> {
    raw: &'a [u8],
    full: [Option<(usize, digest::Digest)>; 2],
}

impl BodyHashes<'_> {
    fn hash(&mut self, relaxed: bool, limit: Option<usize>) -> Result<(digest::Digest, bool), Outcome> {
        let cached = &mut self.full[usize::from(relaxed)];
        if let Some((len, hash)) = *cached {
            let signed_len = limit.unwrap_or(len);
            if signed_len == len {
                return Ok((hash, false));
            }
            if signed_len > len {
                return Err(Outcome::Fail("body shorter than l="));
            }
        }
        let canonical = canon_body(self.raw, relaxed);
        let len = canonical.len();
        let signed = canonical.get(..limit.unwrap_or(len)).ok_or(Outcome::Fail("body shorter than l="))?;
        let hash = digest::digest(&digest::SHA256, signed);
        let partial = signed.len() < len;
        if !partial {
            *cached = Some((len, hash));
        }
        Ok((hash, partial))
    }
}

/// DKIM permits folding whitespace and optional padding. Reject an
/// invalid or oversized value in full.
fn decode_dkim_base64(value: &str, cap: usize) -> Option<Vec<u8>> {
    let limit = cap.div_ceil(3) * 4;
    let encoded: Vec<_> = value.bytes()
        .filter(|&byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .take(limit + 1)
        .collect();
    if encoded.is_empty() || encoded.len() > limit {
        return None;
    }
    let decoder = if encoded.ends_with(b"=") { &STANDARD } else { &STANDARD_NO_PAD };
    decoder.decode(&encoded).ok().filter(|bytes| bytes.len() <= cap)
}

/// Parse a bounded DKIM tag list (§3.2). Duplicate names invalidate the
/// list, including tags that are unknown to this verifier.
fn tags(raw: &[u8]) -> Option<Vec<(&str, String)>> {
    let text = std::str::from_utf8(raw).ok()?.trim_matches([' ', '\t', '\r', '\n']);
    let text = text.strip_suffix(';').unwrap_or(text);
    let mut tags = Vec::new();
    for tag in text.split(';') {
        let (name, value) = tag.split_once('=')?;
        let name = name.trim();
        let mut chars = name.bytes();
        if !chars.next().is_some_and(|b| b.is_ascii_alphabetic())
            || !chars.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || tags.len() == 64
            || tags.iter().any(|(seen, _)| *seen == name)
        {
            return None;
        }
        let value = match name {
            // The decoder checks whitespace inside the encoded value.
            "b" | "bh" | "p" => value.trim_matches([' ', '\t', '\r', '\n']).to_owned(),
            // These tags have tokens that must not be joined across whitespace.
            "c" | "d" | "h" | "q" | "s" | "t" | "x" => value.trim().to_owned(),
            _ => value.chars().filter(|c| !c.is_whitespace()).collect(),
        };
        tags.push((name, value));
    }
    Some(tags)
}

fn tag<'a>(t: &'a [(&str, String)], k: &str) -> Option<&'a str> {
    t.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str())
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
    let mut body = BodyHashes { raw: &raw[body_at.min(raw.len())..], full: [None; 2] };
    let mut out = Vec::new();
    for (idx, f) in fields.iter().enumerate() {
        if !f.name.eq_ignore_ascii_case(b"DKIM-Signature") {
            continue;
        }
        if out.len() >= 8 {
            break;
        }
        let colon = memchr(b':', f.bytes).unwrap_or(0);
        let Some(t) = tags(&f.bytes[colon + 1..]) else {
            out.push(Signature {
                domain: String::new(),
                selector: String::new(),
                outcome: Outcome::PermError("signature tags"),
                partial_body: false,
            });
            continue;
        };
        let domain = tag(&t, "d").unwrap_or("").to_ascii_lowercase();
        let selector = tag(&t, "s").unwrap_or("").to_string();
        let mut sig = Signature { domain: domain.clone(), selector: selector.clone(), outcome: Outcome::PermError("malformed"), partial_body: false };
        sig.outcome = check(&fields, idx, &t, &mut body, key, now, &mut sig.partial_body);
        out.push(sig);
    }
    out
}

fn dns_labels(name: &str) -> bool {
    name.len() <= 253 && name.split('.').all(|label| {
        label.len() <= 63
            && label.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && label.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
            && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

#[allow(clippy::too_many_arguments)]
fn check(fields: &[RawField], idx: usize, t: &[(&str, String)], body: &mut BodyHashes<'_>, key: &dyn Fn(&str, &str) -> Option<String>, now: i64, partial: &mut bool) -> Outcome {
    if tag(t, "v") != Some("1") {
        return Outcome::PermError("version");
    }
    let (Some(d), Some(s), Some(h), Some(bh), Some(b)) = (tag(t, "d"), tag(t, "s"), tag(t, "h"), tag(t, "bh"), tag(t, "b")) else {
        return Outcome::PermError("missing tag");
    };
    if !d.contains('.') || !dns_labels(d) || !dns_labels(s)
        || s.len() + "._domainkey.".len() + d.len() > 253
    {
        return Outcome::PermError("domain or selector");
    }
    let d = d.to_ascii_lowercase();
    let identity_domain = match tag(t, "i") {
        Some(identity) => {
            let Some((_, domain)) = identity.rsplit_once('@') else { return Outcome::PermError("signing identity") };
            let domain = domain.to_ascii_lowercase();
            if !domain.contains('.') || !dns_labels(&domain)
                || (domain != d && !domain.strip_suffix(&d).is_some_and(|prefix| prefix.ends_with('.')))
            {
                return Outcome::PermError("signing identity");
            }
            domain
        }
        None => d.clone(),
    };
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
    const MAX_SIGNED_HEADERS: usize = 64;
    let names: Vec<_> = h.split(':').map(str::trim).take(MAX_SIGNED_HEADERS + 1).collect();
    if names.len() > MAX_SIGNED_HEADERS || names.iter().any(|name| {
        name.is_empty() || !name.bytes().all(|byte| (33..=126).contains(&byte))
    }) {
        return Outcome::PermError("signed header list");
    }
    if !names.iter().any(|name| name.eq_ignore_ascii_case("From")) {
        return Outcome::PermError("From not signed");
    }
    if tag(t, "q").is_some_and(|methods| !methods.split(':').any(|method| method.trim() == "dns/txt")) {
        return Outcome::PermError("key lookup method");
    }
    let timestamp = |name| tag(t, name).map(|value| {
        if !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(());
        }
        value.parse::<u64>().map_err(|_| ())
    }).transpose();
    let (Ok(created), Ok(expiry)) = (timestamp("t"), timestamp("x")) else {
        return Outcome::PermError("signature timestamp");
    };
    if expiry.zip(created).is_some_and(|(x, t)| x <= t) {
        return Outcome::PermError("signature time range");
    }
    if expiry.is_some_and(|x| u64::try_from(now).is_ok_and(|now| x < now)) {
        return Outcome::PermError("expired");
    }
    let Some(body_hash) = decode_dkim_base64(bh, 32) else {
        return Outcome::PermError("body hash encoding");
    };
    let Some(sigbytes) = decode_dkim_base64(b, 1024) else {
        return Outcome::PermError("signature encoding");
    };
    let Ok(limit) = tag(t, "l").map(str::parse::<usize>).transpose() else {
        return Outcome::PermError("l=");
    };
    let (got, is_partial) = match body.hash(bc, limit) {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };
    *partial = is_partial;
    if body_hash != got.as_ref() {
        return Outcome::Fail("body hash");
    }
    // the key
    let Some(rec) = key(s, &d) else { return Outcome::NoKey };
    if rec.is_empty() {
        return Outcome::Fail("key absent");
    }
    let Some(kt) = tags(rec.as_bytes()) else { return Outcome::PermError("key tags") };
    if tag(&kt, "v").is_some() && !kt.first().is_some_and(|(name, v)| *name == "v" && v == "DKIM1") {
        return Outcome::PermError("key version");
    }
    let Some(p) = tag(&kt, "p") else { return Outcome::PermError("key tags") };
    if p.is_empty() {
        return Outcome::Fail("key revoked");
    }
    let k = tag(&kt, "k").unwrap_or("rsa");
    if !matches!((k, alg), ("rsa", "rsa-sha256") | ("ed25519", "ed25519-sha256")) {
        return Outcome::PermError("key type");
    }
    // Both supported signature algorithms use SHA-256.
    if tag(&kt, "h").is_some_and(|hashes| !hashes.split(':').any(|hash| hash.trim() == "sha256")) {
        return Outcome::PermError("key hash");
    }
    if tag(&kt, "s").is_some_and(|services| !services.split(':').any(|service| matches!(service.trim(), "*" | "email"))) {
        return Outcome::PermError("key service");
    }
    if identity_domain != d && tag(&kt, "t").is_some_and(|flags| flags.split(':').any(|flag| flag.trim() == "s")) {
        return Outcome::PermError("key signing identity");
    }
    let Some(pk) = decode_dkim_base64(p, 4096) else { return Outcome::PermError("key encoding") };
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
    /// Waiting for a key or alignment policy.
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

/// The verdict for one message. `alignment` is true for strict, false for
/// relaxed, and None when Mail could not establish the mode. `provider`
/// is the Authentication-Results result from the account's own server, if the
/// message carried one (see [`provider_result`]).
pub fn verdict(from_domain: &str, sigs: &[Signature], alignment: Option<bool>, provider: Option<&ProviderResult>) -> Verdict {
    if from_domain.is_empty() {
        return Verdict::NotVerified;
    }
    let passed = |strict| {
        sigs.iter().any(|s| s.outcome == Outcome::Pass && !s.partial_body && aligned(from_domain, &s.domain, strict))
    };
    if passed(alignment.unwrap_or(true)) {
        return Verdict::Verified { domain: from_domain.to_ascii_lowercase(), by_provider: false };
    }
    if alignment.is_none() && passed(false) {
        return Verdict::Pending;
    }
    let strict = alignment.unwrap_or(false);
    let missing_key = sigs.iter().any(|s| s.outcome == Outcome::NoKey && aligned(from_domain, &s.domain, strict));
    let local_failure = sigs.iter().any(|s| {
        aligned(from_domain, &s.domain, strict)
            && (s.partial_body || matches!(s.outcome, Outcome::Fail(_)))
    });
    if let Some(p) = provider.filter(|_| !local_failure && (sigs.is_empty() || missing_key)) {
        if p.dmarc_pass && p.header_from.eq_ignore_ascii_case(from_domain) {
            return Verdict::Verified { domain: from_domain.to_ascii_lowercase(), by_provider: true };
        }
    }
    if missing_key {
        return Verdict::Pending;
    }
    Verdict::NotVerified
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResult {
    pub dmarc_pass: bool,
    pub header_from: String,
}

/// A single From field with the spelling the message parser recognizes.
pub(crate) fn has_single_from(raw: &[u8]) -> bool {
    let (fields, _) = raw_fields(raw);
    single_from(&fields)
}

fn single_from(fields: &[RawField<'_>]) -> bool {
    // A truncated header block cannot establish uniqueness.
    if fields.len() > MAX_RAW_FIELDS {
        return false;
    }
    // Also count obsolete From fields with whitespace before the colon. The
    // message parser skips those, but the DKIM field selector can match them.
    let mut from = fields.iter().filter(|f| {
        std::str::from_utf8(f.name).is_ok_and(|name| name.trim().eq_ignore_ascii_case("From"))
    });
    from.next().is_some_and(|f| f.name.eq_ignore_ascii_case(b"From")) && from.next().is_none()
}

/// The first Authentication-Results header. The caller must supply a trusted
/// authserv-id from outside the message. Never search past an untrusted field.
pub fn provider_result(raw: &[u8], authserv_id: &str) -> Option<ProviderResult> {
    let (fields, _) = raw_fields(raw);
    if !single_from(&fields) {
        return None;
    }
    let field = fields.iter().find(|f| f.name.eq_ignore_ascii_case(b"Authentication-Results"))?;
    let colon = memchr(b':', field.bytes)?;
    let raw = field.bytes[colon + 1..].strip_suffix(b"\n").unwrap_or(&field.bytes[colon + 1..]);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let parts = auth_result_parts(raw)?;
    let mut identity = parts.first()?.split_ascii_whitespace();
    if !identity.next()?.eq_ignore_ascii_case(authserv_id)
        || !matches!(identity.next(), None | Some("1"))
        || identity.next().is_some()
    {
        return None;
    }
    let mut result = None;
    for part in &parts[1..] {
        let mut words = part.split_ascii_whitespace();
        let Some(method_result) = words.next() else { return None };
        let (method, value) = method_result.split_once('=')?;
        let (name, version) = method.split_once('/').unwrap_or((method, "1"));
        if !name.eq_ignore_ascii_case("dmarc") {
            continue;
        }
        if version != "1" || result.is_some() {
            return None;
        }
        let mut from = None;
        for word in words {
            let (key, value) = word.split_once('=')?;
            if key.eq_ignore_ascii_case("header.from") {
                if from.is_some() {
                    return None;
                }
                let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
                if !auth_result_domain(value) {
                    return None;
                }
                from = Some(value.to_ascii_lowercase());
            }
        }
        result = Some(ProviderResult {
            dmarc_pass: value.eq_ignore_ascii_case("pass"),
            header_from: from?,
        });
    }
    result
}

fn auth_result_domain(name: &str) -> bool {
    name.len() <= 253 && name.contains('.') && name.split('.').all(|label| {
        !label.is_empty() && label.len() <= 63
            && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Split a bounded ASCII trace field on real semicolons. Remove comments
/// without promoting their contents to properties. Reject unsupported forms.
fn auth_result_parts(raw: &[u8]) -> Option<Vec<String>> {
    if raw.len() > 16 * 1024 || !raw.is_ascii() {
        return None;
    }
    let mut raw = raw.iter().copied().peekable();
    let mut parts = vec![String::new()];
    let mut comment = 0u8;
    let mut quoted = false;
    while let Some(mut b) = raw.next() {
        if b == b'\r' || b == b'\n' {
            if b == b'\r' && raw.next()? != b'\n' { return None; }
            if !raw.peek().copied().is_some_and(is_wsp) { return None; }
            b = b' ';
        }
        if !(is_wsp(b) || b.is_ascii_graphic()) || (quoted && is_wsp(b)) {
            return None;
        }
        let output = parts.last_mut()?;
        if b == b'\\' && (comment > 0 || quoted) {
            let escaped = raw.next()?;
            if !escaped.is_ascii_graphic() && !is_wsp(escaped) { return None; }
            if quoted { return None; }
            continue;
        }
        if !quoted && b == b'(' {
            if comment == 0 { output.push(' '); }
            comment = comment.checked_add(1)?;
        } else if !quoted && b == b')' {
            comment = comment.checked_sub(1)?;
        } else if comment == 0 {
            if b == b';' && !quoted {
                parts.push(String::new());
            } else {
                if b == b'"' { quoted = !quoted; }
                output.push(b as char);
            }
        }
    }
    (comment == 0 && !quoted).then_some(parts)
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
    fn body_canonicalization_preserves_internal_blank_lines() {
        let body = b"\n  A \tB \r\n \t\nZ\n\t \r\n\n";
        assert_eq!(canon_body(body, false), b"\r\n  A \tB \r\n \t\r\nZ\r\n\t \r\n");
        assert_eq!(canon_body(body, true), b"\r\n A B\r\n\r\nZ\r\n");
        assert_eq!(canon_body(b"body", false), b"body\r\n");
        assert_eq!(canon_body(b"body", true), b"body\r\n");
        assert!(canon_body(&b" \t\n".repeat(100_000), true).is_empty());
    }

    #[test]
    fn body_hashes_reuse_only_full_hashes_in_the_same_mode() {
        let mut body = BodyHashes { raw: b"A  B\r\n", full: [None; 2] };
        for (relaxed, expected) in [(false, &b"A  B\r\n"[..]), (true, &b"A B\r\n"[..])] {
            let algorithm = &digest::SHA256;
            let (full, partial) = body.hash(relaxed, None).unwrap();
            assert!(!partial);
            assert_eq!(full.as_ref(), digest::digest(algorithm, expected).as_ref());
            let (prefix, partial) = body.hash(relaxed, Some(1)).unwrap();
            assert!(partial);
            assert_eq!(prefix.as_ref(), digest::digest(algorithm, b"A").as_ref());
            let exact = body.hash(relaxed, Some(expected.len())).unwrap();
            assert!(!exact.1);
            assert_eq!(exact.0.as_ref(), full.as_ref());
            assert!(matches!(
                body.hash(relaxed, Some(expected.len() + 1)),
                Err(Outcome::Fail("body shorter than l="))
            ));
        }
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
    #[test]
    fn provider_accepts_a_folded_result_with_comments_and_one_domain() {
        let raw = b"Authentication-Results: edge.example 1;\r\n dmarc/1=pass (receiver note; policy=accept) HEADER.FROM=\"Example.com\"\r\nFrom: A <a@example.com>\r\n\r\n";
        assert_eq!(
            provider_result(raw, "edge.example"),
            Some(ProviderResult { dmarc_pass: true, header_from: "example.com".into() })
        );
    }

    #[test]
    fn provider_ignores_ambiguous_fields() {
        for result in [
            "outside.example; dmarc=pass header.from=example.com\r\nAuthentication-Results: edge.example; dmarc=pass header.from=example.com",
            "edge.example; dmarc=pass header.from=example.com header.from=other.example",
            "edge.example; dmarc=pass header.from=example.com; dmarc=fail header.from=example.com",
            "edge.example; dmarc=pass (header.from=example.com)",
            "edge.example; dmarc=pass header.from=example.com (unfinished",
            "edge.example; dmarc=pass header.from=example.com\r\nFrom: B <b@example.com>",
        ] {
            let raw = format!("Authentication-Results: {result}\r\nFrom: A <a@example.com>\r\n\r\n");
            assert_eq!(provider_result(raw.as_bytes(), "edge.example"), None, "{result}");
        }
    }

    #[test]
    fn local_verification_takes_precedence_over_a_provider_pass() {
        let provider = ProviderResult { dmarc_pass: true, header_from: "example.com".into() };
        let mut sig = Signature {
            domain: "example.com".into(),
            selector: "sel".into(),
            outcome: Outcome::NoKey,
            partial_body: false,
        };
        let check = |sig: &Signature| verdict("example.com", std::slice::from_ref(sig), None, Some(&provider));
        assert!(matches!(check(&sig), Verdict::Verified { by_provider: true, .. }));
        sig.outcome = Outcome::Fail("body hash");
        assert_eq!(check(&sig), Verdict::NotVerified);
        sig.outcome = Outcome::Pass;
        assert!(matches!(check(&sig), Verdict::Verified { by_provider: false, .. }));
        sig.partial_body = true;
        assert_eq!(check(&sig), Verdict::NotVerified);
        assert_eq!(verdict("other.example", &[], None, Some(&provider)), Verdict::NotVerified);
    }

}
