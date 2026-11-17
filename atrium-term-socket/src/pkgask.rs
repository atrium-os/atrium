//! A workspace console's requests (homes.md §5.5), which only a press of
//! the person's can grant. stoa-term takes them (`take_ask`); the
//! workspace is always the tab's, never the request's.
//!
//! - `ESC ] 7770 ; pkg ; <words> BEL` from `/atrium/bin/pkg`: "<workspace>
//!   asks to run: pkg <words>" with Install and Cancel. Install sends
//!   portcullisd `WorkspaceAdmin` with the press and shows pkg's output over
//!   the tab, then answers `ok;<status>`.
//! - `ESC ] 7770 ; run ; <folder> BEL` from `/atrium/bin/atrium-run` (`make
//!   run`): "<workspace> asks to Run <folder>" with Run and Cancel. Run
//!   sends `WorkspaceRun` with the press and answers `ok;0`, or shows the
//!   error and answers `ok;1`.
//!
//! Cancel (or Esc) answers `cancel`. Answers go on the pty's input.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

use portcullis_ipc::{round_trip, write_request, Request, Response, PROTO_VERSION};

/// The answer to Cancel (and to an ask the Terminal can't take).
pub const CANCEL: &[u8] = b"\x1b]7770;cancel\x07\r";

/// The answer once pkg has run.
pub fn ok_reply(status: i32) -> Vec<u8> {
    format!("\x1b]7770;ok;{status}\x07\r").into_bytes()
}

/// The workspace a console's app id names (`workspace/<name>`).
pub fn workspace_of(app: Option<&str>) -> Option<&str> {
    app?.strip_prefix("workspace/").filter(|w| !w.is_empty())
}

/// A request: pkg with its words, or Run with its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Pkg(String),
    Run(String),
}

impl Ask {
    /// From stoa-term's `"<kind> <argument>"`.
    pub fn parse(s: &str) -> Option<Ask> {
        match s.split_once(' ')? {
            ("pkg", w) => Some(Ask::Pkg(w.to_string())),
            ("run", f) => Some(Ask::Run(f.to_string())),
            _ => None,
        }
    }

    /// What the person is asked, and the button that says yes.
    pub fn question(&self, ws: &str) -> (String, &'static str) {
        match self {
            Ask::Pkg(words) => (format!("{ws} asks to run: pkg {words}"), "Install"),
            Ask::Run(folder) => (format!("{ws} asks to Run {folder}"), "Run"),
        }
    }
}

/// Run `folder` of workspace `ws` as a dev app, spending the press.
pub fn run(ws: &str, folder: &str, activation: [u8; 16]) -> Result<String, String> {
    let mut s = connect()?;
    match round_trip(&mut s, &Request::WorkspaceRun { name: ws.into(), project: folder.into(), activation }) {
        Ok(Response::WorkspaceRan { app_id, .. }) => Ok(app_id),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("{other:?}")),
    }
}

fn connect() -> Result<std::os::unix::net::UnixStream, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => Ok(s),
        other => Err(format!("handshake: {other:?}")),
    }
}

/// pkg's status from its output, which ends `pkg: exit <status>`.
pub fn status_of(out: &str) -> Option<i32> {
    out.lines().rev().find_map(|l| l.trim().strip_prefix("pkg: exit ")?.trim().parse().ok())
}

/// pkg running in the workspace: its output, drawn over the tab.
pub struct Admin {
    _pd: Option<OwnedFd>,
    _stdin: File,
    out: File,
    /// The output, as a terminal shows it.
    pub view: stoa_term::Terminal,
    /// The output's text, for the status.
    text: String,
    /// The status once it has ended (`None` while it runs).
    pub done: Option<i32>,
}

impl Admin {
    /// Ask portcullisd to run `pkg <words>` in workspace `ws`, spending the
    /// press `activation`.
    pub fn start(ws: &str, words: &str, activation: [u8; 16], cols: u16, rows: u16) -> Result<Admin, String> {
        let mut s = connect()?;
        let mut argv = vec!["pkg".to_string()];
        argv.extend(words.split(' ').map(str::to_string));
        write_request(&mut s, &Request::WorkspaceAdmin { name: ws.into(), argv, activation }).map_err(|e| e.to_string())?;
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
        unsafe {
            let fl = libc::fcntl(out.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(out.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let mut view = stoa_term::Terminal::new(cols, rows);
        view.feed(format!("$ pkg {words}\r\n").as_bytes());
        Ok(Admin { _pd: fds.pop(), _stdin: stdin, out, view, text: String::new(), done: None })
    }

    pub fn fd(&self) -> i32 {
        self.out.as_raw_fd()
    }

    /// Take what pkg wrote. True when it has ended (`done` is then set).
    pub fn read(&mut self, buf: &mut [u8]) -> bool {
        loop {
            match self.out.read(buf) {
                Ok(0) => break,
                Ok(n) => {
                    // pkg writes bare newlines: a terminal needs CR too.
                    let mut b = Vec::with_capacity(n + 16);
                    for &c in &buf[..n] {
                        if c == b'\n' {
                            b.push(b'\r');
                        }
                        b.push(c);
                    }
                    self.view.feed(&b);
                    self.text.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if self.text.len() > 1 << 16 {
                        let cut = self.text.len() - (1 << 15);
                        let cut = (cut..self.text.len()).find(|&i| self.text.is_char_boundary(i)).unwrap_or(0);
                        self.text.drain(..cut);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let status = status_of(&self.text).unwrap_or(1);
        self.view.feed(b"\r\n[pkg finished: any key goes back to the shell]");
        self.done = Some(status);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_and_names() {
        assert_eq!(ok_reply(0), b"\x1b]7770;ok;0\x07\r");
        assert_eq!(CANCEL, b"\x1b]7770;cancel\x07\r");
        assert_eq!(workspace_of(Some("workspace/scratch")), Some("scratch"));
        assert_eq!(workspace_of(Some("empty")), None);
        assert_eq!(workspace_of(Some("workspace/")), None);
        assert_eq!(workspace_of(None), None);
        assert_eq!(Ask::parse("pkg install helix"), Some(Ask::Pkg("install helix".into())));
        assert_eq!(Ask::parse("run app"), Some(Ask::Run("app".into())));
        assert_eq!(Ask::parse("rm x"), None);
        assert_eq!(Ask::Pkg("install helix".into()).question("scratch"), ("scratch asks to run: pkg install helix".into(), "Install"));
        assert_eq!(Ask::Run(".".into()).question("proj"), ("proj asks to Run .".into(), "Run"));
        assert_eq!(status_of("Installing helix...\npkg: exit 0\n"), Some(0));
        assert_eq!(status_of("pkg: No packages available\npkg: exit 70"), Some(70));
        assert_eq!(status_of("nothing"), None);
    }
}
