//! git-credential-arca: git's credential helper in a workspace
//! (dev-tools.md §8, arca.md §9.1). portcullisd puts it on the
//! workspace's PATH and sets `credential.helper=arca` when the workspace
//! has Arca's agent. For `get` over https it asks the agent (through
//! `SSH_AUTH_SOCK`) for the token the person granted this workspace for
//! that host; anything else, or no token, prints nothing and git carries
//! on (asking, or failing). `store` and `erase` do nothing: tokens are
//! added with `arca-ssh token`, outside the workspace.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use arca_ssh_agent::wire;

fn main() {
    if std::env::args().nth(1).as_deref() != Some("get") {
        // Drain what git sends, then nothing.
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        return;
    }
    let mut attrs = HashMap::new();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            attrs.insert(k.to_string(), v.to_string());
        }
    }
    if attrs.get("protocol").map(String::as_str) != Some("https") {
        return;
    }
    let Some(host) = attrs.get("host") else { return };
    let host = host.to_ascii_lowercase();
    // A port is part of git's host; tokens are per name.
    let host = host.strip_suffix(":443").unwrap_or(&host);
    if !arcad::ssh::valid_host(host) {
        return;
    }
    let Some((user, token)) = ask(host) else { return };
    if let Some(want) = attrs.get("username") {
        if *want != user {
            return;
        }
    }
    let (Ok(token), false) = (String::from_utf8(token), user.contains('\n')) else { return };
    if token.contains('\n') {
        return;
    }
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "username={user}\npassword={token}");
}

fn ask(host: &str) -> Option<(String, Vec<u8>)> {
    let path = std::env::var("SSH_AUTH_SOCK").ok()?;
    let mut s = UnixStream::connect(path).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok()?;
    s.write_all(&wire::frame(&wire::credential_request(host))).ok()?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let n = u32::from_be_bytes(len) as usize;
    if n > wire::MAX_MSG {
        return None;
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).ok()?;
    wire::credential_answer(&b)
}
