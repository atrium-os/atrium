//! Arca's SSH agent, from portcullisd's side (arca.md §9.1, dev-tools.md
//! §8): at each workspace start, ask the agent to open that workspace's
//! socket, then mount its folder read-only at [`IN_JAIL`] and set
//! `SSH_AUTH_SOCK`. The agent tells workspaces apart by which socket a
//! connection came in on, so each jail must see only its own folder.
//! No agent running: the workspace starts without one.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use portcullis_ipc::{WorkspaceCredential, WorkspaceCredentialKind};

/// The agent's sockets (arca-ssh-agent's `SOCKET_ROOT`).
const ROOT: &str = "/atrium/sockets/ssh-agent";
/// Where the workspace sees its folder.
pub const IN_JAIL: &str = "atrium/ssh-agent";
pub const AUTH_SOCK: &str = "SSH_AUTH_SOCK=/atrium/ssh-agent/agent.sock";

fn ask(line: &str) -> Result<Vec<String>, String> {
    let mut s = UnixStream::connect(format!("{ROOT}/control.sock")).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    writeln!(s, "{line}").map_err(|e| e.to_string())?;
    let mut lines = BufReader::new(s).lines();
    match lines.next().transpose().map_err(|e| e.to_string())?.as_deref() {
        Some("ok") => lines.collect::<std::io::Result<Vec<_>>>().map_err(|e| e.to_string()),
        Some(e) => Err(e.strip_prefix("error ").unwrap_or(e).to_string()),
        None => Err("no answer".into()),
    }
}

/// Open the workspace's socket; its folder on the host.
pub fn open(owner: &str, ws: &str, persona: &str, uid: u32) -> Option<PathBuf> {
    match ask(&format!("open {owner} {ws} {persona} {uid}")) {
        Ok(l) => {
            let sock = PathBuf::from(l.first()?);
            // ★ Exactly the folder made for this workspace, nothing above it.
            let dir = sock.parent()?.to_path_buf();
            (dir == PathBuf::from(ROOT).join(format!("{owner}.{ws}"))).then_some(dir)
        }
        Err(e) => {
            eprintln!("portcullisd: workspace {ws} of {owner}: no SSH agent ({e})");
            None
        }
    }
}

pub fn close(owner: &str, ws: &str) {
    let _ = ask(&format!("close {owner} {ws}"));
}

/// The workspace was deleted: its grants go.
pub fn forget(owner: &str, ws: &str, persona: &str) {
    if let Err(e) = ask(&format!("forget {owner} {ws} {persona}")) {
        eprintln!("portcullisd: workspace {ws} of {owner}: its SSH key grants were not ended ({e})");
    }
}

/// The persona comes from the installed workspace record. Refuse malformed
/// atoms before writing the line-based control request.
fn validate_scope(owner: &str, persona: &str) -> Result<(), String> {
    if [owner, persona].iter().any(|s| s.is_empty() || !s.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
    })) {
        return Err("Invalid credential owner or persona".into());
    }
    Ok(())
}

pub fn credentials(owner: &str, persona: &str, workspace: &str) -> Result<Vec<WorkspaceCredential>, String> {
    validate_scope(owner, persona)?;
    ask(&format!("list {owner} {persona}"))?.iter().map(|line| {
        parse_credential(line, workspace).ok_or_else(|| "Invalid credential metadata from Arca".into())
    }).collect()
}

fn parse_credential(line: &str, workspace: &str) -> Option<WorkspaceCredential> {
    let (metadata, grants) = line.rsplit_once(" granted=")?;
    let mut fields = metadata.splitn(3, ' ');
    let encoded = fields.next()?;
    let id = hex::decode(encoded).ok()?.try_into().ok()?;
    let (kind, label) = match (fields.next()?, fields.next()?) {
        ("ssh-ed25519", fields) => (WorkspaceCredentialKind::SshKey, fields.split_once(' ')?.1),
        ("token", fields) => (WorkspaceCredentialKind::HttpsToken, fields),
        _ => return None,
    };
    Some(WorkspaceCredential {
        id, kind, label: label.into(), granted: grants.split(',').any(|name| name == workspace),
    })
}

pub fn set_grant(owner: &str, persona: &str, workspace: &str, id: &[u8; 16], granted: bool) -> Result<(), String> {
    validate_scope(owner, persona)?;
    if !crate::workspace::valid_name(workspace) {
        return Err("Invalid workspace name".into());
    }
    let verb = if granted { "grant" } else { "revoke" };
    ask(&format!("{verb} {owner} {persona} {} {workspace}", hex::encode(id)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_metadata_uses_the_last_grant_field_and_exact_workspace() {
        let id = "00112233445566778899aabbccddeeff";
        let key = parse_credential(&format!("{id} ssh-ed25519 AAAA work granted=other granted=web,docs"), "web").unwrap();
        assert_eq!(key.label, "work granted=other");
        assert_eq!(key.kind, WorkspaceCredentialKind::SshKey);
        assert!(key.granted);
        let token = parse_credential(&format!("{id} token example.org alice granted=web-preview"), "web").unwrap();
        assert_eq!(token.label, "example.org alice");
        assert_eq!(token.kind, WorkspaceCredentialKind::HttpsToken);
        assert!(!token.granted);
        assert!(parse_credential("bad token example.org alice granted=web", "web").is_none());
    }
}
