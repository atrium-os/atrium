//! Running things in the workspace: `WorkspaceExec` (portcullisd starts the
//! command in the workspace's jail, as its uid, and hands Artifex the
//! process descriptor, stdin and output), or, for working on Artifex
//! itself off Atrium, plain local processes in a folder that stands in
//! for `/work` (`ARTIFEX_LOCAL_WORK`).

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use portcullis_ipc::{round_trip, write_request, Request, Response, WorkspaceInfo, PROTO_VERSION};

/// A running command. Dropping it ends the command (closing a process
/// descriptor kills its process).
pub struct Proc {
    _pd: Option<OwnedFd>,
    child: Option<std::process::Child>,
    stdin: Option<File>,
    out: Option<File>,
    /// What is waiting to go to stdin.
    pending: Vec<u8>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn nonblocking(fd: &impl AsRawFd) {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

impl Proc {
    fn new(pd: Option<OwnedFd>, child: Option<std::process::Child>, stdin: File, out: File) -> Proc {
        nonblocking(&stdin);
        nonblocking(&out);
        Proc { _pd: pd, child, stdin: Some(stdin), out: Some(out), pending: Vec::new() }
    }

    /// Queue bytes for stdin (written as the pipe takes them).
    pub fn write(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        self.flush();
    }

    pub fn flush(&mut self) {
        let Some(f) = &mut self.stdin else {
            self.pending.clear();
            return;
        };
        while !self.pending.is_empty() {
            match f.write(&self.pending) {
                Ok(0) => break,
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.stdin = None;
                    self.pending.clear();
                    break;
                }
            }
        }
    }

    /// No more input (the command sees end of file).
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// What the command wrote since the last call (stdout and stderr), and
    /// whether its output has ended.
    pub fn read(&mut self, max: usize) -> (Vec<u8>, bool) {
        let mut got = Vec::new();
        let Some(f) = &mut self.out else { return (got, true) };
        let mut buf = [0u8; 16384];
        while got.len() < max {
            match f.read(&mut buf) {
                Ok(0) => {
                    self.out = None;
                    return (got, true);
                }
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.out = None;
                    return (got, true);
                }
            }
        }
        (got, false)
    }
}

/// Where commands run and what the person's workspaces are.
pub trait Exec {
    fn list(&self) -> Result<Vec<WorkspaceInfo>, String>;
    /// Pick `ws` for Artifex with a press.
    fn pick(&self, ws: &str, activation: [u8; 16]) -> Result<(), String>;
    /// Start `argv` in `ws`, in folder `cwd` of its `work`.
    fn spawn(&self, ws: &str, argv: &[String], cwd: &str) -> Result<Proc, String>;
    /// Open the workspace's terminal with a press.
    fn terminal(&self, ws: &str, activation: [u8; 16]) -> Result<(), String>;
    /// Run folder `folder` of `ws`'s work as a dev app (W5), with a press:
    /// the app's id, and the inspector's socket to it (dev-tools.md §3.6).
    fn run(&self, ws: &str, folder: &str, activation: [u8; 16]) -> Result<(String, Option<OwnedFd>), String>;
    /// Debug folder `folder` as a dev app, with a press (dev-tools.md
    /// §7.3): the app's id, the debugger's address on the workspace's
    /// loopback (`gdb-remote`) and the program's path. The app waits for
    /// the debugger.
    fn debug(&self, ws: &str, folder: &str, activation: [u8; 16]) -> Result<(String, String, String), String>;
    /// Where picked workspaces' `work` folders are (`/work`).
    fn work_root(&self) -> PathBuf;
}

/// Through portcullisd (Artifex's manifest holds `workspace-exec`).
pub struct Portcullis;

fn connect() -> Result<UnixStream, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => Ok(s),
        other => Err(format!("portcullisd: {other:?}")),
    }
}

fn ask(req: &Request) -> Result<Response, String> {
    let mut s = connect()?;
    match round_trip(&mut s, req) {
        Ok(Response::Error { message }) => Err(message),
        Ok(r) => Ok(r),
        Err(e) => Err(format!("portcullisd: {e}")),
    }
}

impl Exec for Portcullis {
    fn list(&self) -> Result<Vec<WorkspaceInfo>, String> {
        match ask(&Request::Workspaces)? {
            Response::WorkspaceList { workspaces, .. } => Ok(workspaces),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    fn pick(&self, ws: &str, activation: [u8; 16]) -> Result<(), String> {
        match ask(&Request::WorkspacePick { name: ws.into(), activation })? {
            Response::Ok => Ok(()),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    fn spawn(&self, ws: &str, argv: &[String], cwd: &str) -> Result<Proc, String> {
        let mut s = connect()?;
        write_request(&mut s, &Request::WorkspaceExec { name: ws.into(), argv: argv.to_vec(), cwd: cwd.into() })
            .map_err(|e| e.to_string())?;
        match portcullis_ipc::read_response_unbuffered(&mut s) {
            Ok(Response::WorkspaceExecStarted { .. }) => {}
            Ok(Response::Error { message }) => return Err(message),
            other => return Err(format!("{other:?}")),
        }
        let mut fds = portcullis_ipc::recv_fds(&s, 3).map_err(|e| format!("recv_fds: {e}"))?;
        if fds.len() != 3 {
            return Err(format!("{} descriptors", fds.len()));
        }
        let out = File::from(fds.pop().unwrap());
        let stdin = File::from(fds.pop().unwrap());
        Ok(Proc::new(fds.pop(), None, stdin, out))
    }

    fn terminal(&self, ws: &str, activation: [u8; 16]) -> Result<(), String> {
        match ask(&Request::Console { app_id: format!("workspace/{ws}"), activation, tab_of: None, new_window: false })? {
            Response::ConsoleOpened { .. } => Ok(()),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    fn run(&self, ws: &str, folder: &str, activation: [u8; 16]) -> Result<(String, Option<OwnedFd>), String> {
        let mut s = connect()?;
        write_request(&mut s, &Request::WorkspaceInspect { name: ws.into(), project: folder.into(), activation })
            .map_err(|e| e.to_string())?;
        let app_id = match portcullis_ipc::read_response_unbuffered(&mut s) {
            Ok(Response::WorkspaceRan { app_id, .. }) => app_id,
            Ok(Response::Error { message }) => return Err(message),
            other => return Err(format!("unexpected answer: {other:?}")),
        };
        // The app runs either way; without its socket it isn't inspected.
        let sock = portcullis_ipc::recv_fds(&s, 1).ok().and_then(|mut f| (f.len() == 1).then(|| f.remove(0)));
        Ok((app_id, sock))
    }

    fn debug(&self, ws: &str, folder: &str, activation: [u8; 16]) -> Result<(String, String, String), String> {
        match ask(&Request::WorkspaceDebug { name: ws.into(), project: folder.into(), activation })? {
            Response::WorkspaceDebugging { app_id, address, program, .. } => Ok((app_id, address, program)),
            other => Err(format!("unexpected answer: {other:?}")),
        }
    }

    fn work_root(&self) -> PathBuf {
        PathBuf::from("/work")
    }
}

/// Local processes; each folder of `root` is a workspace, all picked.
pub struct Local {
    pub root: PathBuf,
}

impl Exec for Local {
    fn list(&self) -> Result<Vec<WorkspaceInfo>, String> {
        let mut out: Vec<WorkspaceInfo> = std::fs::read_dir(&self.root)
            .map_err(|e| format!("{}: {e}", self.root.display()))?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .map(|name| WorkspaceInfo {
                name,
                toolchain: "local".into(),
                persona: "personal".into(),
                network: "outbound".into(),
                uses: vec![],
                running: true,
                object_cache: false,
                hosts: vec![],
                hosts_default: true,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn pick(&self, _ws: &str, _activation: [u8; 16]) -> Result<(), String> {
        Ok(())
    }

    fn spawn(&self, ws: &str, argv: &[String], cwd: &str) -> Result<Proc, String> {
        if cwd.split('/').any(|c| c == "..") || cwd.starts_with('/') {
            return Err(format!("{cwd:?} is not a folder in work"));
        }
        let dir = self.root.join(ws).join(cwd);
        let (prog, args) = argv.split_first().ok_or("nothing to run")?;
        let (out_r, out_w) = pipe()?;
        let out_w2 = out_w.try_clone().map_err(|e| e.to_string())?;
        let mut child = std::process::Command::new(prog)
            .args(args)
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(out_w)
            .stderr(out_w2)
            .spawn()
            .map_err(|e| format!("{prog}: {e}"))?;
        let stdin = File::from(OwnedFd::from(child.stdin.take().ok_or("no stdin")?));
        Ok(Proc::new(None, Some(child), stdin, out_r))
    }

    fn terminal(&self, _ws: &str, _activation: [u8; 16]) -> Result<(), String> {
        Err("No terminal off Atrium".into())
    }

    fn run(&self, _ws: &str, _folder: &str, _activation: [u8; 16]) -> Result<(String, Option<OwnedFd>), String> {
        Err("No Run off Atrium".into())
    }

    /// Off Atrium there is no app jail: the manifest's entry is started
    /// under `lldb-server gdbserver` on the loopback, as portcullisd starts
    /// a dev build, and waits for the debugger the same way.
    fn debug(&self, ws: &str, folder: &str, _activation: [u8; 16]) -> Result<(String, String, String), String> {
        if folder.split('/').any(|c| c == "..") || folder.starts_with('/') {
            return Err(format!("{folder:?} is not a folder in work"));
        }
        let dir = self.root.join(ws).join(folder);
        let m: toml::Value = std::fs::read_to_string(dir.join("atrium.toml")).map_err(|e| format!("atrium.toml: {e}"))?
            .parse().map_err(|e| format!("atrium.toml: {e}"))?;
        let app = m.get("app").ok_or("atrium.toml has no [app]")?;
        let id = app.get("id").and_then(|v| v.as_str()).ok_or("no app id")?;
        let entry = app.get("entry").and_then(|v| v.as_str()).ok_or("no entry")?;
        let program = dir.join(entry);
        if !program.is_file() {
            return Err(format!("{} isn't built", program.display()));
        }
        let fifo = std::env::temp_dir().join(format!("artifex-gdbserver-{}-{id}", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: a path we own, a fresh FIFO.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(format!("mkfifo: {}", std::io::Error::last_os_error()));
        }
        std::process::Command::new("lldb-server")
            .args(["gdbserver", "127.0.0.1:0", "--named-pipe"]).arg(&fifo).arg("--").arg(&program)
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("lldb-server: {e}"))?;
        // Blocks until lldb-server listens and names its port.
        let port = std::fs::read_to_string(&fifo).map_err(|e| format!("lldb-server's port: {e}"))?;
        let _ = std::fs::remove_file(&fifo);
        let port: u16 = port.trim_end_matches('\0').trim().parse().map_err(|_| format!("lldb-server's port: {port:?}"))?;
        Ok((format!("local.{id}"), format!("127.0.0.1:{port}"), program.to_string_lossy().into_owned()))
    }

    fn work_root(&self) -> PathBuf {
        self.root.clone()
    }
}

fn pipe() -> Result<(File, File), String> {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills two descriptors this process then owns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: both are fresh descriptors from pipe(2), owned once each.
    unsafe {
        use std::os::fd::FromRawFd;
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])))
    }
}

/// Is `p` inside `root` with no `..`?
pub fn inside(root: &Path, p: &Path) -> bool {
    crate::tree::relative(root, p).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_runs_with_pipes() {
        let d = std::env::temp_dir().join(format!("artifex-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("ws/sub")).unwrap();
        let l = Local { root: d.clone() };
        assert_eq!(l.list().unwrap()[0].name, "ws");
        let mut p = l.spawn("ws", &["sh".into(), "-c".into(), "pwd; cat; echo err >&2".into()], "sub").unwrap();
        p.write(b"through stdin\n");
        p.close_stdin();
        let mut all = Vec::new();
        for _ in 0..200 {
            let (b, done) = p.read(1 << 20);
            all.extend(b);
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let s = String::from_utf8_lossy(&all);
        assert!(s.contains("/ws/sub\n"), "{s}");
        assert!(s.contains("through stdin"), "{s}");
        assert!(s.contains("err"), "{s}");
        assert!(l.spawn("ws", &["true".into()], "../x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
