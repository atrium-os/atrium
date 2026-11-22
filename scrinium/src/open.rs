//! Asking portcullisd to open a file in an app (opening-files.md §6.2):
//! which apps open a type (`OpenApps`), and the hand-over itself
//! (`OpenFile`, the press's token, then the descriptor). Called off the
//! window's thread.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use portcullis_ipc::{round_trip, send_fds, Request, Response, PROTO_VERSION};

use crate::browser::Choice;

/// portcullisd's socket in Scrinium's jail (`SCRINIUM_PORTCULLIS_SOCKET`
/// overrides).
pub fn socket() -> PathBuf {
    std::env::var_os("SCRINIUM_PORTCULLIS_SOCKET").map(PathBuf::from).unwrap_or_else(|| "/atrium/sockets/portcullis/portcullis.sock".into())
}

fn connect() -> Result<UnixStream, String> {
    let mut s = UnixStream::connect(socket()).map_err(|e| format!("Atrium's app launcher isn't reachable ({e})"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => Ok(s),
        Ok(r) => Err(format!("the app launcher answered {r:?}")),
        Err(e) => Err(format!("the app launcher: {e}")),
    }
}

/// The apps that open `mime`, the default first.
pub fn apps(mime: &str) -> Result<Vec<Choice>, String> {
    let mut s = connect()?;
    match round_trip(&mut s, &Request::OpenApps { mime: mime.into() }).map_err(|e| e.to_string())? {
        Response::OpenApps { apps } => Ok(apps.into_iter().map(|a| Choice { id: a.id, name: a.name, edit: a.edit, default: a.default }).collect()),
        Response::Error { message } => Err(message),
        r => Err(format!("an odd answer: {r:?}")),
    }
}

/// Hand `file` to `app`; the app's name on success.
pub fn open(app: &str, mime: &str, name: &str, write: bool, always: bool, token: [u8; 16], file: &std::fs::File, path: &std::path::Path) -> Result<String, String> {
    open_file(app, mime, name, write, always, token, file, None, Some(path))
}

pub fn open_workbook(parent: u32, mime: &str, name: &str, write: bool, token: [u8; 16], file: &std::fs::File, path: &std::path::Path) -> Result<String, String> {
    open_file("org.atrium.sheets", mime, name, write, false, token, file, Some(parent), Some(path))
}

fn open_file(app: &str, mime: &str, name: &str, write: bool, always: bool, token: [u8; 16], file: &std::fs::File, workbook_picker: Option<u32>, path: Option<&std::path::Path>) -> Result<String, String> {
    let mut s = connect()?;
    let workbook_commit = if write && app == "org.atrium.sheets" { Some(crate::workbook::grant(path.ok_or("Missing workbook path")?, file).map_err(|e| e.to_string())?) } else { None };
    let req = Request::OpenFile { app: app.into(), mime: mime.into(), name: name.into(), write, activation: token, always, workbook_picker, workbook_commit };
    match round_trip(&mut s, &req).map_err(|e| e.to_string())? {
        Response::ReadyForFds => {}
        Response::Error { message } => return Err(message),
        r => return Err(format!("an odd answer: {r:?}")),
    }
    send_fds(&s, &[file.as_raw_fd()]).map_err(|e| e.to_string())?;
    match portcullis_ipc::read_response(&mut s).map_err(|e| e.to_string())? {
        Response::Opened { app, .. } => Ok(app),
        Response::Error { message } => Err(message),
        r => Err(format!("an odd answer: {r:?}")),
    }
}

/// Ask the host to prove retention or return one read-only historical descriptor.
pub fn workbook_version(
    current: &std::fs::File,
    previous: &std::fs::File,
    generation: Option<u64>,
) -> Result<(Option<u64>, Option<std::fs::File>), String> {
    let mut socket = connect()?;
    match round_trip(&mut socket, &Request::WorkbookVersion { generation }).map_err(|e| e.to_string())? {
        Response::ReadyForFds => {}
        Response::Error { message } => return Err(message),
        _ => return Err("Unexpected workbook version reply".into()),
    }
    send_fds(&socket, &[current.as_raw_fd(), previous.as_raw_fd()]).map_err(|e| e.to_string())?;
    match portcullis_ipc::read_response(&mut socket).map_err(|e| e.to_string())? {
        Response::WorkbookVersion { generation: retained } => {
            let file = if generation.is_some() && retained == generation {
                let mut fds = portcullis_ipc::recv_fds(&socket, 1).map_err(|e| e.to_string())?;
                Some(std::fs::File::from(fds.pop().ok_or("Missing workbook version descriptor")?))
            } else {
                None
            };
            Ok((retained, file))
        }
        Response::Error { message } => Err(message),
        _ => Err("Unexpected workbook version reply".into()),
    }
}

pub fn open_workbook_version(parent: u32, name: &str, activation: [u8; 16], file: &std::fs::File) -> Result<String, String> {
    use std::os::unix::fs::FileExt;
    let mut head = [0; atrium_types::HEAD];
    let n = file.read_at(&mut head, 0).map_err(|e| e.to_string())?;
    let detected = atrium_types::detect(name, &head[..n]);
    let ty = detected.known().ok_or_else(|| "Unknown workbook format".to_string())?;
    let mime = if ty.mime == "text/plain" { "text/csv" } else { ty.mime };
    open_file("org.atrium.sheets", mime, name, false, false, activation, file, Some(parent), None)
}
