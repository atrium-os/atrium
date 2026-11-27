//! Asking portcullisd to open a file in an app (opening-files.md §6.2):
//! which apps open a type (`OpenApps`), and the hand-over itself
//! (`OpenFile`, the press's token, then the descriptor). Called off the
//! window's thread.

use std::io::Write;
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

pub fn reopen_workbook(parent: u32, name: &str, activation: [u8; 16], pick: &crate::bookmarks::Pick) -> Result<String, String> {
    let mime = workbook_mime(name, &pick.file)?;
    let write = pick.write && mime != "application/vnd.ms-excel";
    let commit = write.then(|| || crate::workbook::grant_at(
        pick.folder.try_clone()?, pick.name.clone(), &pick.file, Some(pick.scope.clone()),
    ));
    launch_file("org.atrium.sheets", mime, name, write, false, activation, &pick.file, Some(parent), commit)
}

pub fn workbook_mime(name: &str, file: &std::fs::File) -> Result<&'static str, String> {
    use std::os::unix::fs::FileExt;
    let mut head = [0; atrium_types::HEAD];
    let n = file.read_at(&mut head, 0).map_err(|err| err.to_string())?;
    let detected = atrium_types::detect(name, &head[..n]);
    let ty = detected.known().ok_or_else(|| detected.question().unwrap_or_else(|| "Unknown workbook format".into()))?;
    Ok(if ty.mime == "text/plain" { "text/csv" } else { ty.mime })
}

fn open_file(app: &str, mime: &str, name: &str, write: bool, always: bool, token: [u8; 16], file: &std::fs::File, workbook_picker: Option<u32>, path: Option<&std::path::Path>) -> Result<String, String> {
    let workbook_commit = if write && app == "org.atrium.sheets" {
        let path = path.ok_or("Missing workbook path")?;
        Some(|| crate::workbook::grant(path, file))
    } else {
        None
    };
    let result = launch_file(app, mime, name, write, always, token, file, workbook_picker, workbook_commit)?;
    if app == "org.atrium.sheets" {
        if let Some(path) = path {
            if let Err(err) = crate::bookmarks::remember_path(path, file, name) {
                eprintln!("scrinium: could not remember workbook: {err}");
            }
        }
    }
    Ok(result)
}

fn launch_file(
    app: &str, mime: &str, name: &str, write: bool, always: bool, token: [u8; 16],
    file: &std::fs::File, workbook_picker: Option<u32>,
    prepare_commit: Option<impl FnOnce() -> std::io::Result<[u8; 16]>>,
) -> Result<String, String> {
    let mut s = connect()?;
    let req = Request::OpenFile {
        app: app.into(), mime: mime.into(), name: name.into(), write, activation: token,
        always, workbook_picker, workbook_commit: None, prepare_workbook_commit: prepare_commit.is_some(),
    };
    let ready = round_trip(&mut s, &req).map_err(|e| e.to_string())?;
    match (ready, prepare_commit) {
        (Response::ReadyForFds, None) => {}
        (Response::ReadyForWorkbookCommit, Some(prepare)) => {
            let grant = prepare().map_err(|err| err.to_string())?;
            s.write_all(&grant).map_err(|err| err.to_string())?;
        }
        (Response::Error { message }, _) => return Err(message),
        (r, _) => return Err(format!("the app launcher cannot complete this Open ({r:?})")),
    }
    send_fds(&s, &[file.as_raw_fd()]).map_err(|e| e.to_string())?;
    match portcullis_ipc::read_response(&mut s).map_err(|e| e.to_string())? {
        Response::Opened { app, .. } => Ok(app),
        Response::Error { message } => Err(message),
        r => Err(format!("an odd answer: {r:?}")),
    }
}

pub fn workbook_bookmark(
    file: &std::fs::File,
    expected: Option<portcullis_ipc::WorkbookBookmark>,
) -> Result<Option<portcullis_ipc::WorkbookBookmark>, String> {
    let mut socket = connect()?;
    match round_trip(&mut socket, &Request::WorkbookBookmark { expected }).map_err(|e| e.to_string())? {
        Response::ReadyForFds => {}
        Response::Error { message } => return Err(message),
        _ => return Err("Unexpected workbook bookmark reply".into()),
    }
    send_fds(&socket, &[file.as_raw_fd()]).map_err(|e| e.to_string())?;
    match portcullis_ipc::read_response(&mut socket).map_err(|e| e.to_string())? {
        Response::WorkbookBookmark { bookmark } => Ok(bookmark),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected workbook bookmark reply".into()),
    }
}

/// Ask the host to give a staged replacement the workbook's owner.
pub fn workbook_owner(original: &std::fs::File, staged: &std::fs::File) -> Result<(), String> {
    let mut socket = connect()?;
    match round_trip(&mut socket, &Request::WorkbookOwner).map_err(|e| e.to_string())? {
        Response::ReadyForFds => {}
        Response::Error { message } => return Err(message),
        _ => return Err("Unexpected workbook owner reply".into()),
    }
    send_fds(&socket, &[original.as_raw_fd(), staged.as_raw_fd()]).map_err(|e| e.to_string())?;
    match portcullis_ipc::read_response(&mut socket).map_err(|e| e.to_string())? {
        Response::WorkbookOwned => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected workbook owner reply".into()),
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
