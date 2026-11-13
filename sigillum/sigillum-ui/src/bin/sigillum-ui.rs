//! sigillum-ui: Sigillum's trusted sheets (docs/spec/sigillum.md §7.2).
//!
//! Runs as `_sigillum-ui` (65030). It connects to `sigillumd`'s UI socket
//! (which serves only this uid), opens one dialog window per sheet on
//! frescod, and answers with presses, PINs and passcodes. Every choice but
//! Cancel is first spent with frescod as an `IDENTITY` press in the sheet's
//! own window; one frescod refuses never reaches `sigillumd`.
//!
//! Environment (defaults in brackets):
//!   SIGILLUM_UI_SOCKET    [/atrium/sockets/sigillum-ui/ui.sock]
//!   FRESCO_SOCKET         [fresco-client's default]
//!   FRESCO_ACTIVATION     [/var/run/atrium/fresco-activation.sock]

use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use fresco_client::{Connection, Event};
use fresco_protocol::{WindowHints, WmRole};
use sigillum_proto::ui::{self, FromUi, ToUi};
use sigillum_proto::{read_msg, write_msg};
use sigillum_ui::gate::{self, Frescod};
use sigillum_ui::sheet::{Do, State};
use sigillum_ui::{keymap, view};

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn harden() -> std::io::Result<()> {
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscalls on this process with valid arguments.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut arg: libc::c_int = libc::PROC_TRACE_CTL_DISABLE;
        if unsafe { libc::procctl(libc::P_PID, 0, libc::PROC_TRACE_CTL, &mut arg as *mut _ as *mut libc::c_void) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

struct Open {
    win: Connection,
    window_id: u32,
    font: u32,
    state: State,
    size: (f32, f32),
    dirty: bool,
}

struct Ctx {
    out: UnixStream,
    gate: Frescod,
    me: u32,
}

impl Ctx {
    fn send(&mut self, m: &FromUi) -> bool {
        write_msg(&mut self.out, m, ui::MAX_MESSAGE).is_ok()
    }

    /// Carry out a sheet's `Do`s; false if sigillumd is gone.
    fn run(&mut self, o: &mut Open, dos: Vec<Do>) -> bool {
        let mut queue: std::collections::VecDeque<Do> = dos.into();
        while let Some(d) = queue.pop_front() {
            match d {
                Do::Redraw => o.dirty = true,
                Do::Send(m) => {
                    if !self.send(&m) {
                        return false;
                    }
                }
                Do::Spend { choice, token } => {
                    let ok = self.gate.identity(token, self.me, o.window_id);
                    queue.extend(o.state.spent(choice, ok));
                }
            }
        }
        true
    }
}

fn open(fresco: &str, msg: ToUi) -> std::io::Result<Open> {
    let ToUi::Show { key, sheet, devices } = msg else { unreachable!() };
    let mut win = Connection::connect(fresco)?;
    let hints = WindowHints { role: Some(WmRole::Dialog), server_decorations: Some(false), ..Default::default() };
    let window_id = win.window_create(view::W as u32, view::H as u32, "Sigillum", hints)?;
    win.set_default_window(window_id as u16);
    let font = win.font_open("system-sans").map(|f| f.font_id).unwrap_or(0);
    Ok(Open { win, window_id, font, state: State::new(key, sheet, devices), size: (view::W, view::H), dirty: true })
}

fn main() -> ExitCode {
    if let Err(e) = harden() {
        eprintln!("sigillum-ui: harden: {e}");
        return ExitCode::FAILURE;
    }
    let fresco = std::env::var("FRESCO_SOCKET")
        .unwrap_or_else(|_| fresco_client::default_socket_path().to_string_lossy().into_owned());
    let sock = env("SIGILLUM_UI_SOCKET", ui::SOCKET);
    let stream = loop {
        match UnixStream::connect(&sock) {
            Ok(s) => break s,
            Err(e) => {
                eprintln!("sigillum-ui: {sock}: {e}; retrying in 2 s");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    let (tx, rx) = mpsc::channel::<ToUi>();
    let mut input = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sigillum-ui: {e}");
            return ExitCode::FAILURE;
        }
    };
    std::thread::spawn(move || {
        while let Ok(Some(m)) = read_msg::<ToUi>(&mut input, ui::MAX_MESSAGE) {
            if tx.send(m).is_err() {
                return;
            }
        }
    });
    let mut cx = Ctx {
        out: stream,
        gate: Frescod::new(env("FRESCO_ACTIVATION", gate::DEFAULT_SOCKET)),
        // SAFETY: getuid cannot fail.
        me: unsafe { libc::getuid() },
    };
    eprintln!("sigillum-ui: serving sheets for sigillumd");
    let mut sheet: Option<Open> = None;

    loop {
        let wait = if sheet.is_some() { 10 } else { 500 };
        match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(m @ ToUi::Show { .. }) => {
                if let Some(old) = sheet.take() {
                    let mut old = old;
                    let _ = old.win.window_destroy(old.window_id);
                }
                let key = if let ToUi::Show { key, .. } = &m { *key } else { 0 };
                match open(&fresco, m) {
                    Ok(o) => sheet = Some(o),
                    Err(e) => {
                        // No sheet, no ceremony: tell sigillumd at once.
                        eprintln!("sigillum-ui: window: {e}");
                        cx.send(&FromUi::Press { key, choice: sigillum_proto::ui::Choice::Cancel, activation_ok: true });
                    }
                }
            }
            Ok(ToUi::Close { key }) => {
                if sheet.as_ref().is_some_and(|o| o.state.key == key) {
                    let mut o = sheet.take().unwrap();
                    let _ = o.win.window_destroy(o.window_id);
                }
            }
            Ok(m) => {
                if let Some(o) = sheet.as_mut() {
                    let key = match &m {
                        ToUi::AskPin { key, .. } | ToUi::Touch { key, .. } | ToUi::AskPasscode { key } => *key,
                        _ => 0,
                    };
                    if o.state.key == key {
                        let dos = o.state.from_broker(&m);
                        if !cx.run(o, dos) {
                            return ExitCode::FAILURE;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("sigillum-ui: sigillumd went away");
                return ExitCode::FAILURE;
            }
        }

        let Some(o) = sheet.as_mut() else { continue };
        let mut gone = false;
        loop {
            let ev = match o.win.poll_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(_) => {
                    gone = true;
                    break;
                }
            };
            let dos = match ev {
                Event::Resized { width, height, .. } => {
                    o.size = (width as f32, height as f32);
                    vec![Do::Redraw]
                }
                Event::Key { hid_usage, pressed: true, repeat, modifiers, activation, .. } => {
                    let k = keymap::decode(hid_usage, modifiers);
                    // A held Enter must not press twice.
                    if repeat && k == keymap::Key::Enter {
                        Vec::new()
                    } else {
                        o.state.key(k, activation)
                    }
                }
                Event::PointerButton { x, y, pressed: true, activation, .. } => match view::hit(&o.state, o.size.0, o.size.1, x, y) {
                    Some(i) => o.state.click(i, activation),
                    None => Vec::new(),
                },
                Event::CloseRequested { .. } => o.state.key(keymap::Key::Escape, [0; 16]),
                _ => Vec::new(),
            };
            if !cx.run(o, dos) {
                return ExitCode::FAILURE;
            }
        }
        if gone {
            // frescod went away: the sheet can't be seen, so cancel.
            let key = o.state.key;
            sheet = None;
            cx.send(&FromUi::Press { key, choice: sigillum_proto::ui::Choice::Cancel, activation_ok: true });
            continue;
        }
        if o.dirty {
            o.dirty = false;
            if let Err(e) = view::draw(&mut o.win, o.font, &o.state, o.size.0, o.size.1) {
                eprintln!("sigillum-ui: draw: {e}");
            }
        }
    }
}
