//! `stoa-shell` — the SSH handoff helper (stoa.md §2, aqueduct-remote.md §2).
//!
//! sshd runs this as the authenticated user's command:
//!
//! ```text
//! ssh -T user@host stoa-shell <name>
//! ```
//!
//! It mints (or resumes) the named session against the local `stoad`
//! control socket — `stoad` trusts the request because `getpeereid` on the
//! Unix socket yields this user's uid — and writes the result to stdout,
//! which travels back to the client inside the confidential SSH channel:
//!
//! ```text
//! STOA_SESSION <udp_port> <K_sess_hex>
//! ```
//!
//! Then it exits. The SSH connection is now free to drop; the client talks
//! UDP straight to `stoad` on the minted port. `stoa-shell` holds no
//! privilege of its own — compromising it gains only what the invoking
//! user already had.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use stoa::default_ctl;

fn main() {
    // args: <name> [--jail <id>]
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut name = "default".to_string();
    let mut target = "session".to_string();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jail" => {
                if let Some(id) = it.next() {
                    target = format!("jail:{id}");
                }
            }
            other => name = other.to_string(),
        }
    }
    let ctl = default_ctl();

    let stream = match UnixStream::connect(&ctl) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stoa-shell: connect {ctl}: {e}");
            eprintln!("stoa-shell: is stoad running?");
            std::process::exit(1);
        }
    };

    if let Err(e) = (&stream).write_all(format!("MINT {name} {target}\n").as_bytes()) {
        eprintln!("stoa-shell: mint request: {e}");
        std::process::exit(1);
    }

    let mut reply = String::new();
    if BufReader::new(&stream).read_line(&mut reply).is_err() || reply.is_empty() {
        eprintln!("stoa-shell: no reply from stoad");
        std::process::exit(1);
    }
    let reply = reply.trim();
    if let Some(err) = reply.strip_prefix("ERR ") {
        eprintln!("stoa-shell: stoad refused: {err}");
        std::process::exit(1);
    }

    // reply = "<port> <keyhex>"; emit it tagged so the client can find it
    // amid any SSH/login banner noise on the channel.
    println!("STOA_SESSION {reply}");
}
