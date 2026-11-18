//! arca-ssh: the person's SSH keys in Arca (arca.md §9.1).
//!
//!     arca-ssh new [--persona P] COMMENT    make a key; prints its id and public line
//!     arca-ssh list [--persona P]           keys, public lines, the workspaces granted
//!     arca-ssh grant [--persona P] ID WORKSPACE
//!     arca-ssh revoke [--persona P] ID WORKSPACE
//!     arca-ssh delete [--persona P] ID
//!
//! The persona defaults to personal. The agent acts as whoever runs this
//! (the kernel's peer uid); the private key never leaves Arca.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut persona = "personal".to_string();
    if let Some(i) = args.iter().position(|a| a == "--persona") {
        if i + 1 >= args.len() {
            return usage();
        }
        persona = args.remove(i + 1);
        args.remove(i);
    }
    let Some(verb) = args.first().cloned() else { return usage() };
    let rest = &args[1..];
    let line = match (verb.as_str(), rest.len()) {
        ("new", n) if n >= 1 => format!("new {persona} {}", rest.join(" ")),
        ("list", 0) => format!("list {persona}"),
        ("grant" | "revoke", 2) => format!("{verb} {persona} {} {}", rest[0], rest[1]),
        ("delete", 1) => format!("delete {persona} {}", rest[0]),
        _ => return usage(),
    };
    let root = std::env::var("SSH_AGENT_ROOT").unwrap_or_else(|_| arca_ssh_agent::SOCKET_ROOT.into());
    let path = format!("{root}/{}", arca_ssh_agent::CONTROL);
    let mut s = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("arca-ssh: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if writeln!(s, "{line}").is_err() {
        return ExitCode::FAILURE;
    }
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    match out.split_once('\n').map(|(a, b)| (a, b)).unwrap_or((out.trim_end(), "")) {
        ("ok", body) => {
            print!("{body}");
            ExitCode::SUCCESS
        }
        (err, _) => {
            eprintln!("arca-ssh: {}", err.strip_prefix("error ").unwrap_or(err));
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: arca-ssh [--persona P] new COMMENT | list | grant ID WORKSPACE | revoke ID WORKSPACE | delete ID");
    ExitCode::from(2)
}
