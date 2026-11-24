//! The SSH agent's keys (arca.md §9.1, dev-tools.md §8).
//!
//! `atrium-ssh-agent` is the one system service that may use them: it
//! listens on the agent socket portcullisd mounts into workspaces, names
//! the asking workspace from the kernel's peer uid and the launch registry,
//! and asks arcad to list or sign. arcad holds the keys (Ed25519 items,
//! recipient `Service("ssh-agent")`, in the workspace's persona) and the
//! grants (the item's scope: workspace names, one a line). A workspace gets
//! only the keys granted to it, by name, in its own persona.

/// The `ARCA_SERVICES` name of the agent, and its items' recipient.
pub const SERVICE: &str = "ssh-agent";

/// A workspace name as portcullisd allows one: 1–32 lowercase letters,
/// digits and dashes, not starting with a dash.
pub fn valid_workspace(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0] != b'-' && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A host a token may be for: a lowercase DNS name, no port, no wildcard.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && !h.starts_with('.') && !h.ends_with('.') && !h.contains("..")
        && h.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
}

/// A token item's account: `host username`.
pub fn token_account(host: &str, user: &str) -> String {
    format!("{host} {user}")
}

use arca_store::Kind;

/// A kind the agent holds as a key: Ed25519 for SSH, P-256 for SSH
/// (`ecdsa-sha2-nistp256`) and for signing bundles (opifex.md §5, a
/// publisher key: the platform verifies P-256 only).
pub fn is_key(k: Kind) -> bool {
    matches!(k, Kind::Ed25519 | Kind::P256)
}

/// The scope line that carries a verify-each-use key's public key.
fn prefix(k: Kind) -> &'static str {
    if k == Kind::P256 { "@p256:" } else { "@ed25519:" }
}

/// Public metadata for keys whose seed needs a touch to read: Ed25519's
/// 32 bytes, or P-256's uncompressed SEC1 point (65 bytes).
pub fn public_scope(kind: Kind, pk: &[u8]) -> Vec<u8> {
    let hex: String = pk.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}{hex}", prefix(kind)).into_bytes()
}

pub fn scope_public_key(kind: Kind, scope: &[u8]) -> Option<Vec<u8>> {
    let want = if kind == Kind::P256 { 65 } else { 32 };
    let hex = std::str::from_utf8(scope).ok()?.lines().next()?.strip_prefix(prefix(kind))?;
    if hex.len() != want * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..want).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect()
}

/// The workspaces in a scope.
pub fn grants(scope: &[u8]) -> Vec<&str> {
    std::str::from_utf8(scope).unwrap_or("").lines().filter(|l| valid_workspace(l)).collect()
}

pub fn granted(scope: &[u8], ws: &str) -> bool {
    grants(scope).contains(&ws)
}

/// The scope with `ws` added (`on`) or removed, sorted, each name once.
pub fn with_grant(scope: &[u8], ws: &str, on: bool) -> Vec<u8> {
    let mut v: Vec<&str> = grants(scope).into_iter().filter(|g| *g != ws).collect();
    if on {
        v.push(ws);
    }
    v.sort();
    v.dedup();
    if let Some(line) = std::str::from_utf8(scope).ok().and_then(|s| s.lines().next())
        .filter(|line| line.starts_with(prefix(Kind::Ed25519)) || line.starts_with(prefix(Kind::P256)))
    {
        v.insert(0, line);
    }
    v.join("\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes() {
        let s = with_grant(b"", "web", true);
        let s = with_grant(&s, "lib-a", true);
        let s = with_grant(&s, "web", true);
        assert_eq!(s, b"lib-a\nweb");
        assert!(granted(&s, "web") && !granted(&s, "we") && !granted(&s, "lib-a\nweb"));
        assert_eq!(with_grant(&s, "web", false), b"lib-a");
        assert!(!valid_workspace("-x") && !valid_workspace("X") && !valid_workspace("") && valid_workspace("a-1"));
        assert!(grants(b"ok\nBAD\n\nfine").len() == 2);
    }

    #[test]
    fn public_key_lines_survive_grants() {
        let pk = [7u8; 65];
        let s = with_grant(&public_scope(Kind::P256, &pk), "web", true);
        assert_eq!(scope_public_key(Kind::P256, &s), Some(pk.to_vec()));
        assert_eq!(scope_public_key(Kind::Ed25519, &s), None, "kinds don't mix");
        assert_eq!(grants(&s), vec!["web"]);
        let s = with_grant(&public_scope(Kind::Ed25519, &[1; 32]), "web", true);
        assert_eq!(scope_public_key(Kind::Ed25519, &s), Some(vec![1; 32]));
    }
}
