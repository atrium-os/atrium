//! WebAuthn RP ids an app may use (sigillum.md §13; opifex.md §7.3; C7).
//!
//! An app's manifest asks for RP ids under `[capabilities] identity.webauthn`.
//! At install, opifexd fetches `https://<rp>/.well-known/atrium-apps` for each,
//! through the open fetcher, and keeps only the RPs whose file names this
//! app: a line `<publisher> <app id>`, where `<publisher>` is the publisher
//! id (`pub:…`, which survives key rotation) or, for the Atrium catalog's
//! platform key, its key id. Lines starting with `#` are comments. The
//! verified ones go into the installed record, which Sigillum reads; an app
//! with none gets only its private RP `app:<publisher>/<id>`.

/// At most this many RP ids are checked for one app.
pub const MAX_RPS: usize = 8;

/// The RP ids the manifest asks for, deduplicated, in order.
pub fn rp_ids(manifest: &str) -> Vec<String> {
    let Ok(v) = manifest.parse::<toml::Value>() else { return Vec::new() };
    let caps = v.get("capabilities");
    let list = caps.and_then(|c| c.get("identity")).and_then(|i| i.get("webauthn"))
        .or_else(|| caps.and_then(|c| c.get("identity.webauthn")));
    let mut out: Vec<String> = Vec::new();
    for r in list.and_then(|l| l.as_array()).into_iter().flatten().filter_map(|x| x.as_str()) {
        let r = r.to_ascii_lowercase();
        if host_ok(&r) && !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// A plain host name: lowercase labels of letters, digits and `-`, at least two.
pub fn host_ok(h: &str) -> bool {
    let labels: Vec<&str> = h.split('.').collect();
    h.len() <= 253 && labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

pub fn url(rp: &str) -> String { format!("https://{rp}/.well-known/atrium-apps") }

/// Does the file name `app` from `publisher`?
pub fn names(text: &str, publisher: &str, app: &str) -> bool {
    text.lines().map(str::trim).filter(|l| !l.starts_with('#')).any(|l| {
        let mut it = l.split_whitespace();
        it.next() == Some(publisher) && it.next() == Some(app) && it.next().is_none()
    })
}

/// The RPs whose file names this app, fetching each with `get`.
pub fn verify(manifest: &str, publisher: &str, app: &str, get: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>) -> (Vec<String>, Vec<String>) {
    let (mut ok, mut dropped) = (Vec::new(), Vec::new());
    for (i, rp) in rp_ids(manifest).into_iter().enumerate() {
        if i >= MAX_RPS {
            dropped.push(format!("{rp}: more than {MAX_RPS} asked for"));
            continue;
        }
        match get(&url(&rp)) {
            Ok(b) if names(&String::from_utf8_lossy(&b), publisher, app) => ok.push(rp),
            Ok(_) => dropped.push(format!("{rp}: its atrium-apps file doesn't name {publisher} {app}")),
            Err(e) => dropped.push(format!("{rp}: {e}")),
        }
    }
    (ok, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rps_from_the_manifest_and_their_files() {
        let m = "[app]\nid = \"com.example.x\"\n\n[capabilities]\nidentity.webauthn = [\"example.com\", \"Login.Example.com\", \"example.com\", \"bad host\", \"x\"]\n";
        assert_eq!(rp_ids(m), vec!["example.com".to_string(), "login.example.com".into()]);
        let file = "# Atrium apps that may use our passkeys\npub:0123456789abcdef com.example.x\n";
        assert!(names(file, "pub:0123456789abcdef", "com.example.x"));
        assert!(!names(file, "pub:0123456789abcdef", "com.example.y"));
        assert!(!names("#pub:0123456789abcdef com.example.x", "pub:0123456789abcdef", "com.example.x"));
        assert!(!names("pub:0123456789abcdef com.example.x extra", "pub:0123456789abcdef", "com.example.x"));
        let (ok, dropped) = verify(m, "pub:0123456789abcdef", "com.example.x", &mut |u| {
            if u == "https://example.com/.well-known/atrium-apps" { Ok(file.as_bytes().to_vec()) } else { Err("HTTP 404".into()) }
        });
        assert_eq!(ok, vec!["example.com".to_string()]);
        assert!(dropped[0].starts_with("login.example.com: HTTP 404"));
    }
}
