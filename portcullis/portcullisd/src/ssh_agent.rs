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
    let mut lines = BufReader::new(s).lines().map_while(Result::ok);
    match lines.next().as_deref() {
        Some("ok") => Ok(lines.collect()),
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
