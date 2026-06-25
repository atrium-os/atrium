//! `BrokerSpawner` — `Target::Jail` via the portcullisd `ExecInJail` broker
//! (FreeBSD only). This is Stoa's jail-target path (stoa.md §4.5): a shell
//! INSIDE a specific running jail, brokered through portcullisd so `_stoad`
//! never touches jaild directly.
//!
//! Flow: connect portcullisd → Hello → `ExecInJail` → read the
//! `JailExecStarted` line → `recv_fds(2)` = `[procdesc, pty_master]` → wrap
//! as a [`PtyShell`] whose process is reaped via the procdesc. `_stoad`
//! opens nothing; jaild allocates the pty and the master rides SCM_RIGHTS
//! all the way up.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;

use portcullis_ipc::{recv_fds, write_request, Request, Response, PROTO_VERSION};

use crate::{pty_shell_from_broker, PtyShell, ShellSpawner, SpawnSpec, Target};

/// Default portcullisd broker socket (the federated path `_stoad` reaches).
const BROKER_SOCK_DEFAULT: &str = "/atrium/sockets/portcullis.sock";

/// Spawns shells inside existing jails via the portcullisd broker.
#[derive(Debug, Clone)]
pub struct BrokerSpawner {
    pub broker_sock: String,
}

impl Default for BrokerSpawner {
    fn default() -> Self {
        BrokerSpawner {
            broker_sock: std::env::var("STOA_BROKER")
                .unwrap_or_else(|_| BROKER_SOCK_DEFAULT.to_string()),
        }
    }
}

impl BrokerSpawner {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ShellSpawner for BrokerSpawner {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<PtyShell> {
        let jail_name = match &spec.target {
            Target::Jail(name) => name.clone(),
            // The session-jail path goes through LaunchSessionComponent, not
            // ExecInJail; stoad uses DirectSpawner or that path for it.
            Target::SessionJail => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "BrokerSpawner handles Target::Jail only (use the session-jail path for SessionJail)",
                ))
            }
        };

        let mut c = UnixStream::connect(&self.broker_sock).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("connect broker {}: {e}", self.broker_sock))
        })?;

        // Mandatory Hello handshake.
        write_request(&mut c, &Request::Hello { version: PROTO_VERSION })?;
        match read_response_line(&mut c)? {
            Response::Hello { .. } => {}
            other => return Err(io::Error::other(format!("broker handshake: {other:?}"))),
        }

        // ExecInJail. Default to the jail's app-uid (non-root); a root shell
        // would set want_root (needs the broker's jail_exec_root grant).
        let argv = if spec.cmd.is_empty() {
            vec!["/bin/sh".to_string()]
        } else {
            spec.cmd.clone()
        };
        let path = argv[0].clone();
        write_request(
            &mut c,
            &Request::ExecInJail {
                jail_name: jail_name.clone(),
                path,
                argv,
                want_root: false,
                cols: spec.cols,
                rows: spec.rows,
            },
        )?;

        // CRITICAL: read the response line byte-by-byte, NOT via a BufReader.
        // portcullisd sends the JSON line and then the fds in a separate
        // sendmsg; a BufReader could pull the fd message's payload byte into
        // its buffer and the kernel would drop the attached SCM_RIGHTS fds.
        let pid = match read_response_line(&mut c)? {
            Response::JailExecStarted { pid, uid } => {
                eprintln!("stoad: jexec into {jail_name:?} → pid {pid} uid {uid}");
                pid
            }
            Response::Error { message } => {
                return Err(io::Error::other(format!("ExecInJail denied: {message}")))
            }
            other => return Err(io::Error::other(format!("ExecInJail: unexpected {other:?}"))),
        };

        let fds = recv_fds(&c, 2)?;
        if fds.len() != 2 {
            return Err(io::Error::other(format!("expected 2 fds, got {}", fds.len())));
        }
        let mut it = fds.into_iter();
        let procdesc = it.next().unwrap();
        let master = it.next().unwrap();
        Ok(pty_shell_from_broker(master, procdesc, pid))
    }
}

/// Read one newline-terminated JSON response WITHOUT buffering past the
/// newline (see the SCM_RIGHTS hazard noted in `spawn`).
fn read_response_line(c: &mut UnixStream) -> io::Result<Response> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match c.read(&mut byte)? {
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "broker closed")),
            _ => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
        }
    }
    serde_json::from_slice(&line).map_err(|e| io::Error::other(format!("bad response: {e}")))
}
