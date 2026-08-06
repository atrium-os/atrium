//! portcullis-ipc — wire protocol between `portcullis` CLI and
//! `portcullisd`. Newline-delimited JSON over a Unix-domain socket
//! at `SOCKET_PATH`. Each line is one `Request` from the client or
//! one `Response` from the daemon; framing is "until the next \n".
//!
//! JSON (vs bincode/protobuf) chosen so the protocol is debuggable
//! with `nc -U` and unimportant from a perf perspective — request
//! rate is "human-driven app launches", not anything hot.
//!
//! Versioning: every `Request` and `Response` is tagged with its
//! variant name via `#[serde(tag = "op")]` / `#[serde(tag = "kind")]`,
//! so adding new variants is forward-compatible (older daemons reply
//! `UnknownOp`; older clients ignore unknown response kinds).

use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use portcullis_toml::Capabilities;

pub mod fdpass;
pub use fdpass::{recv_fds, send_fds};

/// Default socket path. Created by portcullisd at startup mode 0666
/// (world-connectable); per-user authorization happens inside the
/// daemon via getpeereid(2). Lives under /atrium/sockets/ so
/// atrium-session's bind-mount of that directory exposes it to
/// processes inside session jails without an extra mount entry.
pub const SOCKET_PATH: &str = "/atrium/sockets/portcullis.sock";

/// The same socket under the per-service DIRECTORY convention:
/// `/atrium/sockets/<service>/<service>.sock`.
///
/// This exists because a jail cannot be given the flat path above.
/// `mount_nullfs` refuses to mount a unix-socket node ("must be either a file or
/// directory"), so every capability-granted socket is reached by mounting the
/// service's own directory — see portcullis-jail's `apply_socket`. With the
/// socket sitting loose in the shared sockets root there is no directory to
/// mount that wouldn't also expose every OTHER service's socket, which is
/// exactly the capability granularity that design protects.
///
/// portcullisd binds HERE and leaves the flat path as a symlink, so existing
/// clients (the CLI, anything holding `SOCKET_PATH`) keep working unchanged.
pub const SERVICE_SOCKET_PATH: &str = "/atrium/sockets/portcullis/portcullis.sock";

/// Where a client should look for portcullisd: the per-service path first (the
/// only one that exists inside a jail), then the flat compat path.
///
/// Returning a path that exists — rather than one that merely might — keeps the
/// in-jail failure honest: if neither is present the caller reports "not
/// reachable" instead of a confusing ENOENT on a path that was never the right
/// one for its context.
pub fn resolve_socket_path() -> Option<&'static str> {
    [SERVICE_SOCKET_PATH, SOCKET_PATH]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

/// Protocol version — bumped if a wire-incompatible change lands.
/// Clients send their version in `Request::Hello`; mismatched versions
/// get `Response::ProtoMismatch` and the daemon closes the connection.
pub const PROTO_VERSION: u32 = 1;

/// The bounded reclaim signal a memory governor may request — mirrors jaild's
/// `ReapSignal` but keeps this app-protocol crate free of a jaild dependency
/// (portcullisd maps one to the other). No raw signal numbers cross the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovSignal {
    /// SIGINFO — shed caches, keep running.
    Trim,
    /// SIGTERM — exit gracefully.
    Exit,
    /// SIGKILL — force.
    Kill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Handshake; must be the first message on a fresh connection.
    Hello { version: u32 },
    /// Cheap liveness probe.
    Ping,
    /// "Can app `app_id` (with this manifest hash and these caps)
    /// launch right now?" Daemon returns `Authorized` if the policy
    /// already covers everything, `NeedsApproval` with the delta
    /// otherwise.
    Authorize {
        app_id:        String,
        manifest_hash: String,
        requested:     Capabilities,
    },
    /// "Persist a grant for these capabilities." Used by both the
    /// CLI bootstrap (`policy grant`) and, in the future, the
    /// prompt UI after the user clicks Allow.
    Grant {
        app_id:        String,
        manifest_hash: String,
        capabilities:  Capabilities,
    },
    /// Drop a previously persisted grant.
    Revoke { app_id: String },
    /// Re-read policy.toml from disk (for after manual edits).
    Reload,
    /// Run an installed app inside its per-app jail. Daemon does
    /// the policy check, the overlay mount, the jail(8) invocation,
    /// and the teardown — CLI just forwards and waits for the exit
    /// status. `bypass_policy = true` is the daemon-side equivalent
    /// of `portcullis launch --no-prompt` (dev mode; nothing
    /// persisted).
    ///
    /// Stdio: this commit (Phase 4.4 step 1) inherits the daemon's
    /// stdio, so app output lands in the daemon log. SCM_RIGHTS pty
    /// passing arrives in step 2 and replaces this with proper
    /// terminal handoff to the requesting client.
    Launch {
        app_id:        String,
        bypass_policy: bool,
    },

    /// "What may I launch?" — the installed-app catalog, for the launcher UI
    /// (Forum's dock). Requires the caller's manifest to hold `app-launch`.
    ///
    /// The daemon answers from its own view of the app tree rather than the
    /// caller reading the tree itself. That is the point: a jailed launcher
    /// never needs the app directory mounted, so it learns exactly what it may
    /// launch and nothing else about the filesystem — and the TCB stays the one
    /// deciding what is listed.
    Catalog,

    /// A memory governor (atrium-memoryd) asks portcullisd to signal a
    /// jaild-created jail's processes — the reclaim cascade. portcullisd checks
    /// the caller is a `memory_govern` service (its services.d manifest's
    /// exec.uid == the peer uid) and forwards to jaild as `Reap`; jaild applies
    /// its own jaild-created-only + `[resource_control]` gates. See
    /// `atrium-memory-pressure.md` §9.5.
    GovernReap {
        jail_name: String,
        signal:    GovSignal,
    },

    /// A memory governor (atrium-memfed) asks portcullisd to set a
    /// jaild-created jail's RCTL `memoryuse` cap (forwarded to jaild as
    /// `SetRctl`). Same `memory_govern` capability gate.
    GovernSetRctl {
        jail_name:    String,
        memoryuse_mb: u64,
    },

    /// The jailed, non-root session launcher (`_ostiarius`) asks portcullisd to
    /// launch a DECLARED session component (vestibulum, forum-wm, …). portcullisd
    /// checks the caller holds `session_launch` (its services.d manifest cap),
    /// looks `component_id` up in the session-component registry — an UNKNOWN id
    /// is refused, so `_ostiarius` can launch only the declared session set, never
    /// an arbitrary jail — fills the per-session owner, and forwards `CreateJail`
    /// to jaild. The procdesc is held by the TCB, not ostiarius. See
    /// docs/spec/ostiarius-privsep.md.
    LaunchSessionComponent {
        component_id: String,
        owner_name:   String,
    },

    /// `_ostiarius` forwards a credential received from vestibulum for portcullisd
    /// to verify against PAM/shadow — which it can read as root; the hashes never
    /// leave portcullisd. Same `session_launch` gate. Returns only yes/no + the
    /// canonical username.
    VerifyCredential {
        user:     String,
        password: String,
    },

    /// `_ostiarius` asks portcullisd to tear down a session component it launched
    /// (logout, or the component exited). portcullisd closes the procdesc it holds
    /// for `jail_name` — killing the persist=0 jail — and RemoveJails it. Same
    /// `session_launch` gate.
    TeardownSessionComponent {
        jail_name: String,
    },

    /// A jailed, non-root broker client (e.g. `_stoad`) asks portcullisd to
    /// exec a shell INSIDE an existing jaild-created jail on a pty — Stoa's
    /// jail-target sessions / `jexec` reimagined (stoa.md §4.5). portcullisd
    /// gates on the `jail_exec` capability (and `jail_exec_root` if
    /// `want_root`), forwards `ExecInJail` to jaild, and relays the two fds
    /// jaild returns — `[procdesc, pty_master]` — back to the caller over
    /// SCM_RIGHTS. The shell runs as the jail's own app-uid (non-root)
    /// unless `want_root`. Reply: `JailExecStarted` then the fds.
    ExecInJail {
        jail_name: String,
        path:      String,
        argv:      Vec<String>,
        #[serde(default)]
        want_root: bool,
        cols:      u16,
        rows:      u16,
    },
}

/// One installed app, as the daemon is willing to describe it to a launcher.
///
/// Deliberately just what a launcher must draw and act on — id, name, blurb,
/// icon. No entry path, no bundle hash, no capability set: the launcher only
/// ever names an app back to the daemon, so nothing here needs to describe how
/// it would run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    pub icon:        Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Handshake reply; daemon advertises its proto version.
    Hello { version: u32 },
    Pong,
    /// Authorize → all requested caps already granted.
    Authorized,
    /// Authorize → needs user approval. `delta` is human-readable
    /// reason lines (suitable for a prompt UI or CLI message).
    NeedsApproval { delta: Vec<String> },
    /// Grant or Revoke → success.
    Ok,
    /// Generic error reply with a message.
    Error { message: String },
    /// Wire-version mismatch; client should disconnect.
    ProtoMismatch { server_version: u32 },
    /// Daemon doesn't recognize this op (forward-compat).
    UnknownOp,
    /// Launch → app exited with this status.
    /// `code = None` if the jail terminated by signal (Unix
    /// convention: signal-exit doesn't have a numeric exit code).
    LaunchExit { code: Option<i32> },
    /// Launch → setup or teardown failed before/after the app ran.
    LaunchFailed { stage: String, message: String },
    /// Launch handshake: daemon has read+parsed the Launch request,
    /// passed the policy gate, and is now blocked in `recv_fds`
    /// waiting for the client's stdio. The CLI replies by sending
    /// the SCM_RIGHTS sendmsg with the three fds. This round-trip
    /// drains the daemon's BufReader so the cmsg's 1-byte payload
    /// can't be silently swallowed by a buffered read.
    ReadyForFds,
    /// Catalog → the apps this caller may launch.
    CatalogList { apps: Vec<CatalogEntry> },

    /// Launch → the app is `instances = "single"` and one is already running.
    ///
    /// Refusing is correct; reporting it as a failure is not. Without this, a
    /// duplicate launch surfaced as a raw jail(8) "already exists" line plus a
    /// generic non-zero exit, which a launcher cannot tell apart from a broken
    /// app — so clicking a dock icon twice showed an error instead of raising
    /// the window that was already there.
    ///
    /// `uid` is the running instance's per-app uid. That is what makes this
    /// actionable: surfaces carry `owner_uid`, so a launcher can find the live
    /// window and focus it without needing the launch registry (which a jailed
    /// launcher cannot read).
    AlreadyRunning { app_id: String, jid: i32, uid: u32 },

    /// Launch → policy gate refused. `delta` is the human-readable
    /// list of capabilities the user hasn't granted yet (suitable
    /// for showing in a prompt UI). Distinct from LaunchFailed —
    /// this is "needs interactive approval", not a permanent error.
    /// The CLI may prompt the user and re-issue Launch with
    /// bypass_policy=true (Allow Once) or call Grant first then
    /// re-issue (Allow Always).
    LaunchNeedsApproval { delta: Vec<String> },

    /// LaunchSessionComponent → the session jail was created. The procdesc is
    /// held by the TCB; ostiarius gets the identifiers for status/teardown.
    SessionComponentLaunched { pid: i32, jid: i32, jail_name: String },
    /// VerifyCredential → the credential is valid; the canonical username.
    CredentialVerified { user: String },

    /// ExecInJail → the shell is running inside the jail on a pty. Two fds
    /// follow over SCM_RIGHTS, in order: `[procdesc, pty_master]`. `uid` is
    /// the uid the shell actually runs as inside the jail (the jail's
    /// app-uid, or 0 if `want_root` was granted) — for the caller's logs.
    JailExecStarted { pid: i32, uid: u32 },
}

/// Send one request and read one response over a connected stream.
/// Convenience for synchronous client code.
pub fn round_trip(stream: &mut UnixStream, req: &Request) -> io::Result<Response> {
    write_request(stream, req)?;
    read_response(stream)
}

pub fn write_request<W: Write>(w: &mut W, req: &Request) -> io::Result<()> {
    let mut line = serde_json::to_vec(req).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> io::Result<()> {
    let mut line = serde_json::to_vec(resp).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Request> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed"));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                 format!("bad request frame: {e}")))
}

pub fn read_response<R: Read>(r: &mut R) -> io::Result<Response> {
    let mut br = io::BufReader::new(r);
    let mut line = String::new();
    let n = br.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed"));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                 format!("bad response frame: {e}")))
}

use std::io::Read;

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_toml::NetworkCap;

    #[test]
    fn roundtrip_authorize_request() {
        let mut caps = Capabilities::default();
        caps.clipboard = Some(true);
        caps.network   = Some(NetworkCap::Loopback);
        let req = Request::Authorize {
            app_id:        "org.atrium.edit".into(),
            manifest_hash: "sha256:abc".into(),
            requested:     caps,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"authorize\""));
        let back: Request = serde_json::from_str(&s).unwrap();
        match back {
            Request::Authorize { app_id, .. } => assert_eq!(app_id, "org.atrium.edit"),
            _ => panic!("wrong variant"),
        }
    }

    /// Lock the governor-broker wire format (portcullisd + the daemons depend on
    /// it): the `op` tags and the bounded GovSignal round-trip.
    #[test]
    fn roundtrip_govern_requests() {
        let r = Request::GovernReap {
            jail_name: "app-org-atrium-web".into(),
            signal: GovSignal::Exit,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"govern_reap\""));
        assert!(s.contains("\"signal\":\"exit\""));
        match serde_json::from_str::<Request>(&s).unwrap() {
            Request::GovernReap { jail_name, signal } => {
                assert_eq!(jail_name, "app-org-atrium-web");
                assert_eq!(signal, GovSignal::Exit);
            }
            _ => panic!("wrong variant"),
        }

        let r = Request::GovernSetRctl {
            jail_name: "app-org-atrium-web".into(),
            memoryuse_mb: 2048,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"govern_set_rctl\""));
        match serde_json::from_str::<Request>(&s).unwrap() {
            Request::GovernSetRctl { memoryuse_mb, .. } => assert_eq!(memoryuse_mb, 2048),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_needs_approval_response() {
        let r = Response::NeedsApproval {
            delta: vec!["Use the fresco graphics service".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back {
            Response::NeedsApproval { delta } => assert_eq!(delta.len(), 1),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn newline_framing_via_pipe() {
        use std::io::Cursor;
        let req1 = Request::Ping;
        let req2 = Request::Reload;
        let mut buf = Vec::new();
        write_request(&mut buf, &req1).unwrap();
        write_request(&mut buf, &req2).unwrap();
        let mut br = io::BufReader::new(Cursor::new(buf));
        let r1 = read_request(&mut br).unwrap();
        let r2 = read_request(&mut br).unwrap();
        matches!(r1, Request::Ping);
        matches!(r2, Request::Reload);
    }
}
