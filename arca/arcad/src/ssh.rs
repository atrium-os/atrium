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
}
