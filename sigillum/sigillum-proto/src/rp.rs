//! RP ID syntax, shared by sigillumd and the WebAuthn client. The *origin*
//! check (is this RP ID allowed for the page's origin) is the web broker's
//! (`sigillum-webauthn`, sigillum.md §9.1). Here: is it a well-formed domain.

/// A lowercase DNS name: 1–253 bytes, labels of 1–63 `[a-z0-9-]` not
/// starting or ending with `-`, and not an IP address (WebAuthn RP IDs are
/// domains). `localhost` is allowed for development RPs.
pub fn valid_domain(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 || s.ends_with('.') {
        return false;
    }
    let labels: Vec<&str> = s.split('.').collect();
    for l in &labels {
        if l.is_empty() || l.len() > 63 || l.starts_with('-') || l.ends_with('-') {
            return false;
        }
        if !l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return false;
        }
    }
    // The last label of an IPv4 literal is all digits; no TLD is.
    !labels.last().unwrap().bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains() {
        for ok in ["example.com", "login.example.co.uk", "localhost", "xn--bcher-kva.example", "a-b.c"] {
            assert!(valid_domain(ok), "{ok}");
        }
        for bad in ["", "Example.com", "example.com.", "-a.com", "a-.com", "a..com", "10.0.0.1", "exa mple.com", "app:x/y", &"a".repeat(64), "évil.com"] {
            assert!(!valid_domain(bad), "{bad}");
        }
    }
}
