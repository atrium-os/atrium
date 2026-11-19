//! arca-ssh: the person's SSH keys in Arca (arca.md §9.1).
//!
//!     arca-ssh new [--persona P] [--verify-each-use] COMMENT    make a key; prints its id and public line
//!     arca-ssh list [--persona P]           keys, public lines, the workspaces granted
//!     arca-ssh grant [--persona P] ID WORKSPACE
//!     arca-ssh revoke [--persona P] ID WORKSPACE
//!     arca-ssh delete [--persona P] ID
//!     arca-ssh token [--persona P] HOST USERNAME   an HTTPS token (git, a
//!                                           registry), read from stdin;
//!                                           grant it like a key
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
        ("new", 1) if rest[0] == "--verify-each-use" => return usage(),
        ("new", _) if rest.first().is_some_and(|s| s == "--verify-each-use") => {
            format!("new-touch {persona} {}", rest[1..].join(" "))
        }
        ("new", n) if n >= 1 => format!("new {persona} {}", rest.join(" ")),
        ("list", 0) => format!("list {persona}"),
        ("grant" | "revoke", 2) => format!("{verb} {persona} {} {}", rest[0], rest[1]),
        ("delete", 1) => format!("delete {persona} {}", rest[0]),
        ("token", 2) => {
            let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
            if tty {
                eprintln!("arca-ssh: paste the token for {} and press Return (not shown)", rest[0]);
            }
            let mut t = String::new();
            let read = no_echo(tty, || std::io::stdin().read_line(&mut t));
            if read.is_err() || t.trim().is_empty() {
                eprintln!("arca-ssh: no token on stdin");
                return ExitCode::FAILURE;
            }
            format!("token {persona} {} {}\n{}", rest[0], rest[1], t.trim())
        }
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

/// Run `f` with the terminal's echo off (the token isn't shown).
fn no_echo<T>(tty: bool, f: impl FnOnce() -> T) -> T {
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    let off = tty && unsafe { libc::tcgetattr(0, &mut old) } == 0 && {
        let mut t = old;
        t.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) == 0 }
    };
    let r = f();
    if off {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
        eprintln!();
    }
    r
}

fn usage() -> ExitCode {
    eprintln!("usage: arca-ssh [--persona P] new [--verify-each-use] COMMENT | list | grant ID WORKSPACE | revoke ID WORKSPACE | delete ID | token HOST USERNAME");
    ExitCode::from(2)
}
