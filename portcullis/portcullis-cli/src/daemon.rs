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
use std::os::unix::net::UnixStream;
use std::path::Path;

use portcullis_ipc::{
    round_trip, Request, Response, PROTO_VERSION, SOCKET_PATH,
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

/// Outcome of asking the daemon to launch an app. `Exited(code)`
/// holds the wrapped app's exit code; `Failed{stage, message}` is
/// any setup/teardown error (with a coarse stage tag like
/// "manifest", "mount", "jail-c", "policy").
pub enum LaunchReply {
    Exited { code: Option<i32> },
    Failed { stage: String, message: String },
}

/// Forward a launch to portcullisd. Synchronous: returns when the
/// jail has exited and been torn down. `Ok(None)` means daemon
/// offline — caller should fall back to running the launch in-process.
pub fn launch(app_id: &str, bypass_policy: bool) -> DaemonResult<LaunchReply> {
    let Some(mut s) = opened()? else { return Ok(None) };
    let resp = round_trip(&mut s, &Request::Launch {
        app_id:        app_id.into(),
        bypass_policy,
    })?;
    match resp {
        Response::LaunchExit { code }            => Ok(Some(LaunchReply::Exited { code })),
        Response::LaunchFailed { stage, message } => Ok(Some(LaunchReply::Failed { stage, message })),
        Response::Error{message}                  => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected launch reply: {other:?}"))),
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
