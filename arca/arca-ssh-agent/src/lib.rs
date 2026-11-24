//! Arca's SSH agent (arca.md §9.1, dev-tools.md §8).
//!
//! portcullisd asks the agent, at each workspace start, to open a socket
//! for that workspace in its own folder under [`SOCKET_ROOT`]; it mounts
//! that folder (read-only) into the workspace and sets `SSH_AUTH_SOCK`.
//! Which socket a connection came in on says which workspace is asking:
//! a workspace's jail sees only its own folder, so even its root, which can
//! take any uid inside the jail, can't reach another workspace's socket.
//! The peer must also be the workspace's own uid (the launch registry's).
//!
//! The agent speaks the ssh-agent protocol (draft-miller-ssh-agent) for
//! listing identities and signing, nothing else: no adding, removing or
//! locking keys from inside a workspace. Keys and their grants are arcad's
//! ([`arcad::proto::ssh_op`]); the person manages them through the control
//! socket (`arca-ssh`).

pub mod arca;
pub mod control;
pub mod wire;

/// Where the agent's sockets live: `control.sock`, and one folder per
/// open workspace (`<owner>.<workspace>/agent.sock`).
pub const SOCKET_ROOT: &str = "/atrium/sockets/ssh-agent";
pub const CONTROL: &str = "control.sock";
pub const AGENT: &str = "agent.sock";
/// Where a workspace sees its folder, and so `SSH_AUTH_SOCK`.
pub const IN_WORKSPACE: &str = "/atrium/ssh-agent";

/// A workspace's folder name under [`SOCKET_ROOT`].
pub fn folder(owner: &str, ws: &str) -> String {
    format!("{owner}.{ws}")
}

/// Arca's persona for a workspace persona: Personal is Arca's main persona.
pub fn arca_persona(p: &str) -> &str {
    if p == "personal" { arcad::server::PERSONA } else { p }
}

/// A person's user name as the store allows it, without dots (they separate
/// the folder name).
pub fn valid_owner(u: &str) -> bool {
    arca_store::model::valid_user(u) && !u.contains('.')
}

pub fn valid_persona(p: &str) -> bool {
    p == "personal" || portcullis_peer::valid_persona(p)
}

/// Base64 (standard, padded), for public key lines.
pub fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity((b.len() + 2) / 3 * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

/// `ssh-ed25519 AAAA... comment` (or `ecdsa-sha2-nistp256 ...`), for
/// `authorized_keys` or a forge.
pub fn public_line(k: &wire::Key) -> String {
    let algo = if k.p256 { "ecdsa-sha2-nistp256" } else { "ssh-ed25519" };
    format!("{algo} {} {}", base64(&wire::key_blob(k)), k.comment)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut o = [0u8; 16];
    for (i, b) in o.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_lines() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The blob of an all-zero key, as ssh-keygen would print it.
        let k = wire::Key { id: [0; 16], p256: false, pk: vec![0; 32], comment: "c".into() };
        assert!(public_line(&k).starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"));
        let k = wire::Key { p256: true, pk: vec![4; 65], ..k };
        assert!(public_line(&k).starts_with("ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABB"));
        assert_eq!(unhex16(&hex(&[7; 16])), Some([7; 16]));
        assert_eq!(arca_persona("personal"), "main");
        assert_eq!(arca_persona("work"), "work");
        assert!(!valid_owner("a.b") && valid_owner("alice"));
    }
}
