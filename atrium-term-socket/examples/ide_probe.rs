//! ide_probe: a stand-in IDE for scripts/vm-ide-check.sh (homes.md §5.3,
//! W6). Installed with `workspace-exec`, it opens a window, lists the
//! person's workspaces, and asks to run a command in one before any pick
//! and to pick one with no press (both refused). Each click in its window
//! picks the next workspace in its bundle's `/targets` with that click's
//! press, then builds and runs `app/hello.c` there, writes a file in
//! `/work/<name>`, asks for a workspace it wasn't given, and starts a long
//! command it then drops. Its window is at (950, 400), off the Workspaces
//! app's. Reports on stderr, `ide-probe: ...`.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};
use portcullis_ipc::{round_trip, write_request, Request, Response, PROTO_VERSION};

fn connect() -> Result<UnixStream, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("no portcullisd")?;
    let mut s = UnixStream::connect(path).map_err(|e| format!("connect: {e}"))?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => Ok(s),
        other => Err(format!("handshake: {other:?}")),
    }
}

fn ask(req: Request) -> String {
    let mut s = match connect() {
        Ok(s) => s,
        Err(e) => return format!("err {e}"),
    };
    match round_trip(&mut s, &req) {
        Ok(Response::Ok) => "ok".into(),
        Ok(Response::WorkspaceList { workspaces, .. }) => {
            format!("ok {}", workspaces.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(","))
        }
        Ok(Response::Error { message }) => format!("err {message}"),
        other => format!("err {other:?}"),
    }
}

/// Start `argv` in `ws`: (procdesc, stdin, output), or why not.
fn exec(ws: &str, argv: &[&str], cwd: &str) -> Result<(OwnedFd, OwnedFd, OwnedFd), String> {
    let mut s = connect()?;
    write_request(&mut s, &Request::WorkspaceExec { name: ws.into(), argv: argv.iter().map(|a| a.to_string()).collect(), cwd: cwd.into() })
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
    let out = fds.pop().unwrap();
    let inp = fds.pop().unwrap();
    Ok((fds.pop().unwrap(), inp, out))
}

/// Run to the end: (its output, did it end).
fn run(ws: &str, argv: &[&str], cwd: &str) -> String {
    match exec(ws, argv, cwd) {
        Ok((pd, inp, out)) => {
            drop(inp);
            let mut text = String::new();
            let _ = std::fs::File::from(out).read_to_string(&mut text);
            let mut p = libc::pollfd { fd: pd.as_raw_fd(), events: libc::POLLHUP, revents: 0 };
            // SAFETY: one pollfd that outlives the call.
            let ended = unsafe { libc::poll(&mut p, 1, 30_000) } > 0;
            format!("ok{} {}", if ended { "" } else { " (still running)" }, text.trim().replace('\n', " | "))
        }
        Err(e) => format!("err {e}"),
    }
}

fn round(ws: &str) {
    eprintln!("ide-probe: ls /work -> {:?}", std::fs::read_dir("/work").map(|d| {
        let mut v: Vec<String> = d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v.join(",")
    }));
    eprintln!("ide-probe: build {ws} -> {}", run(ws, &["cc", "-o", "hello", "hello.c"], "app"));
    eprintln!("ide-probe: run {ws} -> {}", run(ws, &["./hello"], "app"));
    eprintln!("ide-probe: stdin {ws} -> {}", match exec(ws, &["cat"], "") {
        Ok((_pd, inp, out)) => {
            let mut f = std::fs::File::from(inp);
            let _ = f.write_all(b"through the pipe\n");
            drop(f);
            let mut text = String::new();
            let _ = std::fs::File::from(out).read_to_string(&mut text);
            format!("ok {}", text.trim())
        }
        Err(e) => format!("err {e}"),
    });
    eprintln!("ide-probe: escape {ws} -> {}", run(ws, &["ls"], "../.."));
    let mine = format!("/work/{ws}/app/from-ide.txt");
    eprintln!("ide-probe: write {mine} -> {:?}", std::fs::write(&mine, "edited in the IDE\n").map(|_| "ok"));
    eprintln!("ide-probe: cat-from-ws {ws} -> {}", run(ws, &["cat", "app/from-ide.txt"], ""));
    eprintln!("ide-probe: other -> {}", run("other", &["id", "-u"], ""));
    match exec(ws, &["sleep", "77"], "") {
        Ok((pd, _i, _o)) => {
            drop(pd);
            eprintln!("ide-probe: dropped sleep 77");
        }
        Err(e) => eprintln!("ide-probe: sleep -> err {e}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let targets: Vec<String> = std::fs::read_to_string("/targets").unwrap_or_default().lines().map(str::to_string).collect();
    let mut conn = Connection::connect(fresco_client::default_socket_path())?;
    let win = conn.window_create(300, 200, "ide-probe", WindowHints { initial_position: Some((950, 400)), ..Default::default() })?;
    let mut f = conn.frame()?;
    f.rect(RectParams { x: 0.0, y: 0.0, w: 300.0, h: 200.0, r: 0.3, g: 0.2, b: 0.5, a: 1.0, radius: 0.0 })?;
    f.finish()?;
    eprintln!("ide-probe: list -> {}", ask(Request::Workspaces));
    if let Some(t) = targets.first() {
        eprintln!("ide-probe: unpicked {t} -> {}", run(t, &["id", "-u"], ""));
        eprintln!("ide-probe: nopress {t} -> {}", ask(Request::WorkspacePick { name: t.clone(), activation: [0; 16] }));
    }
    let mut next = 0;
    loop {
        let Some(ev) = conn.wait_event(None)? else { continue };
        if let Event::PointerButton { window_id, pressed: true, activation, .. } = ev {
            if window_id != win {
                continue;
            }
            let Some(t) = targets.get(next) else { continue };
            let r = ask(Request::WorkspacePick { name: t.clone(), activation });
            eprintln!("ide-probe: pick {t} -> {r}");
            if r.contains("lost focus") {
                continue;
            }
            next += 1;
            if r == "ok" {
                round(t);
            }
        }
    }
}
