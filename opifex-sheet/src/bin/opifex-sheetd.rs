//! opifex-sheetd, the install sheet (docs/spec/opifex.md §7.1, build step C3).
//!
//! Runs jailed as `_opifexsheet` (65042), with no network and none of
//! anyone's files. For each asker (peer uid), one app at a time: asks
//! opifexd what the catalog says of it, shows the sheet (its own window on
//! frescod, the rows its guard), and on a counted press on *Install* asks
//! opifexd to install exactly the release it showed (`InstallApproved`,
//! which opifexd takes only from this uid). The asker hears `Installed`,
//! `Declined` or `Failed`.
//!
//! `opifex-sheetd ask <id>` asks a running one, as any caller would.
//!
//! Settings: `OPIFEX_SHEET_SOCKET` (where to listen), `OPIFEX_SOCKET`
//! (opifexd), `OPIFEX_SHEET_ACTIVATION` (frescod's activation socket).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opifex::proto::{Described, Request, Response};
use opifex_sheet::sheet::Outcome;
use opifex_sheet::{Answer, Ask, MAX_LINE};

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid on a connected socket we own, into two locals.
    (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a ucred we own, its size given.
    (unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut n) } == 0).then_some(c.uid)
}

/// One request to opifexd.
fn opifexd(r: &Request, wait: Duration) -> Result<Response, String> {
    let path = std::env::var_os("OPIFEX_SOCKET").map(PathBuf::from).unwrap_or_else(|| opifex::proto::SOCKET.into());
    let mut s = UnixStream::connect(&path).map_err(|e| format!("opifexd: {e}"))?;
    s.set_read_timeout(Some(wait)).ok();
    let mut line = serde_json::to_vec(r).map_err(|e| e.to_string())?;
    line.push(b'\n');
    s.write_all(&line).map_err(|e| format!("opifexd: {e}"))?;
    let mut back = String::new();
    BufReader::new(s).read_line(&mut back).map_err(|e| format!("opifexd: {e}"))?;
    serde_json::from_str(back.trim_end()).map_err(|e| format!("opifexd: {e}"))
}

/// A catalog id: the only thing an asker sends, checked before anything.
fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.contains('.')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// What happens to one ask, with `show` putting the sheet in front of the
/// person (a parameter so tests can stand in for them).
fn decide(asker: u32, id: &str, show: impl FnOnce(Described) -> Result<Outcome, String>) -> Answer {
    if !plain_id(id) {
        return Answer::Failed { why: "not an app id".into() };
    }
    let app = match opifexd(&Request::Describe { id: id.into() }, Duration::from_secs(10)) {
        Ok(Response::Described { app }) => app,
        Ok(Response::Error { message }) => return Answer::Failed { why: message },
        Ok(other) => return Answer::Failed { why: format!("opifexd: {other:?}") },
        Err(why) => return Answer::Failed { why },
    };
    if app.kind != "app" {
        return Answer::Failed { why: format!("{id} is a {}, not an app", app.kind) };
    }
    if app.installed.as_deref() == Some(app.latest.as_str()) {
        return Answer::Installed;
    }
    let manifest_sha256 = app.manifest_sha256.clone();
    match show(app) {
        Ok(Outcome::NotNow) => Answer::Declined,
        Err(why) => Answer::Failed { why },
        Ok(Outcome::Install) => {
            let r = Request::InstallApproved { id: id.into(), manifest_sha256, for_uid: asker };
            match opifexd(&r, Duration::from_secs(300)) {
                Ok(Response::Done { .. }) => Answer::Installed,
                Ok(Response::Error { message }) => Answer::Failed { why: message },
                Ok(other) => Answer::Failed { why: format!("opifexd: {other:?}") },
                Err(why) => Answer::Failed { why },
            }
        }
    }
}

const ACTIVATION: &str = "/atrium/sockets/fresco-activation/fresco-activation.sock";
const SHEET_FOR: Duration = Duration::from_secs(110);
/// After a Not now, the same asker waits this long before asking again.
const QUIET_AFTER_DECLINE: Duration = Duration::from_secs(30);

/// Spend `token`, a press on Install, as `PROMPT`: frescod counts it only
/// if the guard (the rows) was seen whole, uncovered and still before it.
fn counted(token: [u8; 16]) -> bool {
    use fresco_protocol::activation::{op, purpose, Consume, ConsumeResult};
    if token == fresco_protocol::activation::NONE {
        return false;
    }
    let path = std::env::var("OPIFEX_SHEET_ACTIVATION").unwrap_or_else(|_| ACTIVATION.into());
    let ask = || -> std::io::Result<ConsumeResult> {
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let mut c = aqueduct::Connection::connect(&path)?;
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        let payload = postcard::to_stdvec(&Consume { token, presenter_uid: me, purpose: purpose::PROMPT }).map_err(|e| bad(e.to_string()))?;
        c.send_message(aqueduct::classes::CLASS_DISPLAY, op::CONSUME, aqueduct::flag::RESPONSE_EXPECTED, &payload)?;
        c.set_read_timeout(Some(Duration::from_millis(500)))?;
        let m = c.recv_message()?;
        postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()))
    };
    match ask() {
        Ok(ConsumeResult::Ok { .. }) => true,
        Ok(r) => {
            eprintln!("opifex-sheetd: a press on Install not counted: {r:?}");
            false
        }
        Err(e) => {
            eprintln!("opifex-sheetd: frescod's activation socket: {e}");
            false
        }
    }
}

/// One sheet on screen at a time, whoever asks.
static SHEET: Mutex<()> = Mutex::new(());

/// The sheet in front of the person: opifex-sheetd's own window on frescod.
fn show(app: Described) -> Result<Outcome, String> {
    use fresco_client::{Connection, Event};
    use fresco_protocol::{WindowHints, WmRole};
    use opifex_sheet::sheet::{Do, Key, Sheet};
    use opifex_sheet::view;
    let _one = SHEET.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = Sheet::new(app);
    // Room for every line and the notice, fixed now so the guard never moves.
    let lines = s.lines().len() + 1;
    let fresco = fresco_client::default_socket_path();
    let mut win = Connection::connect(&fresco).map_err(|e| format!("frescod ({}): {e}", fresco.display()))?;
    let text = win.appearance().map(|a| a.text_scale()).unwrap_or(1.0);
    let (w, h) = view::size(lines, text);
    let hints = WindowHints { role: Some(WmRole::Dialog), server_decorations: Some(false), min_size: Some((w.ceil() as u32, h.ceil() as u32)), ..Default::default() };
    let id = win.window_create(w.ceil() as u32, h.ceil() as u32, "Install", hints).map_err(|e| format!("frescod: {e}"))?;
    win.set_default_window(id as u16);
    let font = view::Font::open(&mut win);
    let (mut size, mut k) = ((w, h), win.window_scale(id));
    let guard = |win: &mut Connection, size: (f32, f32), k: f32| {
        let g = view::guard(lines, size.0, text);
        let r = [(g.x * k).ceil() as i32, (g.y * k).ceil() as i32, (g.w * k).floor() as i32, (g.h * k).floor() as i32];
        if let Err(e) = win.window_guard(id, Some(r)) {
            eprintln!("opifex-sheetd: frescod: guard: {e}");
        }
    };
    guard(&mut win, size, k);
    let at = |r: view::Rect| (((r.x + 24.0) * k) as i32, ((r.y + r.h / 2.0) * k) as i32);
    let rs = view::rows(lines, size.0, text);
    let (a, b) = (at(rs[0]), at(rs[1]));
    eprintln!("opifex-sheetd: asking about {} {} (install={},{} notnow={},{})", s.app.id, s.app.latest, a.0, a.1, b.0, b.1);
    let until = Instant::now() + SHEET_FOR;
    let mut dirty = true;
    let out = 'run: loop {
        if Instant::now() > until {
            eprintln!("opifex-sheetd: the sheet timed out");
            break Outcome::NotNow;
        }
        loop {
            let ev = match win.poll_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("opifex-sheetd: frescod: {e}");
                    break 'run Outcome::NotNow;
                }
            };
            let d = match ev {
                Event::Resized { width, height, scale, .. } => {
                    size = (width, height);
                    if scale > 0.0 {
                        k = scale;
                    }
                    guard(&mut win, size, k);
                    Do::Redraw
                }
                Event::Key { hid_usage, pressed: true, repeat: false, activation, .. } => s.key(Key::from_hid(hid_usage), activation),
                Event::PointerButton { x, y, pressed: true, activation, .. } => match view::hit(lines, size.0, text, x, y) {
                    Some(i) => s.press(i, activation),
                    None => Do::Nothing,
                },
                Event::CloseRequested { .. } => Do::Done(Outcome::NotNow),
                _ => Do::Nothing,
            };
            let d = match d {
                Do::Spend { token } => s.spent(counted(token)),
                d => d,
            };
            match d {
                Do::Done(o) => break 'run o,
                Do::Redraw => dirty = true,
                _ => {}
            }
        }
        if dirty {
            dirty = false;
            if let Err(e) = view::draw(&mut win, font, &s, lines, size.0, size.1, k, text) {
                eprintln!("opifex-sheetd: draw: {e}");
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = win.window_destroy(id);
    Ok(out)
}

#[derive(Default)]
struct Askers {
    busy: HashSet<u32>,
    declined: HashMap<u32, Instant>,
}

fn serve(s: UnixStream, askers: Arc<Mutex<Askers>>) {
    let Some(uid) = peer_uid(&s) else { return };
    let answer = |mut s: &UnixStream, a: Answer| {
        let mut line = serde_json::to_vec(&a).unwrap_or_default();
        line.push(b'\n');
        let _ = s.write_all(&line);
    };
    {
        let mut a = askers.lock().unwrap();
        if a.declined.get(&uid).is_some_and(|t| t.elapsed() < QUIET_AFTER_DECLINE) {
            return answer(&s, Answer::Declined);
        }
        if !a.busy.insert(uid) {
            return answer(&s, Answer::Failed { why: "one install at a time".into() });
        }
    }
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let read = BufReader::new((&s).take(MAX_LINE as u64)).read_line(&mut line);
    let a = match read.ok().and_then(|_| serde_json::from_str::<Ask>(line.trim_end()).ok()) {
        Some(Ask::Install { id }) => {
            let a = decide(uid, &id, show);
            eprintln!("opifex-sheetd: {id} for uid {uid}: {a:?}");
            a
        }
        None => Answer::Failed { why: "not a request".into() },
    };
    {
        let mut st = askers.lock().unwrap();
        if a == Answer::Declined {
            st.declined.insert(uid, Instant::now());
        }
        st.busy.remove(&uid);
    }
    answer(&s, a);
}

fn main() {
    // `opifex-sheetd ask <id>`: a caller, for tests and scripts.
    let args: Vec<String> = std::env::args().collect();
    if let [_, ask, id] = &args[..] {
        if ask == "ask" {
            match opifex_sheet::install(id) {
                Ok(a) => println!("{a:?}"),
                Err(e) => {
                    eprintln!("opifex-sheetd: {e}");
                    std::process::exit(1)
                }
            }
            return;
        }
    }
    let path = opifex_sheet::socket();
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).unwrap_or_else(|e| {
        eprintln!("opifex-sheetd: {}: {e}", path.display());
        std::process::exit(1)
    });
    // Anyone may ask; the person decides.
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)) {
        eprintln!("opifex-sheetd: {}: {e}", path.display());
        std::process::exit(1);
    }
    eprintln!("opifex-sheetd: listening on {} (uid {})", path.display(), unsafe { libc::getuid() });
    let askers = Arc::new(Mutex::new(Askers::default()));
    for s in l.incoming().flatten() {
        let askers = askers.clone();
        let _ = std::thread::Builder::new().name("ask".into()).spawn(move || serve(s, askers));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in opifexd: answers each request in turn with `answers`, and
    /// gives back what it was asked.
    fn fake(tag: &str, answers: Vec<Response>) -> (PathBuf, std::thread::JoinHandle<Vec<Request>>) {
        let d = std::env::temp_dir().join(format!("opifex-sheet-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("opifex.sock");
        let l = UnixListener::bind(&p).unwrap();
        let h = std::thread::spawn(move || {
            let mut got = Vec::new();
            for a in answers {
                let (s, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&s).read_line(&mut line).unwrap();
                got.push(serde_json::from_str(line.trim_end()).unwrap());
                let mut out = serde_json::to_vec(&a).unwrap();
                out.push(b'\n');
                (&s).write_all(&out).unwrap();
            }
            got
        });
        (p, h)
    }

    fn app(installed: Option<&str>) -> Described {
        Described {
            id: "org.atrium.hello".into(), kind: "app".into(), name: "Hello".into(), description: String::new(),
            latest: "1.1".into(), publisher: "k".into(), catalog: "Atrium".into(), capabilities: vec![],
            archive_size: 1, installed: installed.map(Into::into), manifest_sha256: "ab".repeat(32), added: vec![],
        }
    }

    // One test: OPIFEX_SOCKET is process-wide.
    #[test]
    fn the_person_decides_and_opifexd_installs_what_was_shown() {
        let (p, h) = fake("yes", vec![
            Response::Described { app: app(None) },
            Response::Done { id: "org.atrium.hello".into(), version: "1.1".into() },
        ]);
        std::env::set_var("OPIFEX_SOCKET", &p);
        let mut shown = None;
        assert_eq!(decide(1001, "org.atrium.hello", |a| { shown = Some(a.latest.clone()); Ok(Outcome::Install) }), Answer::Installed);
        assert_eq!(shown.as_deref(), Some("1.1"));
        let got = h.join().unwrap();
        assert_eq!(got[1], Request::InstallApproved { id: "org.atrium.hello".into(), manifest_sha256: "ab".repeat(32), for_uid: 1001 });

        // Not now: opifexd is asked nothing more.
        let (p, h) = fake("no", vec![Response::Described { app: app(None) }]);
        std::env::set_var("OPIFEX_SOCKET", &p);
        assert_eq!(decide(1001, "org.atrium.hello", |_| Ok(Outcome::NotNow)), Answer::Declined);
        assert_eq!(h.join().unwrap().len(), 1);

        // Already at the newest: no sheet.
        let (p, h) = fake("have", vec![Response::Described { app: app(Some("1.1")) }]);
        std::env::set_var("OPIFEX_SOCKET", &p);
        assert_eq!(decide(1001, "org.atrium.hello", |_| panic!("no sheet")), Answer::Installed);
        h.join().unwrap();

        // Not an app (a font, say): no sheet.
        let (p, h) = fake("font", vec![Response::Described { app: Described { kind: "font".into(), ..app(None) } }]);
        std::env::set_var("OPIFEX_SOCKET", &p);
        assert!(matches!(decide(1001, "org.atrium.hello", |_| panic!("no sheet")), Answer::Failed { why } if why.contains("not an app")));
        h.join().unwrap();

        // Junk ids never reach opifexd.
        std::env::set_var("OPIFEX_SOCKET", "/nonexistent/opifex.sock");
        for id in ["", "../etc", "a b.c", "noddot"] {
            assert_eq!(decide(1001, id, |_| panic!("no sheet")), Answer::Failed { why: "not an app id".into() });
        }
    }
}
