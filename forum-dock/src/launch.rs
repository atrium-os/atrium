//! Asking the TCB to launch an app, and the few WM intents the dock sends
//! (docs/spec/forum.md §6, §6.1).
//!
//! The dock is only the requester: portcullisd does the policy check, the
//! per-app uid and the jail. These functions never exit the process; the
//! CLI and the Applications view decide what to do with each outcome.

use std::io;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response, PROTO_VERSION};

/// What portcullisd answered before the app ran.
#[derive(Debug)]
pub enum PreLaunch {
    /// Accepted: the app is starting. Wait on it with [`Started::wait`].
    Started(Started),
    /// A single-instance app is up already, as `uid`.
    AlreadyRunning { jid: i32, uid: u32 },
    /// The policy gate wants the person's approval of these capabilities.
    NeedsApproval(Vec<String>),
    /// Refused or failed, in the daemon's words (or ours, when it is not there).
    Failed(String),
}

/// A launch portcullisd accepted. The connection stays open while the app runs.
#[derive(Debug)]
pub struct Started {
    stream: UnixStream,
}

impl Started {
    /// Block until the app exits: its exit code, or the failure the daemon
    /// reported after it started.
    pub fn wait(mut self) -> Result<Option<i32>, String> {
        match read_response(&mut self.stream) {
            Ok(Response::LaunchExit { code }) => Ok(code),
            Ok(Response::LaunchFailed { stage, message }) => Err(format!("failed at {stage}: {message}")),
            Ok(other) => Err(format!("unexpected reply: {other:?}")),
            Err(e) => Err(format!("lost portcullisd: {e}")),
        }
    }
}

/// Ask portcullisd to launch `app_id`, handing it `stdio` for the app.
/// Mirrors the portcullis CLI's wire dance: Hello → Launch → ReadyForFds →
/// stdio over SCM_RIGHTS; the exit comes later ([`Started::wait`]).
pub fn request(app_id: &str, stdio: [RawFd; 3]) -> PreLaunch {
    // Resolve the daemon's socket rather than hardcoding the flat path: inside a
    // jail only the per-service directory is mounted (by `app-launch`).
    let Some(path) = portcullis_ipc::resolve_socket_path() else {
        return PreLaunch::Failed(format!(
            "portcullisd is not reachable (looked for {} and {}); a jailed dock needs `app-launch`",
            portcullis_ipc::SERVICE_SOCKET_PATH,
            portcullis_ipc::SOCKET_PATH
        ));
    };
    let mut s = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => return PreLaunch::Failed(format!("portcullisd is not reachable at {path}: {e}")),
    };
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
        Ok(Response::ProtoMismatch { server_version }) => {
            return PreLaunch::Failed(format!("portcullisd speaks v{server_version}, the dock v{PROTO_VERSION}"))
        }
        Ok(other) => return PreLaunch::Failed(format!("unexpected handshake reply: {other:?}")),
        Err(e) => return PreLaunch::Failed(format!("handshake: {e}")),
    }
    if let Err(e) = write_request(&mut s, &Request::Launch { app_id: app_id.into(), bypass_policy: false, persona: None }) {
        return PreLaunch::Failed(format!("send: {e}"));
    }
    match read_response(&mut s) {
        Ok(Response::ReadyForFds) => {}
        Ok(Response::LaunchNeedsApproval { delta }) => return PreLaunch::NeedsApproval(delta),
        Ok(Response::LaunchFailed { stage, message }) => return PreLaunch::Failed(format!("failed at {stage}: {message}")),
        // Decided up front, before anything is mounted: a second press on a
        // running app's tile lands here.
        Ok(Response::AlreadyRunning { jid, uid, .. }) => return PreLaunch::AlreadyRunning { jid, uid },
        Ok(Response::Error { message }) => return PreLaunch::Failed(message),
        Ok(other) => return PreLaunch::Failed(format!("unexpected reply: {other:?}")),
        Err(e) => return PreLaunch::Failed(format!("lost portcullisd: {e}")),
    }
    // The daemon is blocked in recv_fds: hand over the app's stdio.
    if let Err(e) = send_fds(&s, &stdio) {
        return PreLaunch::Failed(format!("handing over stdio: {e}"));
    }
    PreLaunch::Started(Started { stream: s })
}

// ── forum-wm intents ─────────────────────────────────────────────────────────

/// One intent, one connection, one reply (the forum-ctl wire).
pub fn intent(i: &forum_ctl::Intent) -> io::Result<forum_ctl::Reply> {
    let mut s = UnixStream::connect(forum_ctl::default_socket_path())?;
    let req = forum_ctl::encode(i).map_err(|e| io::Error::other(format!("encode: {e}")))?;
    forum_ctl::write_frame(&mut s, &req)?;
    forum_ctl::decode::<forum_ctl::Reply>(&forum_ctl::read_frame(&mut s)?).map_err(|e| io::Error::other(format!("decode: {e}")))
}

fn ack(r: io::Result<forum_ctl::Reply>) -> io::Result<()> {
    match r? {
        forum_ctl::Reply::Ack => Ok(()),
        forum_ctl::Reply::Err { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("{other:?}"))),
    }
}

/// Ask forum-wm to keep `pt` points free at the bottom of the screen.
pub fn reserve_bottom(pt: u32) -> io::Result<()> {
    ack(intent(&forum_ctl::Intent::ReserveBottom { pt }))
}

/// The session's surfaces, from forum-wm.
pub fn surfaces() -> io::Result<Vec<forum_ctl::WmSurfaceInfo>> {
    match intent(&forum_ctl::Intent::ListSurfaces)? {
        forum_ctl::Reply::Surfaces { surfaces, .. } => Ok(surfaces),
        forum_ctl::Reply::Err { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
    }
}

/// Ask forum-wm to focus the surface owned by `uid`: the running instance of
/// an app asked for again. Matching on `owner_uid` works from inside a jail
/// (the dock cannot read the launch registry; portcullisd told us the uid).
/// The focused surface id, or `None` if the app has no window (yet).
pub fn focus_running(uid: u32) -> io::Result<Option<u32>> {
    let Some(target) = surfaces()?.into_iter().find(|s| s.owner_uid == uid) else {
        return Ok(None);
    };
    ack(intent(&forum_ctl::Intent::Focus { surface_id: target.surface_id }))?;
    Ok(Some(target.surface_id))
}

/// Block until the person presses Super+A (forum-wm answers this intent late,
/// §6.1). `Ok(())` once per press; an error when forum-wm is gone or refuses.
pub fn wait_apps_key() -> io::Result<()> {
    ack(intent(&forum_ctl::Intent::WaitAppsKey))
}

/// Ask portcullisd for a console in `app_id` (homes.md §4), `activation`
/// the press on Terminal. Its window comes from the terminal app.
pub fn console(app_id: &str, activation: [u8; 16]) -> Result<String, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
        other => return Err(format!("handshake: {other:?}")),
    }
    match round_trip(&mut s, &Request::Console { app_id: app_id.into(), activation, tab_of: None, new_window: false }) {
        Ok(Response::ConsoleOpened { title }) => Ok(title),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

fn portcullisd() -> Result<UnixStream, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => Ok(s),
        other => Err(format!("handshake: {other:?}")),
    }
}

/// The person's workspaces (homes.md §5); Scratch is made at the first ask.
pub fn workspaces() -> Result<Vec<portcullis_ipc::WorkspaceInfo>, String> {
    workspace_request(&Request::Workspaces)
}

/// Make a workspace with the defaults (C, outbound network).
pub fn new_workspace(name: &str) -> Result<Vec<portcullis_ipc::WorkspaceInfo>, String> {
    workspace_request(&Request::WorkspaceNew { name: name.into(), toolchain: "c".into(), network: None, uses: vec![] })
}

fn workspace_request(req: &Request) -> Result<Vec<portcullis_ipc::WorkspaceInfo>, String> {
    let mut s = portcullisd()?;
    match round_trip(&mut s, req) {
        Ok(Response::WorkspaceList { workspaces, .. }) => Ok(workspaces),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}
