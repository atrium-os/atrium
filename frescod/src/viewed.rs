//! The host's own monitors while a viewer has the session (fenestra.md
//! §4.2, §11.4, decision 3).
//!
//! On a host with a seat (monitors of its own), a remote session takes the
//! desktop to the viewer's output, as on a headless host, and the seat's
//! monitors show only this: frescod's own screen saying the session is
//! being viewed, by whom, with a Disconnect. Nothing of the session is
//! shown at the desk and no input from the seat reaches it: the seat's
//! keys and pointer act on this screen alone, and its one control is
//! Disconnect (a click on it, or Return).
//!
//! When the session ends, for any reason, frescod asks ostiarius to lock
//! the seat (its lock socket, as the bar's Lock does) and keeps this
//! screen up until the lock file says the seat is locked; only then do
//! the seat's monitors get the desktop back, behind the lock screen. So
//! nobody at the desk gets the session by waiting for the viewer to go.
//!
//! `FRESCOD_FENESTRA_LIVE=1` is the per-host opt-in to keep showing the
//! session at the desk while viewed (decision 3): none of this applies.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::chrome::{self, Px};

/// How often a lock is asked for again while the seat is not yet locked.
const LOCK_RETRY: Duration = Duration::from_secs(2);
/// How long one lock request may take.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No viewer: the seat shows the desktop.
    Off,
    /// A viewer has the session: the seat shows this screen.
    Viewed,
    /// The session ended; the seat shows this screen until it is locked.
    Ending,
}

struct State {
    phase: Phase,
    /// The seat's size, device pixels, and its scale.
    seat: (u32, u32),
    scale_milli: u32,
    /// The account the session runs as: the seat's user, as the chrome's
    /// font shows it.
    who: String,
    /// Who is viewing, as fenestrad saw them: where from, how many, and
    /// what their key's comment says (their word, not the host's); the
    /// screen's lines for it, none until a viewer connects.
    viewer: Vec<String>,
    /// A live seat (`FRESCOD_FENESTRA_LIVE=1`) is being viewed: the strip
    /// shows the indicator, its Disconnects at these desktop rects.
    live_on: bool,
    live_hits: Vec<[i32; 4]>,
    /// The seat's pointer on this screen; shown once it moves.
    cursor: (f32, f32),
    cursor_shown: bool,
    button: Option<[i32; 4]>,
    /// A press that began on the button (it must end there too).
    pressed_on_button: bool,
    disconnect: bool,
    asked_lock: Option<Instant>,
    /// Bumped whenever the picture changes.
    gen: u64,
    /// The monitor whose screen has the button and the pointer (the seat
    /// size is its size); the others show the words alone.
    primary: Option<u32>,
    /// Pictures made for monitors: (output, width, height, gen) and BGRA.
    cache: Vec<((u32, u32, u32, u64), std::sync::Arc<Vec<u8>>)>,
}

const FRESH: State = State {
    phase: Phase::Off,
    seat: (0, 0),
    scale_milli: 1000,
    who: String::new(),
    viewer: Vec::new(),
    live_on: false,
    live_hits: Vec::new(),
    cursor: (0.0, 0.0),
    cursor_shown: false,
    button: None,
    pressed_on_button: false,
    disconnect: false,
    asked_lock: None,
    gen: 0,
    primary: None,
    cache: Vec::new(),
};

#[cfg(not(test))]
static STATE: Mutex<State> = Mutex::new(FRESH);

static WAKE: Mutex<Option<crate::redraw::Redraw>> = Mutex::new(None);

#[cfg(not(test))]
fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// In tests each thread has its own seat, so a test that is viewed does not
/// take the input of tests running beside it.
#[cfg(test)]
fn state() -> std::sync::MutexGuard<'static, State> {
    thread_local!(static STATE: &'static Mutex<State> = Box::leak(Box::new(Mutex::new(FRESH))));
    STATE.with(|s| *s).lock().unwrap_or_else(|e| e.into_inner())
}

fn changed(s: &mut State) {
    s.gen += 1;
    if let Some(r) = WAKE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        r.wake();
    }
}

/// Redraw through `r` when the seat's picture changes.
pub fn set_redraw(r: crate::redraw::Redraw) {
    *WAKE.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
}

/// Whether the session stays on the seat's monitors while viewed.
pub fn live() -> bool {
    std::env::var("FRESCOD_FENESTRA_LIVE").is_ok_and(|v| v == "1")
}

pub fn phase() -> Phase {
    state().phase
}

/// Whether the seat's input acts on this screen rather than the session.
pub fn active() -> bool {
    phase() != Phase::Off
}

/// The picture's generation: it changed when this did.
pub fn gen() -> u64 {
    state().gen
}

/// A live seat: whether a session is on now (the strip's indicator).
pub fn set_live(on: bool) {
    let mut s = state();
    if s.live_on != on {
        s.live_on = on;
        if !on {
            s.viewer.clear();
            s.live_hits.clear();
        }
        eprintln!("frescod: viewed: live seat, indicator {}", if on { "on" } else { "off" });
        changed(&mut s);
    }
}

/// The live indicator's words while a live seat is viewed, else `None`.
pub fn live_label() -> Option<String> {
    let s = state();
    s.live_on.then(|| s.viewer.first().map_or("viewed remotely".to_string(), |l| format!("viewed {l}")))
}

/// Where the live indicator's Disconnects are now (chrome.rs drew them).
pub fn set_live_hits(hits: Vec<[i32; 4]>) {
    state().live_hits = hits;
}

/// A press at desktop (`x`, `y`) on a live indicator's Disconnect: taken
/// (true), and the sessions end as with the viewed screen's.
pub fn press_live(x: f32, y: f32) -> bool {
    let mut s = state();
    let on = s.live_on && s.live_hits.iter().any(|&[x0, y0, x1, y1]| x >= x0 as f32 && x < x1 as f32 && y >= y0 as f32 && y < y1 as f32);
    if on {
        eprintln!("frescod: viewed: Disconnect pressed on the live indicator");
        s.disconnect = true;
        changed(&mut s);
    }
    on
}

/// Who is viewing (fenestrad's checked [`fenestra_link::Rec::Viewer`]):
/// `viewers` connected, the latest from `addr`, whose key's comment is
/// `key` (may be empty). Shown from now until the session ends.
pub fn set_viewer(viewers: u8, addr: &str, key: &str) {
    let addr = chrome::viewer_name(addr);
    let key = chrome::viewer_name(key);
    let mut lines = Vec::new();
    match viewers {
        0 => {}
        1 => lines.push(format!("from {addr}")),
        n => lines.push(format!("by {n} viewers, the latest from {addr}")),
    }
    if viewers > 0 && !key.is_empty() {
        lines.push(format!("their key says: {key}"));
    }
    let mut s = state();
    if s.viewer != lines {
        eprintln!("frescod: viewed: viewers {viewers}, latest from {addr}, key {key:?}");
        s.viewer = lines;
        changed(&mut s);
    }
}

/// A viewer took the session: the seat (`w` x `h` at `scale_milli`) shows
/// this screen from now.
pub fn begin(w: u32, h: u32, scale_milli: u32) {
    begin_on(None, w, h, scale_milli);
}

/// [`begin`] for a seat of monitors: `primary` (`w` x `h`) has the button.
pub fn begin_on(primary: Option<u32>, w: u32, h: u32, scale_milli: u32) {
    let who = portcullis_peer::seat::active_at(&crate::lock::seat_path()).map(|u| chrome::chrome_name(&u)).unwrap_or_default();
    let mut s = state();
    if s.phase == Phase::Off {
        s.cursor = (w as f32 / 2.0, h as f32 / 2.0);
        s.cursor_shown = false;
    }
    s.phase = Phase::Viewed;
    (s.seat, s.scale_milli, s.who, s.primary) = ((w, h), scale_milli, who, primary);
    s.pressed_on_button = false;
    s.disconnect = false;
    s.asked_lock = None;
    eprintln!("frescod: viewed: the seat's monitors show the viewed screen");
    changed(&mut s);
}

/// The session ended: lock the seat, and keep this screen until it is.
pub fn end() {
    let mut s = state();
    s.viewer.clear();
    if s.phase != Phase::Viewed {
        return;
    }
    s.phase = Phase::Ending;
    s.button = None;
    eprintln!("frescod: viewed: the session ended; locking the seat");
    changed(&mut s);
}

/// Each frame while ending: asks for the lock (again every
/// [`LOCK_RETRY`]), and once the seat is `locked`, ends this screen.
/// True when the seat's monitors get the desktop back now.
pub fn poll(locked: bool) -> bool {
    let mut s = state();
    if s.phase != Phase::Ending {
        return false;
    }
    if locked {
        s.phase = Phase::Off;
        eprintln!("frescod: viewed: the seat is locked; its monitors show the desktop again");
        changed(&mut s);
        return true;
    }
    if s.asked_lock.is_none_or(|t| t.elapsed() >= LOCK_RETRY) {
        s.asked_lock = Some(Instant::now());
        drop(s);
        let _ = std::thread::Builder::new().name("frescod-viewed-lock".into()).spawn(|| {
            if let Err(e) = ask_lock(&lock_sock()) {
                // Once a minute, not at every retry: the seat stays on the
                // viewed screen until it is locked (RUNBOOK, Fenestra).
                static SAID: Mutex<Option<Instant>> = Mutex::new(None);
                let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
                if said.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                    *said = Some(Instant::now());
                    eprintln!("frescod: viewed: locking the seat: {e} (the seat stays on the viewed screen until it is locked)");
                }
            }
            // The next look at the lock, and the next request if it is
            // still not locked: the frame loop may have nothing else to do.
            std::thread::sleep(LOCK_RETRY);
            if let Some(r) = WAKE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                r.wake();
            }
        });
    }
    false
}

fn lock_sock() -> String {
    std::env::var("FRESCOD_LOCK_SOCK").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/atrium/sockets/ostiarius-lock/lock.sock".into())
}

/// One `lock` on ostiarius's lock socket; its answer must be ok.
fn ask_lock(path: &str) -> std::io::Result<()> {
    let c = UnixStream::connect(path)?;
    c.set_read_timeout(Some(LOCK_TIMEOUT))?;
    c.set_write_timeout(Some(LOCK_TIMEOUT))?;
    (&c).write_all(b"{\"op\":\"lock\"}\n")?;
    let mut line = String::new();
    BufReader::new(std::io::Read::take(&c, 1024)).read_line(&mut line)?;
    // ostiarius answers `{"status":"ok",...}` (serde's tag first) or an error.
    if line.trim_start().starts_with("{\"status\":\"ok\"") {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("ostiarius answered {:?}", line.trim())))
    }
}

/// The seat's pointer moved by (`dx`, `dy`) device pixels.
pub fn pointer_by(dx: f32, dy: f32) {
    let mut s = state();
    let (x, y) = s.cursor;
    pointer_set(&mut s, x + dx, y + dy);
}

/// The seat's pointer to (`fx`, `fy`), each 0..=1 across the seat.
pub fn pointer_at(fx: f32, fy: f32) {
    let mut s = state();
    let (w, h) = s.seat;
    pointer_set(&mut s, fx * w as f32, fy * h as f32);
}

/// The seat's pointer to (`x`, `y`) in its pixels (the injector's).
pub fn pointer_to(x: f32, y: f32) {
    pointer_set(&mut state(), x, y);
}

fn pointer_set(s: &mut State, x: f32, y: f32) {
    let (w, h) = s.seat;
    let p = (x.clamp(0.0, w.max(1) as f32 - 1.0), y.clamp(0.0, h.max(1) as f32 - 1.0));
    if p != s.cursor || !s.cursor_shown {
        s.cursor = p;
        s.cursor_shown = true;
        changed(s);
    }
}

fn on_button(s: &State) -> bool {
    let (x, y) = (s.cursor.0 as i32, s.cursor.1 as i32);
    s.button.is_some_and(|[x0, y0, x1, y1]| x >= x0 && x < x1 && y >= y0 && y < y1)
}

/// A seat button: a primary press and release both on Disconnect presses
/// it. Anything else is taken and goes nowhere.
pub fn button(button: u8, down: bool) {
    let mut s = state();
    if s.phase != Phase::Viewed || button != 1 {
        return;
    }
    if down {
        s.pressed_on_button = on_button(&s);
    } else if std::mem::take(&mut s.pressed_on_button) && on_button(&s) {
        press_disconnect(&mut s);
    }
}

/// A seat key (HID keyboard usage): Return or keypad Enter presses
/// Disconnect, the screen's one control. Anything else goes nowhere.
pub fn key(usage: u16, down: bool) {
    let mut s = state();
    if s.phase == Phase::Viewed && down && matches!(usage, 0x28 | 0x58) {
        press_disconnect(&mut s);
    }
}

fn press_disconnect(s: &mut State) {
    if !s.disconnect {
        eprintln!("frescod: viewed: Disconnect pressed at the seat");
        s.disconnect = true;
    }
    changed(s);
}

/// Whether Disconnect was pressed since the last call.
pub fn take_disconnect() -> bool {
    std::mem::take(&mut state().disconnect)
}

/// What the seat's monitors show now (BGRA, the seat's size), or `None`
/// when they show the desktop.
pub fn picture() -> Option<(u32, u32, Vec<u8>)> {
    let mut s = state();
    if s.phase == Phase::Off || s.seat.0 == 0 || s.seat.1 == 0 {
        return None;
    }
    let (w, h) = s.seat;
    Some((w, h, draw(&mut s, w, h, true)))
}

/// Fill `staging` (monitor `output`, `w` x `h`, BGRA) with what it shows
/// while viewed: the viewed screen, or black when that can't be had at
/// this size (never the desktop). False when not viewed: the desktop.
pub fn fill_for(output: u32, w: u32, h: u32, staging: &mut [u8]) -> bool {
    if !active() {
        return false;
    }
    match picture_for(output, w, h).filter(|p| p.len() == staging.len()) {
        Some(p) => staging.copy_from_slice(&p),
        None => staging.fill(0),
    }
    true
}

/// What monitor `output` (`w` x `h`) shows now, or `None` when it shows
/// the desktop; made again only when the picture changed.
pub fn picture_for(output: u32, w: u32, h: u32) -> Option<std::sync::Arc<Vec<u8>>> {
    let mut s = state();
    if s.phase == Phase::Off || w == 0 || h == 0 {
        s.cache.clear();
        return None;
    }
    let key = (output, w, h, s.gen);
    if let Some((_, p)) = s.cache.iter().find(|(k, _)| *k == key) {
        return Some(p.clone());
    }
    let primary = s.primary.is_none_or(|p| p == output);
    let p = std::sync::Arc::new(draw(&mut s, w, h, primary));
    let gen = s.gen;
    s.cache.retain(|((o, ..), _)| *o != output);
    s.cache.retain(|((.., g), _)| *g == gen);
    s.cache.push((key, p.clone()));
    Some(p)
}

/// The screen's words: who views it and as whom, or that it is locking.
fn draw_lines(s: &State) -> Vec<String> {
    let lines = match s.phase {
        Phase::Viewed => [
            vec!["this screen is being viewed remotely".to_string()],
            s.viewer.clone(),
            vec![if s.who.is_empty() { "full control".to_string() } else { format!("full control as {}", s.who) }],
        ]
        .concat(),
        _ => vec!["the remote session ended".to_string(), "locking".to_string()],
    };
    lines.into_iter().filter(|l| !l.is_empty()).collect()
}

fn draw(s: &mut State, w: u32, h: u32, primary: bool) -> Vec<u8> {
    let px = Px::new(s.scale_milli);
    let owned = draw_lines(s);
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    let button = (s.phase == Phase::Viewed).then_some("disconnect");
    let cursor = (primary && s.cursor_shown).then_some((s.cursor.0 as i32, s.cursor.1 as i32));
    let (nodes, hit) = chrome::viewed_screen(px, w, h, &lines, button.filter(|_| primary), cursor);
    if primary {
        s.button = hit;
    }
    chrome::paint(&nodes, w, h)
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        let mut s = state();
        s.phase = Phase::Off;
        s.disconnect = false;
        s.viewer.clear();
    }

    #[test]
    fn a_monitor_never_falls_back_to_the_desktop_while_viewed() {
        reset();
        let mut staging = vec![7u8; 640 * 360 * 4];
        assert!(!fill_for(0, 640, 360, &mut staging), "not viewed: the desktop");
        begin(640, 360, 1000);
        assert!(fill_for(0, 640, 360, &mut staging));
        assert!(staging.chunks_exact(4).any(|p| p != [0, 0, 0, 0]), "the viewed screen");
        // A buffer the picture does not fit (padding, a racing mode change).
        let mut odd = vec![7u8; 640 * 360 * 4 + 64];
        assert!(fill_for(0, 640, 360, &mut odd));
        assert!(odd.iter().all(|b| *b == 0), "black, not the desktop");
        end();
        let mut staging = vec![7u8; 640 * 360 * 4];
        assert!(fill_for(0, 640, 360, &mut staging), "still covered while locking");
        assert!(poll(true));
        assert!(!fill_for(0, 640, 360, &mut staging));
        reset();
    }

    #[test]
    fn a_live_seat_shows_who_and_takes_disconnect_from_its_pill_only() {
        reset();
        assert_eq!(live_label(), None);
        set_live(true);
        assert_eq!(live_label().as_deref(), Some("viewed remotely"));
        set_viewer(1, "192.0.2.4", "k");
        assert_eq!(live_label().as_deref(), Some("viewed from 192.0.2.4"));
        set_live_hits(vec![[100, 3, 180, 21]]);
        assert!(!press_live(99.0, 10.0));
        assert!(!take_disconnect());
        assert!(press_live(100.0, 10.0));
        assert!(take_disconnect());
        set_live(false);
        assert_eq!(live_label(), None);
        assert!(!press_live(100.0, 10.0), "no indicator, nothing to press");
        reset();
    }

    #[test]
    fn the_screen_names_the_viewer_not_only_the_account() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        begin(640, 360, 1000);
        set_viewer(1, "192.0.2.4", "Shankar@MacBook (work)\u{202e}");
        let lines = || draw_lines(&state());
        assert_eq!(lines()[1..3], ["from 192.0.2.4", "their key says: shankar@macbook work"], "{:?}", lines());
        // A second viewer: counted, never hidden behind the first's name.
        set_viewer(2, "2001:db8::1", "");
        // (The last line names the seat's user, whoever it is here.)
        assert_eq!(lines().len(), 3, "{:?}", lines());
        assert_eq!(lines()[1], "by 2 viewers, the latest from 2001:db8::1");
        assert!(lines()[2].starts_with("full control"), "{:?}", lines());
        set_viewer(0, "", "");
        assert_eq!(lines().len(), 2, "{:?}", lines());
        set_viewer(1, "192.0.2.4", "k");
        end();
        assert!(state().viewer.is_empty(), "the next session starts unnamed");
        reset();
    }

    #[test]
    fn disconnect_is_a_click_on_it_or_return_and_nothing_else() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        // Off: nothing is taken.
        key(0x28, true);
        assert!(!take_disconnect());
        begin(800, 600, 1000);
        assert!(active());
        let (w, h, _) = picture().expect("the viewed screen");
        assert_eq!((w, h), (800, 600));
        let [x0, y0, x1, y1] = state().button.expect("a button");
        // Other keys, other buttons, a click off it, a press that slides
        // onto it: nothing.
        key(0x04, true);
        button(3, true);
        button(3, false);
        pointer_to(5.0, 5.0);
        button(1, true);
        pointer_to(((x0 + x1) / 2) as f32, ((y0 + y1) / 2) as f32);
        button(1, false);
        assert!(!take_disconnect());
        // A click on it, then Return.
        button(1, true);
        button(1, false);
        assert!(take_disconnect());
        assert!(!take_disconnect());
        key(0x28, true);
        assert!(take_disconnect());
        reset();
    }

    #[test]
    fn the_seat_gets_the_desktop_back_only_once_locked() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        std::env::set_var("FRESCOD_LOCK_SOCK", "/nonexistent/lock.sock");
        begin(640, 480, 2000);
        assert!(!poll(true), "not ending yet");
        end();
        assert_eq!(phase(), Phase::Ending);
        // No button while ending, and its keys press nothing.
        assert!(picture().is_some());
        key(0x28, true);
        assert!(!take_disconnect());
        assert!(!poll(false));
        assert!(!poll(false));
        assert_eq!(phase(), Phase::Ending);
        assert!(poll(true));
        assert_eq!(phase(), Phase::Off);
        assert!(picture().is_none());
        reset();
    }

    #[test]
    fn a_lock_is_asked_of_ostiarius_and_its_answer_checked() {
        let d = std::env::temp_dir().join(format!("frescod-viewed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("lock.sock");
        let _ = std::fs::remove_file(&p);
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let t = std::thread::spawn(move || {
            for answer in ["{\"status\":\"ok\"}\n", "{\"status\":\"err\",\"message\":\"no\"}\n"] {
                let (c, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&c).read_line(&mut line).unwrap();
                assert_eq!(line, "{\"op\":\"lock\"}\n");
                (&c).write_all(answer.as_bytes()).unwrap();
            }
        });
        assert!(ask_lock(p.to_str().unwrap()).is_ok());
        assert!(ask_lock(p.to_str().unwrap()).is_err());
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
