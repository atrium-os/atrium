//! HTTP Digest (RFC 7616) for an already identified, pinned IPPS printer.
use std::collections::HashMap;
use std::fmt::Write;

use base64::Engine;
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Hash {
    Md5,
    Sha256,
    Sha512_256,
}

impl Hash {
    fn hex(self, fields: &[&[u8]]) -> Zeroizing<String> {
        fn hash<D: sha2::Digest>(fields: &[&[u8]]) -> Zeroizing<String> {
            let mut h = D::new();
            for (i, field) in fields.iter().enumerate() {
                if i != 0 {
                    h.update(b":");
                }
                h.update(field);
            }
            let mut output = Zeroizing::new(String::with_capacity(<D as sha2::Digest>::output_size() * 2));
            let mut digest = h.finalize();
            for byte in &digest {
                write!(*output, "{byte:02x}").unwrap();
            }
            digest.as_mut_slice().zeroize();
            output
        }
        match self {
            Self::Md5 => hash::<md5::Md5>(fields),
            Self::Sha256 => hash::<sha2::Sha256>(fields),
            Self::Sha512_256 => hash::<sha2::Sha512_256>(fields),
        }
    }
}

pub(crate) enum Scheme {
    Basic,
    Digest(Digest),
}

pub(crate) struct Digest {
    hash: Hash,
    algorithm: &'static str,
    session: bool,
    qop: bool,
    realm: String,
    nonce: String,
    opaque: Option<String>,
    count: u32,
}

fn quoted(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\') {
            result.push('\\');
        }
        result.push(c);
    }
    result.push('"');
    result
}

impl Scheme {
    pub(crate) fn choose(status: u16, headers: &[(String, String)]) -> Option<Self> {
        let mut best: Option<Digest> = None;
        let mut basic = false;
        for (_, line) in headers.iter().filter(|(name, _)| name == "www-authenticate") {
            for (name, params) in challenges(line) {
                if name.eq_ignore_ascii_case("Digest") {
                    if let Some(digest) = Digest::parse(&params) {
                        if best.as_ref().is_none_or(|old| (digest.hash, digest.qop, digest.session) > (old.hash, old.qop, old.session)) {
                            best = Some(digest);
                        }
                    }
                } else if name.eq_ignore_ascii_case("Basic") {
                    basic = true;
                }
            }
        }
        best.map(Self::Digest).or_else(|| {
            // Older IPPS printers accept an anonymous Validate-Job but still
            // expect Basic on job requests. Keep that behavior in the absence
            // of an advertised challenge.
            (basic || (status != 401 && !headers.iter().any(|(name, _)| name == "www-authenticate")))
                .then_some(Self::Basic)
        })
    }

    pub(crate) fn header(&mut self, secret: &[u8], path: &str) -> Option<Zeroizing<String>> {
        match self {
            Self::Basic => {
                let mut value = Zeroizing::new(String::with_capacity(6 + secret.len().div_ceil(3) * 4));
                value.push_str("Basic ");
                base64::engine::general_purpose::STANDARD.encode_string(secret, &mut *value);
                Some(value)
            }
            Self::Digest(digest) => digest.header(secret, path),
        }
    }
}

impl Digest {
    fn parse(params: &HashMap<String, String>) -> Option<Self> {
        let (hash, algorithm, session) = match params.get("algorithm").map(|a| a.to_ascii_lowercase()).as_deref() {
            None | Some("md5") => (Hash::Md5, "MD5", false),
            Some("md5-sess") => (Hash::Md5, "MD5-sess", true),
            Some("sha-256") => (Hash::Sha256, "SHA-256", false),
            Some("sha-256-sess") => (Hash::Sha256, "SHA-256-sess", true),
            Some("sha-512-256") => (Hash::Sha512_256, "SHA-512-256", false),
            Some("sha-512-256-sess") => (Hash::Sha512_256, "SHA-512-256-sess", true),
            _ => return None,
        };
        if params.get("userhash").is_some_and(|v| !v.eq_ignore_ascii_case("false")) {
            return None;
        }
        let qop = params.get("qop");
        if qop.is_some_and(|v| !v.split(',').any(|q| q.trim().eq_ignore_ascii_case("auth"))) {
            return None;
        }
        let nonce = params.get("nonce")?.clone();
        if nonce.is_empty() {
            return None;
        }
        Some(Self { hash, algorithm, session, qop: qop.is_some(), realm: params.get("realm")?.clone(),
            nonce, opaque: params.get("opaque").cloned(), count: 0 })
    }

    fn header(&mut self, secret: &[u8], path: &str) -> Option<Zeroizing<String>> {
        let separator = secret.iter().position(|&byte| byte == b':')?;
        let user = std::str::from_utf8(&secret[..separator]).ok()?;
        let password = &secret[separator + 1..];
        self.count = self.count.checked_add(1)?;
        let nc = format!("{:08x}", self.count);
        let mut random = [0u8; 16];
        rustls::crypto::ring::default_provider().secure_random.fill(&mut random).ok()?;
        let cnonce: String = random.iter().fold(String::with_capacity(32), |mut out, byte| {
            write!(out, "{byte:02x}").unwrap();
            out
        });
        let nonce = self.nonce.as_bytes();
        let mut a1 = self.hash.hex(&[user.as_bytes(), self.realm.as_bytes(), password]);
        if self.session {
            a1 = self.hash.hex(&[a1.as_bytes(), nonce, cnonce.as_bytes()]);
        }
        let a2 = self.hash.hex(&[b"POST", path.as_bytes()]);
        let response = if self.qop {
            self.hash.hex(&[a1.as_bytes(), nonce, nc.as_bytes(), cnonce.as_bytes(), b"auth", a2.as_bytes()])
        } else {
            self.hash.hex(&[a1.as_bytes(), nonce, a2.as_bytes()])
        };
        let mut out = Zeroizing::new(String::new());
        let username = Zeroizing::new(quoted(user));
        write!(*out, "Digest username={}, realm={}, nonce={}, uri={}, algorithm={}",
            &*username, quoted(&self.realm), quoted(&self.nonce), quoted(path), self.algorithm).unwrap();
        if let Some(opaque) = &self.opaque {
            write!(*out, ", opaque={}", quoted(opaque)).unwrap();
        }
        if self.qop {
            write!(*out, ", qop=auth, nc={nc}").unwrap();
        }
        if self.qop || self.session {
            write!(*out, ", cnonce={}", quoted(&cnonce)).unwrap();
        }
        // Append the sensitive response last, after reserving room for it.
        out.reserve(14 + response.len());
        write!(*out, ", response=\"{}\"", &*response).unwrap();
        Some(out)
    }
}

type Params = HashMap<String, String>;

/// A comma starts another parameter or another scheme, except inside quotes.
/// Reject duplicate parameters and malformed quoted strings.
fn challenges(line: &str) -> Vec<(&str, Params)> {
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return Vec::new();
    }
    let mut items = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            ',' if !in_quote => {
                items.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if in_quote {
        return Vec::new();
    }
    items.push(&line[start..]);
    let mut results = Vec::new();
    let mut scheme = "";
    let mut params = Params::new();
    let mut valid = false;
    for item in items.into_iter().map(str::trim).filter(|s| !s.is_empty()) {
        let split = item.find(|c: char| c == '=' || c.is_ascii_whitespace()).unwrap_or(item.len());
        let (token, rest) = item.split_at(split);
        let mut parameter = item;
        if !rest.trim_start().starts_with('=') {
            if valid {
                results.push((scheme, std::mem::take(&mut params)));
            }
            params.clear();
            scheme = token;
            valid = true;
            parameter = rest.trim();
        }
        if !parameter.is_empty() {
            let entry = parameter.split_once('=').and_then(|(key, value)| {
                let raw = value.trim();
                let value = if let Some(body) = raw.strip_prefix('"') {
                    let mut decoded = String::new();
                    let mut chars = body.chars();
                    loop {
                        match chars.next()? {
                            '"' if chars.as_str().is_empty() => break decoded,
                            '"' => return None,
                            '\\' => decoded.push(chars.next()?),
                            c => decoded.push(c),
                        }
                    }
                } else if raw.is_empty() || raw.contains(|c: char| c.is_ascii_whitespace() || c == '"') {
                    return None;
                } else {
                    raw.into()
                };
                Some((key.trim().to_ascii_lowercase(), value))
            });
            valid &= entry.is_some_and(|(name, value)| params.insert(name, value).is_none());
        }
    }
    if valid {
        results.push((scheme, params));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(line: &str) -> Option<Scheme> {
        Scheme::choose(401, &[("www-authenticate".into(), line.into())])
    }

    #[test]
    fn digest_challenges_preserve_quoted_values_and_skip_unsupported_options() {
        let Some(Scheme::Digest(digest)) = offered(
            r#"Digest realm="a,b \"printer\"", nonce=old, algorithm=MD5, Digest realm="a,b \"printer\"", nonce=new, algorithm=SHA-512-256-sess, qop="auth,auth-int""#,
        ) else {
            panic!("expected Digest");
        };
        assert_eq!(digest.realm, "a,b \"printer\"");
        assert_eq!(digest.nonce, "new");
        assert_eq!(digest.algorithm, "SHA-512-256-sess");
        assert!(offered("Digest realm=fixture, nonce=abc, qop=auth-int").is_none());
        assert!(offered("Digest realm=fixture, nonce=abc, nonce=def").is_none());
        assert!(Scheme::choose(401, &[]).is_none());
        assert!(matches!(Scheme::choose(200, &[]), Some(Scheme::Basic)));
    }
}
