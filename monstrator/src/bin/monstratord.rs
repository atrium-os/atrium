//! monstratord, the link opener (docs/spec/opening-links.md §5–6).
//!
//! Runs jailed as `_monstrator`, with no network and none of the person's
//! files. For each app asking (peer uid), one link at a time: checks it
//! (atrium-links), asks portcullisd for a grant with the asker's press
//! (portcullisd spends it), picks the app (the default, or the only one;
//! the sheet for a choice is L3), and has portcullisd hand it over. Logs
//! name the scheme, never the link.
//!
//! Settings: `MONSTRATOR_SOCKET` (where to listen), `PORTCULLIS_SOCKET`.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use monstrator::sheet::Outcome;
use monstrator::{Answer, Ask, MAX_LINE};
use portcullis_ipc::{round_trip, Request, Response};

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid on a connected socket we own, into two locals.
    (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

/// (Host builds: the same answer from SO_PEERCRED.)
#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a ucred we own, its size given.
    (unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut n) } == 0).then_some(c.uid)
}

fn portcullis_socket() -> PathBuf {
    std::env::var_os("PORTCULLIS_SOCKET").map(PathBuf::from)
        .or_else(|| portcullis_ipc::resolve_socket_path().map(PathBuf::from))
        .unwrap_or_else(|| portcullis_ipc::SERVICE_SOCKET_PATH.into())
}

/// One link from `asker`, start to end.
fn open(asker: u32, url: &str, activation: [u8; 16], choose: bool) -> Result<String, String> {
    let link = atrium_links::check(url)?;
    let mut p = UnixStream::connect(portcullis_socket()).map_err(|e| format!("portcullisd: {e}"))?;
    p.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let ask = |p: &mut UnixStream, r: &Request| round_trip(p, r).map_err(|e| format!("portcullisd: {e}"));
    match ask(&mut p, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
        Response::Hello { .. } => {}
        other => return Err(format!("portcullisd: {other:?}")),
    }
    let (grant, apps, personas, install) = match ask(&mut p, &Request::LinkGrant { asker_uid: asker, activation, scheme: link.scheme.clone(), app: link.app.clone() })? {
        Response::LinkGranted { grant, apps, personas, install, .. } => (grant, apps, personas, install),
        Response::Error { message } => return Err(message),
        other => return Err(format!("portcullisd: {other:?}")),
    };
    // An atrium-app: link to an app that isn't here (§4.3, L5): Opifex's
    // install sheet asks the person, for the app that asked; then it opens.
    if install {
        let id = apps.first().map(|a| a.id.clone()).ok_or("no app named")?;
        self::install(asker, &id)?;
        return match ask(&mut p, &Request::DeliverLink { grant, app: id, url: link.url, always: false, persona: None })? {
            Response::LinkDelivered { .. } => Ok(link.scheme),
            Response::Error { message } => Err(message),
            other => Err(format!("portcullisd: {other:?}")),
        };
    }
    // Without "Open with…", the default or the only app; with it, or with
    // a choice, the person makes it in Monstrator's own sheet (§5), where
    // another persona may be chosen too (personas.md §6).
    let (app, always, persona) = match monstrator::pick(&apps).filter(|_| !choose) {
        Some(a) => (a.to_string(), false, None),
        None => match self::choose(&link.shown, &link.scheme, apps, personas)? {
            Outcome::Chose { app, always, persona } => (app, always, persona),
            Outcome::Copy { token } => {
                copy_address(asker, token, &link.url)?;
                return Err("address copied".into());
            }
            Outcome::Cancelled => return Err("cancelled".into()),
        },
    };
    match ask(&mut p, &Request::DeliverLink { grant, app: app.clone(), url: link.url, always, persona })? {
        Response::LinkDelivered { .. } => Ok(link.scheme),
        Response::Error { message } => Err(message),
        other => Err(format!("portcullisd: {other:?}")),
    }
}

/// Opifex's install sheet (opifex.md §7.1), for an `atrium-app:` link to
/// an app that isn't installed.
const INSTALL_SHEET: &str = "/atrium/sockets/opifex-sheet/opifex-sheet.sock";

/// The install sheet's wire (opifex_sheet::Ask and Answer, as JSON lines).
#[derive(serde::Serialize)]
enum SheetAsk<'a> {
    Install { id: &'a str, for_uid: Option<u32> },
}

#[derive(serde::Deserialize, Debug)]
enum SheetAnswer {
    Installed,
    Declined,
    Failed { why: String },
}

/// Ask the install sheet to install `id` for `asker`, the app whose link
/// it was (the sheet takes `for_uid` from `_monstrator` only). Waits for
/// the person, up to the sheet's two minutes.
fn install(asker: u32, id: &str) -> Result<(), String> {
    let path = std::env::var("MONSTRATOR_INSTALL_SHEET").unwrap_or_else(|_| INSTALL_SHEET.into());
    let mut s = UnixStream::connect(&path).map_err(|e| format!("not installed, and the install sheet: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(150))).ok();
    let mut line = serde_json::to_vec(&SheetAsk::Install { id, for_uid: Some(asker) }).map_err(|e| e.to_string())?;
    line.push(b'\n');
    s.write_all(&line).map_err(|e| format!("the install sheet: {e}"))?;
    let mut back = String::new();
    BufReader::new(&s).read_line(&mut back).map_err(|e| format!("the install sheet: {e}"))?;
    match serde_json::from_str::<SheetAnswer>(back.trim_end()) {
        Ok(SheetAnswer::Installed) => {
            eprintln!("monstratord: {id} installed for uid {asker}, through the install sheet");
            Ok(())
        }
        Ok(SheetAnswer::Declined) => Err(format!("{id} isn't installed: not now")),
        Ok(SheetAnswer::Failed { why }) => Err(format!("{id} isn't installed: {why}")),
        Err(e) => Err(format!("the install sheet: {e}")),
    }
}

/// Tabula, the clipboard (tabula.md), where Copy address goes.
const TABULA: &str = "/atrium/sockets/clipboard/clipboard.sock";

/// Copy address (personas.md §6): `url` on the clipboard for `asker`, so
/// the entry is in the asker's persona and pastes only there (unless the
/// person's switch for clipboard crossings is on). `token` is the press
/// in the sheet; Tabula spends it with frescod as a copy.
fn copy_address(asker: u32, token: [u8; 16], url: &str) -> Result<(), String> {
    use tabula::proto::{op, Format, Reply, Set, CLASS};
    let path = std::env::var("MONSTRATOR_TABULA").unwrap_or_else(|_| TABULA.into());
    let go = || -> Result<Reply, String> {
        let mut c = aqueduct::Connection::connect(&path).map_err(|e| format!("the clipboard: {e}"))?;
        let hash = c.upload_blob(url.as_bytes()).map_err(|e| e.to_string())?;
        let set = Set { token, formats: vec![Format { mime: "text/plain".into(), hash }], sensitive: false, for_uid: Some(asker) };
        let payload = postcard::to_stdvec(&set).map_err(|e| e.to_string())?;
        c.send_message(CLASS, op::SET, aqueduct::flag::RESPONSE_EXPECTED, &payload).map_err(|e| e.to_string())?;
        loop {
            let m = c.recv_message_or_timeout(Duration::from_secs(2)).map_err(|e| e.to_string())?
                .ok_or("the clipboard did not answer")?;
            if m.flags & aqueduct::flag::IS_RESPONSE != 0 && m.op == op::SET {
                return postcard::from_bytes(&m.payload).map_err(|e| e.to_string());
            }
        }
    };
    match go()? {
        Reply::Ok => {
            eprintln!("monstratord: the address copied for uid {asker}");
            Ok(())
        }
        r => Err(format!("the clipboard refused: {r:?}")),
    }
}

/// frescod's activation socket, where the sheet's presses are spent.
const ACTIVATION: &str = "/atrium/sockets/fresco-activation/fresco-activation.sock";
/// How long the sheet waits: under the grant's two minutes.
const SHEET_FOR: Duration = Duration::from_secs(110);

/// Spend `token`, a press in the sheet, as `PROMPT`: frescod
/// counts it only if the window's guard (the rows) was seen whole,
/// uncovered and still before it.
fn counted(token: [u8; 16]) -> bool {
    use fresco_protocol::activation::{op, purpose, Consume, ConsumeResult};
    if token == fresco_protocol::activation::NONE {
        return false;
    }
    let path = std::env::var("MONSTRATOR_ACTIVATION").unwrap_or_else(|_| ACTIVATION.into());
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
        // frescod counted it for our uid, in a window whose guard it saw
        // whole: the sheet is monstratord's only window. (Window ids don't
        // compare: with renderers split off, events carry the renderer's.)
        Ok(ConsumeResult::Ok { .. }) => true,
        Ok(r) => {
            eprintln!("monstratord: a press in the sheet not counted: {r:?}");
            false
        }
        Err(e) => {
            eprintln!("monstratord: frescod's activation socket: {e}");
            false
        }
    }
}

/// One sheet at a time, whoever asks.
static SHEET: Mutex<()> = Mutex::new(());

/// Ask the person which of `apps` opens the link (§5): Monstrator's own
/// window on frescod, its rows the window's guard.
fn choose(shown: &str, scheme: &str, apps: Vec<portcullis_ipc::LinkChoice>, personas: Vec<portcullis_ipc::PersonaChoice>) -> Result<Outcome, String> {
    use fresco_client::{Connection, Event};
    use fresco_protocol::{WindowHints, WmRole};
    use monstrator::sheet::{Do, Key, Sheet};
    use monstrator::view;
    let _one = SHEET.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = Sheet::with_personas(shown, scheme, apps, personas);
    let n = s.rows().len();
    let fresco = fresco_client::default_socket_path();
    let mut win = Connection::connect(&fresco).map_err(|e| format!("frescod ({}): {e}", fresco.display()))?;
    let text = win.appearance().map(|a| a.text_scale()).unwrap_or(1.0);
    let (w, h) = view::size(n, text);
    let hints = |w: f32, h: f32| WindowHints { role: Some(WmRole::Dialog), server_decorations: Some(false), min_size: Some((w.ceil() as u32, h.ceil() as u32)), ..Default::default() };
    let id = win.window_create(w.ceil() as u32, h.ceil() as u32, "Open link", hints(w, h)).map_err(|e| format!("frescod: {e}"))?;
    win.set_default_window(id as u16);
    let font = view::Font::open(&mut win);
    let (mut size, mut k) = ((w, h), win.window_scale(id));
    let guard = |win: &mut Connection, size: (f32, f32), k: f32| {
        let g = view::guard(n, size.0, text);
        let r = [(g.x * k).ceil() as i32, (g.y * k).ceil() as i32, (g.w * k).floor() as i32, (g.h * k).floor() as i32];
        if let Err(e) = win.window_guard(id, Some(r)) {
            eprintln!("monstratord: frescod: guard: {e}");
        }
    };
    guard(&mut win, size, k);
    eprintln!("monstratord: asking which of {} apps opens a {scheme} link{}", s.apps.len(),
              if s.personas.is_empty() { String::new() } else { format!(", in which of {} personas", s.personas.len()) });
    let until = std::time::Instant::now() + SHEET_FOR;
    let mut dirty = true;
    let out = 'run: loop {
        if std::time::Instant::now() > until {
            eprintln!("monstratord: the sheet timed out");
            break Outcome::Cancelled;
        }
        loop {
            let ev = match win.poll_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("monstratord: frescod: {e}");
                    break 'run Outcome::Cancelled;
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
                Event::Key { hid_usage, pressed: true, repeat: false, activation, .. } => {
                    let d = s.key(Key::from_hid(hid_usage), activation);
                    eprintln!("monstratord: key {hid_usage:#x}: focus {}", s.focus);
                    d
                }
                Event::PointerButton { x, y, pressed: true, activation, .. } => match view::hit(n, size.0, text, x, y) {
                    Some(i) => {
                            s.press(i, activation)
                    }
                    None => Do::Nothing,
                },
                Event::CloseRequested { .. } => {
                    eprintln!("monstratord: the sheet was closed");
                    Do::Done(Outcome::Cancelled)
                }
                _ => Do::Nothing,
            };
            let d = match d {
                Do::Spend { row, token } => s.spent(row, counted(token)),
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
            if let Err(e) = view::draw(&mut win, font, &s, size.0, size.1, k, text) {
                eprintln!("monstratord: draw: {e}");
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = win.window_destroy(id);
    Ok(out)
}

fn serve(s: UnixStream, busy: Arc<Mutex<HashSet<u32>>>) {
    let Some(uid) = peer_uid(&s) else { return };
    let answer = |mut s: &UnixStream, a: Answer| {
        let mut line = serde_json::to_vec(&a).unwrap_or_default();
        line.push(b'\n');
        let _ = s.write_all(&line);
    };
    // One link at a time per asker (§5).
    if !busy.lock().unwrap().insert(uid) {
        return answer(&s, Answer::NotOpened { why: "one link at a time".into() });
    }
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let read = BufReader::new((&s).take(MAX_LINE as u64)).read_line(&mut line);
    let result = match read.ok().and_then(|_| serde_json::from_str::<Ask>(line.trim_end()).ok()) {
        Some(Ask::OpenLink { url, activation, choose }) => open(uid, &url, activation, choose),
        None => Err("not a request".into()),
    };
    match result {
        Ok(scheme) => {
            eprintln!("monstratord: a {scheme} link from uid {uid}: opened");
            answer(&s, Answer::Opened);
        }
        Err(why) => {
            eprintln!("monstratord: a link from uid {uid}: not opened: {why}");
            answer(&s, Answer::NotOpened { why });
        }
    }
    busy.lock().unwrap().remove(&uid);
}

fn main() {
    let path = monstrator::socket();
    let _ = std::fs::remove_file(&path);
    let l = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("monstratord: {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    // Anyone may ask; each asker is its peer uid, and portcullisd decides.
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)) {
        eprintln!("monstratord: {}: {e}", path.display());
        std::process::exit(1);
    }
    eprintln!("monstratord: listening on {} (uid {})", path.display(), unsafe { libc::getuid() });
    let busy = Arc::new(Mutex::new(HashSet::new()));
    for s in l.incoming().flatten() {
        let busy = busy.clone();
        let _ = std::thread::Builder::new().name("link".into()).spawn(move || serve(s, busy));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_ipc::{read_request, write_response, LinkChoice};

    /// A stand-in portcullisd: answers Hello, then `answers` in turn, and
    /// gives back what it was asked.
    fn fake(answers: Vec<Response>) -> (PathBuf, std::thread::JoinHandle<Vec<Request>>) {
        let d = std::env::temp_dir().join(format!("monstrator-{}-{}", std::process::id(), answers.len()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("portcullis.sock");
        let l = UnixListener::bind(&p).unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut got = Vec::new();
            for a in std::iter::once(Response::Hello { version: portcullis_ipc::PROTO_VERSION }).chain(answers) {
                match read_request(&mut r) {
                    Ok(q) => got.push(q),
                    Err(_) => break,
                }
                write_response(&mut w, &a).unwrap();
            }
            got
        });
        (p, h)
    }

    #[test]
    fn a_link_goes_through_portcullisd_once() {
        let (p, h) = fake(vec![
            Response::LinkGranted { grant: [9; 16], person: "ana".into(), apps: vec![LinkChoice { id: "org.example.mail".into(), name: "Mail".into(), default: false }], personas: vec![], install: false },
            Response::LinkDelivered { app: "Mail".into() },
        ]);
        std::env::set_var("PORTCULLIS_SOCKET", &p);
        assert_eq!(open(50001, "MAILTO:ana@example.org", [1; 16], false).unwrap(), "mailto");
        let got = h.join().unwrap();
        assert!(matches!(&got[1], Request::LinkGrant { asker_uid: 50001, activation, scheme, app: None } if *activation == [1; 16] && scheme == "mailto"));
        assert!(matches!(&got[2], Request::DeliverLink { grant, app, url, always: false, persona: None } if *grant == [9; 16] && app == "org.example.mail" && url == "mailto:ana@example.org"));
        // Refused before portcullisd is asked anything.
        std::env::set_var("PORTCULLIS_SOCKET", "/nonexistent/portcullis.sock");
        assert!(open(50001, "javascript:alert(1)", [1; 16], false).unwrap_err().contains("never opened"));
        assert!(open(50001, "mailto:a@b\nbcc:c@d", [1; 16], false).unwrap_err().contains("control"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        // L5: an atrium-app: link to an app that isn't here. The install
        // sheet is asked for the asking app's uid; installed, it opens;
        // declined, nothing is delivered.
        for (answer, opened) in [("\"Installed\"", true), ("\"Declined\"", false)] {
            let not_here = || Response::LinkGranted { grant: [7; 16], person: "ana".into(), apps: vec![LinkChoice { id: "org.atrium.hello".into(), name: "org.atrium.hello".into(), default: false }], personas: vec![], install: true };
            let (p, h) = fake(if opened { vec![not_here(), Response::LinkDelivered { app: "Hello".into() }] } else { vec![not_here()] });
            std::env::set_var("PORTCULLIS_SOCKET", &p);
            let sheet = p.parent().unwrap().join("sheet.sock");
            let l = UnixListener::bind(&sheet).unwrap();
            let a = answer.to_string();
            let sh = std::thread::spawn(move || {
                let (s, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&s).read_line(&mut line).unwrap();
                (&s).write_all(format!("{a}\n").as_bytes()).unwrap();
                line
            });
            std::env::set_var("MONSTRATOR_INSTALL_SHEET", &sheet);
            let r = open(50001, "atrium-app://org.atrium.hello", [2; 16], false);
            assert_eq!(r.is_ok(), opened, "{r:?}");
            let asked: serde_json::Value = serde_json::from_str(sh.join().unwrap().trim()).unwrap();
            assert_eq!(asked["Install"]["id"], "org.atrium.hello");
            assert_eq!(asked["Install"]["for_uid"], 50001);
            let got = h.join().unwrap();
            assert_eq!(got.iter().filter(|q| matches!(q, Request::DeliverLink { .. })).count(), usize::from(opened));
            let _ = std::fs::remove_dir_all(p.parent().unwrap());
        }
    }
}
