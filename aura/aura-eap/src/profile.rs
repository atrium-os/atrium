//! An enterprise network's settings and password, as one value: what
//! aura-keys keeps in Arca for the network and hands the EAP helper for
//! one join. Text lines `key=hex` after a `eap1` header, so a passphrase
//! (8 to 63 printable bytes) can never be mistaken for one.

use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// PEAPv0 with EAP-MSCHAPv2 inside.
    Peap,
    TtlsPap,
    TtlsMschapv2,
    /// EAP-TLS: a client certificate, no password.
    Tls,
}

impl Method {
    pub fn eap_type(self) -> u8 {
        use crate::eap::typ;
        match self {
            Method::Peap => typ::PEAP,
            Method::TtlsPap | Method::TtlsMschapv2 => typ::TTLS,
            Method::Tls => typ::TLS,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Method::Peap => "peap",
            Method::TtlsPap => "ttls-pap",
            Method::TtlsMschapv2 => "ttls-mschapv2",
            Method::Tls => "tls",
        }
    }

    pub fn from_name(s: &str) -> Option<Method> {
        [Method::Peap, Method::TtlsPap, Method::TtlsMschapv2, Method::Tls].into_iter().find(|m| m.name() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub method: Method,
    /// The name inside the tunnel.
    pub identity: String,
    /// The name sent in the clear; `None` means `anonymous@<realm>`.
    pub anonymous: Option<String>,
    /// The server certificate must name this domain or a name under it.
    pub domain: String,
    /// CA certificates (DER) the server must chain to.
    pub ca: Vec<Vec<u8>>,
    /// Or the system's root certificates (with `domain`; no `ca`).
    pub system_roots: bool,
    /// The top certificate (DER) pinned by "Trust this server".
    pub pin: Option<Vec<u8>>,
    pub password: Zeroizing<String>,
    /// EAP-TLS: our certificate chain (DER, ours first) and the Arca item
    /// that holds its key (aura-keys signs with it; nobody reads it).
    pub cert: Vec<Vec<u8>>,
    pub key: Option<[u8; 16]>,
}

const MAGIC: &[u8] = b"eap1\n";
const MAX: usize = 48 * 1024;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    s.chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()).collect()
}

impl Profile {
    /// The outer identity: the configured one, or `anonymous@realm` (the
    /// realm from the identity); EAP-TLS sends the identity itself.
    pub fn outer_identity(&self) -> String {
        if let Some(a) = &self.anonymous {
            return a.clone();
        }
        if self.method == Method::Tls {
            return self.identity.clone();
        }
        match self.identity.rsplit_once('@') {
            Some((_, realm)) if !realm.is_empty() => format!("anonymous@{realm}"),
            _ => "anonymous".into(),
        }
    }

    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(MAGIC.to_vec());
        let mut put = |k: &str, v: &[u8]| {
            out.extend_from_slice(k.as_bytes());
            out.push(b'=');
            out.extend_from_slice(Zeroizing::new(hex(v)).as_bytes());
            out.push(b'\n');
        };
        put("method", self.method.name().as_bytes());
        put("identity", self.identity.as_bytes());
        if let Some(a) = &self.anonymous {
            put("anonymous", a.as_bytes());
        }
        put("domain", self.domain.as_bytes());
        for c in &self.ca {
            put("ca", c);
        }
        if self.system_roots {
            put("roots", b"system");
        }
        if let Some(p) = &self.pin {
            put("pin", p);
        }
        put("password", self.password.as_bytes());
        for c in &self.cert {
            put("cert", c);
        }
        if let Some(k) = &self.key {
            put("key", k);
        }
        out
    }

    /// Whether `b` is an enterprise profile at all (and not a passphrase).
    pub fn is_profile(b: &[u8]) -> bool {
        b.starts_with(MAGIC)
    }

    pub fn decode(b: &[u8]) -> Option<Profile> {
        if !Profile::is_profile(b) || b.len() > MAX {
            return None;
        }
        let mut p = Profile {
            method: Method::Peap,
            identity: String::new(),
            anonymous: None,
            domain: String::new(),
            ca: Vec::new(),
            system_roots: false,
            pin: None,
            password: Zeroizing::new(String::new()),
            cert: Vec::new(),
            key: None,
        };
        let mut method = None;
        for line in b[MAGIC.len()..].split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
            let eq = line.iter().position(|&c| c == b'=')?;
            let (k, v) = (&line[..eq], Zeroizing::new(unhex(&line[eq + 1..])?));
            let text = || String::from_utf8(v.to_vec()).ok();
            match k {
                b"method" => method = Method::from_name(&text()?),
                b"identity" => p.identity = text()?,
                b"anonymous" => p.anonymous = Some(text()?),
                b"domain" => p.domain = text()?,
                b"ca" if p.ca.len() < 8 => p.ca.push(v.to_vec()),
                b"roots" if v.as_slice() == b"system" => p.system_roots = true,
                b"pin" => p.pin = Some(v.to_vec()),
                b"password" => p.password = Zeroizing::new(text()?),
                b"cert" if p.cert.len() < 8 => p.cert.push(v.to_vec()),
                b"key" => p.key = Some(<[u8; 16]>::try_from(v.as_slice()).ok()?),
                _ => return None,
            }
        }
        p.method = method?;
        if p.identity.is_empty() || p.identity.len() > 253 {
            return None;
        }
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = Profile {
            method: Method::TtlsMschapv2,
            identity: "alice@corp.example".into(),
            anonymous: None,
            domain: "corp.example".into(),
            ca: vec![vec![0x30, 1, 2]],
            system_roots: true,
            pin: None,
            password: Zeroizing::new("s3cret=\n".into()),
            cert: vec![vec![0x30, 9]],
            key: Some([5; 16]),
        };
        let e = p.encode();
        assert!(Profile::is_profile(&e));
        assert_eq!(Profile::decode(&e).unwrap(), p);
        assert_eq!(p.outer_identity(), "anonymous@corp.example");
        assert!(!Profile::is_profile(b"a passphrase"));
        assert_eq!(Profile::decode(b"eap1\nmethod=7065617\n"), None, "odd hex");
        assert_eq!(Profile::decode(b"eap1\nidentity=61\n"), None, "no method");
        assert_eq!(Profile::decode(b"eap1\nmethod=70656170\nidentity=61\nevil=00\n"), None, "unknown key");
    }
}
