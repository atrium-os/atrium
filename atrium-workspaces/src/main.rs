//! Workspaces: the window, and portcullisd's answers on threads of their
//! own (homes.md §5). Its `workspaces` capability lets it list, make,
//! change, stop and delete the person's workspaces and open their
//! terminals; deleting and opening need the press that asked.
//!
//! Environment: portcullisd's socket (portcullis_ipc::resolve_socket_path),
//! `WORKSPACES_SIZE` (900x600), `WORKSPACES_TRACE` (print the state on each
//! change, for checks).

use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atrium_workspaces::{title, Queue, State, Want, WorkspacesView};
use pergola::text_edit::TextEdit;
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};
use portcullis_ipc::{round_trip, Request, Response, Toolchain, WorkspaceInfo, PROTO_VERSION};

/// What a call came to.
enum Got {
    List(Result<(Vec<WorkspaceInfo>, Vec<Toolchain>), String>),
    /// A step done: what to say (an error if so), and whether to list again.
    Done(Result<String, String>),
}

fn portcullisd() -> Result<UnixStream, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => Ok(s),
        other => Err(format!("portcullisd: {other:?}")),
    }
}

fn ask(req: &Request) -> Result<Response, String> {
    let mut s = portcullisd()?;
    match round_trip(&mut s, req) {
        Ok(Response::Error { message }) => Err(message),
        Ok(r) => Ok(r),
        Err(e) => Err(format!("portcullisd: {e}")),
    }
}

fn listed(r: Result<Response, String>) -> Result<(Vec<WorkspaceInfo>, Vec<Toolchain>), String> {
    match r? {
        Response::WorkspaceList { workspaces, toolchains } => Ok((workspaces, toolchains)),
        other => Err(format!("unexpected answer: {other:?}")),
    }
}

fn open(name: &str, activation: [u8; 16]) -> Result<String, String> {
    match ask(&Request::Console { app_id: format!("workspace/{name}"), activation, tab_of: None, new_window: false })? {
        Response::ConsoleOpened { title } => Ok(format!("{title} is open")),
        other => Err(format!("unexpected answer: {other:?}")),
    }
}

/// Carry `w` out on a thread; the answer and a fresh list come back on `tx`.
fn run(w: Want, tx: Sender<Got>) {
    std::thread::spawn(move || {
        let done = match w {
            Want::List => None,
            Want::New { name, toolchain, network, activation } => Some(
                listed(ask(&Request::WorkspaceNew { name: name.clone(), toolchain, network: Some(network), uses: vec![] }))
                    .and_then(|_| open(&name, activation).map(|m| format!("{} made. {m}", title(&name))))),
            Want::Set { name, network, uses } => Some(
                listed(ask(&Request::WorkspaceSet { name: name.clone(), network, uses }))
                    .map(|_| format!("{} changed", title(&name)))),
            Want::Stop(name) => Some(
                listed(ask(&Request::WorkspaceStop { name: name.clone() })).map(|_| format!("{} stopped", title(&name)))),
            Want::Delete(name, activation) => Some(
                listed(ask(&Request::WorkspaceDelete { name: name.clone(), activation }))
                    .map(|_| format!("{} deleted", title(&name)))),
            Want::Open(name, activation) => Some(open(&name, activation)),
            Want::Duplicate { from, to } => Some(
                listed(ask(&Request::WorkspaceDuplicate { name: from.clone(), new_name: to.clone() }))
                    .map(|_| format!("{} copied as {}", title(&from), title(&to)))),
            Want::Run { name, project, activation } => Some(match ask(&Request::WorkspaceRun { name: name.clone(), project, activation }) {
                Ok(Response::WorkspaceRan { app_id, capabilities }) => Ok(format!("Running {app_id}{}",
                    if capabilities.is_empty() { String::new() } else { format!(" with {}", capabilities.join(", ")) })),
                Ok(other) => Err(format!("unexpected answer: {other:?}")),
                Err(e) => Err(e),
            }),
        };
        if let Some(d) = done {
            let _ = tx.send(Got::Done(d));
        }
        let _ = tx.send(Got::List(listed(ask(&Request::Workspaces))));
    });
}

fn main() {
    let (w, h) = std::env::var("WORKSPACES_SIZE").ok()
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((900, 600));
    let mut win = match Window::open(&WindowDesc::new("Workspaces", SizeSpec::Fixed(w, h))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("atrium-workspaces: frescod: {e}");
            std::process::exit(1);
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(State::default(), dirty.clone());
    let name = Mutable::with_dirty(TextEdit::new(""), dirty.clone());
    let project = Mutable::with_dirty(TextEdit::new(""), dirty.clone());
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let queue: Queue = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspacesView { state: state.clone(), name: name.clone(), project: project.clone(), size: size.clone(), queue: queue.clone() };
    let mut app = App::new_with_flag(view, dirty);
    let (tx, rx) = channel();
    run(Want::List, tx.clone());
    let trace = std::env::var_os("WORKSPACES_TRACE").is_some();
    let mut traced = String::new();
    // Workspaces run and stop from elsewhere too (Forum, a terminal's exit).
    let mut next_list = Instant::now() + Duration::from_secs(5);
    let mut failed = false;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(100))) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("atrium-workspaces: frescod: {e}");
                failed = true;
                break;
            }
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        let acts: Vec<_> = std::mem::take(&mut *queue.lock().unwrap_or_else(|p| p.into_inner()));
        for a in acts {
            let mut s = state.get_cloned();
            let creating = matches!(a, atrium_workspaces::Act::Create(_));
            let text = if matches!(a, atrium_workspaces::Act::Run(..)) { project.get_cloned().text().to_string() } else { name.get_cloned().text().to_string() };
            let want = s.act(a, &text);
            if creating && want.is_some() {
                s.creating = false;
                s.selected = name.get_cloned().text().trim().to_string().into();
                name.set(TextEdit::new(""));
            }
            if want.is_some() {
                s.note = None;
            }
            state.set(s);
            if let Some(w) = want {
                run(w, tx.clone());
            }
        }
        while let Ok(g) = rx.try_recv() {
            let mut s = state.get_cloned();
            match g {
                Got::List(Ok((l, t))) => s.take_list(l, t),
                Got::List(Err(e)) => s.note = Some((format!("Can't list your workspaces: {e}"), true)),
                Got::Done(r) => {
                    s.busy = None;
                    s.note = Some(match r {
                        Ok(m) => (m, false),
                        Err(e) => (e, true),
                    });
                }
            }
            state.set(s);
        }
        if Instant::now() >= next_list {
            next_list = Instant::now() + Duration::from_secs(5);
            run(Want::List, tx.clone());
        }
        if trace {
            let s = state.get_cloned();
            let line = format!("list=[{}] selected={:?} creating={} confirm_delete={} note={:?}",
                s.list.iter().map(|w| format!("{}{}{}", w.name, if w.running { "*" } else { "" },
                    if w.uses.is_empty() { String::new() } else { format!("<{}", w.uses.join("+")) })).collect::<Vec<_>>().join(","),
                s.selected, s.creating, s.confirm_delete, s.note.as_ref().map(|n| &n.0));
            if line != traced {
                eprintln!("atrium-workspaces: {line}");
                traced = line;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
