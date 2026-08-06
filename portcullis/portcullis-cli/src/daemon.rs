//! Thin client for talking to portcullisd over its Unix-domain socket.
//!
//! Every operation tries the daemon first; if the socket doesn't
//! exist (daemon not running) or the connection is refused, the
//! caller is expected to fall back to direct file access via
//! portcullis-policy. Errors from a daemon that DID answer (parse
//! errors, Error responses, etc.) are surfaced — those are real
//! failures, not "daemon is offline."
//!
//! Distinction matters because we don't want a flaky socket to
//! silently fall through to a stale file write that races the
//! daemon's in-memory copy: if the daemon is up, it's the
//! canonical writer.

use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use portcullis_ipc::{
    read_response, round_trip, send_fds, write_request,
    Request, Response, PROTO_VERSION, SOCKET_PATH,
};
use portcullis_toml::Capabilities;

/// Result of a daemon op: `Ok(Some(_))` = daemon answered; `Ok(None)`
/// = daemon not reachable, fall back; `Err(_)` = daemon answered with
/// an error or the wire broke mid-exchange.
pub type DaemonResult<T> = io::Result<Option<T>>;

/// Env override for the daemon socket — handy for E2E tests and
/// for running the daemon as a non-root user during development.
const SOCKET_ENV: &str = "PORTCULLIS_SOCKET";

fn socket_path() -> std::path::PathBuf {
    std::env::var_os(SOCKET_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(SOCKET_PATH).to_path_buf())
}

fn connect() -> io::Result<Option<UnixStream>> {
    let p = socket_path();
    if !p.exists() {
        return Ok(None);
    }
    match UnixStream::connect(&p) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused
               || e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn handshake(stream: &mut UnixStream) -> io::Result<()> {
    match round_trip(stream, &Request::Hello { version: PROTO_VERSION })? {
        Response::Hello { version } if version == PROTO_VERSION => Ok(()),
        Response::ProtoMismatch { server_version } =>
            Err(io::Error::other(
                format!("portcullisd speaks proto v{server_version}, \
                         this CLI speaks v{PROTO_VERSION}"))),
        other =>
            Err(io::Error::other(format!("unexpected hello reply: {other:?}"))),
    }
}

fn opened() -> io::Result<Option<UnixStream>> {
    let Some(mut s) = connect()? else { return Ok(None) };
    handshake(&mut s)?;
    Ok(Some(s))
}

/// Returns `Some(true)` if the daemon authorized the launch,
/// `Some(false)` (with `delta_lines` populated) if it needs approval,
/// `None` if the daemon is not running.
pub fn authorize(
    app_id: &str,
    manifest_hash: &str,
    requested: &Capabilities,
) -> DaemonResult<AuthorizeOutcome> {
    let Some(mut s) = opened()? else { return Ok(None) };
    let resp = round_trip(&mut s, &Request::Authorize {
        app_id:        app_id.into(),
        manifest_hash: manifest_hash.into(),
        requested:     requested.clone(),
    })?;
    match resp {
        Response::Authorized           => Ok(Some(AuthorizeOutcome::Authorized)),
        Response::NeedsApproval{delta} => Ok(Some(AuthorizeOutcome::NeedsApproval(delta))),
        Response::Error{message}       => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected authorize reply: {other:?}"))),
    }
}

pub enum AuthorizeOutcome {
    Authorized,
    NeedsApproval(Vec<String>),
}

/// Persist a grant via the daemon. `Ok(Some(()))` on success;
/// `Ok(None)` if daemon offline (caller should fall back).
pub fn grant(
    app_id: &str,
    manifest_hash: &str,
    capabilities: &Capabilities,
) -> DaemonResult<()> {
    let Some(mut s) = opened()? else { return Ok(None) };
    let resp = round_trip(&mut s, &Request::Grant {
        app_id:        app_id.into(),
        manifest_hash: manifest_hash.into(),
        capabilities:  capabilities.clone(),
    })?;
    match resp {
        Response::Ok               => Ok(Some(())),
        Response::Error{message}   => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected grant reply: {other:?}"))),
    }
}

pub fn revoke(app_id: &str) -> DaemonResult<()> {
    let Some(mut s) = opened()? else { return Ok(None) };
    let resp = round_trip(&mut s, &Request::Revoke { app_id: app_id.into() })?;
    match resp {
        Response::Ok               => Ok(Some(())),
        Response::Error{message}   => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected revoke reply: {other:?}"))),
    }
}

pub fn reload() -> DaemonResult<()> {
    let Some(mut s) = opened()? else { return Ok(None) };
    let resp = round_trip(&mut s, &Request::Reload)?;
    match resp {
        Response::Ok               => Ok(Some(())),
        Response::Error{message}   => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected reload reply: {other:?}"))),
    }
}

/// Outcome of asking the daemon to launch an app.
///   Exited        — the wrapped app ran and finished with this code.
///   Failed        — daemon-side setup/teardown error (manifest,
///                   mount, jail-c, etc.).
///   NeedsApproval — policy gate refused; caller should prompt the
///                   user and either persist a grant + retry
///                   (Allow Always), retry with bypass=true
///                   (Allow Once), or give up (Deny).
pub enum LaunchReply {
    Exited        { code: Option<i32> },
    Failed        { stage: String, message: String },
    NeedsApproval { delta: Vec<String> },
    /// The app is single-instance and one is already up. Not a failure: the
    /// requested state (this app is running) already holds.
    AlreadyRunning { jid: i32, uid: u32 },
}

/// Forward a launch to portcullisd. Synchronous: returns when the
/// jail has exited and been torn down. `Ok(None)` means daemon
/// offline — caller should fall back to running the launch in-process.
///
/// Wire dance: send Launch → expect ReadyForFds (or terminal
/// LaunchFailed for policy / manifest errors) → send our stdio fds
/// via SCM_RIGHTS → expect LaunchExit / LaunchFailed.
pub fn launch(app_id: &str, bypass_policy: bool) -> DaemonResult<LaunchReply> {
    let Some(mut s) = opened()? else { return Ok(None) };

    write_request(&mut s, &Request::Launch {
        app_id:        app_id.into(),
        bypass_policy,
    })?;

    /* Daemon's first reply is one of:
     *   ReadyForFds          → proceed to the fd handoff
     *   LaunchNeedsApproval  → policy refused; surface for prompting
     *   LaunchFailed         → manifest/build error; terminal
     *   Error                → protocol/internal failure
     */
    match read_response(&mut s)? {
        Response::ReadyForFds                     => { /* fall through */ }
        Response::LaunchNeedsApproval { delta }   => return Ok(Some(LaunchReply::NeedsApproval { delta })),
        Response::AlreadyRunning { jid, uid, .. } => return Ok(Some(LaunchReply::AlreadyRunning { jid, uid })),
        Response::LaunchFailed { stage, message } => return Ok(Some(LaunchReply::Failed { stage, message })),
        Response::Error{message}                  => return Err(io::Error::other(message)),
        other => return Err(io::Error::other(format!("unexpected pre-launch reply: {other:?}"))),
    }

    /* Hand over our stdio. The launched app reads/writes our tty
     * (or whatever 0/1/2 are bound to in this process). */
    let stdio_fds = [
        std::io::stdin().as_raw_fd(),
        std::io::stdout().as_raw_fd(),
        std::io::stderr().as_raw_fd(),
    ];
    send_fds(&s, &stdio_fds)?;

    match read_response(&mut s)? {
        Response::LaunchExit { code }             => Ok(Some(LaunchReply::Exited { code })),
        Response::LaunchFailed { stage, message } => Ok(Some(LaunchReply::Failed { stage, message })),
        Response::Error{message}                  => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected post-launch reply: {other:?}"))),
    }
}

pub fn ping() -> DaemonResult<()> {
    let Some(mut s) = opened()? else { return Ok(None) };
    let resp = round_trip(&mut s, &Request::Ping)?;
    match resp {
        Response::Pong             => Ok(Some(())),
        other => Err(io::Error::other(format!("unexpected ping reply: {other:?}"))),
    }
}
