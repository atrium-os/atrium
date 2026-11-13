//! The lock screen, frescod's side (docs/spec/lock-screen.md §3).
//!
//! ostiarius owns the lock: it writes the seat's lock file, naming the
//! user, beside the active-session file in its socket dir, which frescod's
//! jail has read-only through /atrium/sockets. While that file names the
//! active user, only the lock screen's windows (vestibulum's uid, from the
//! launch registry) and Sigillum's sheets are composed or get input; the
//! WM's background is not drawn and the WM hears no input. frescod reads
//! the files itself: no client can tell it the screen is unlocked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the lock files are read again.
const POLL: Duration = Duration::from_millis(100);

/// The lock screen's app (ostiarius launches it owned by `_login`).
const LOCK_APP: (&str, &str) = ("_login", "org.atrium.vestibulum");

static LOCKED: AtomicBool = AtomicBool::new(false);

/// Whether the screen was locked when last read (input routing).
pub fn locked() -> bool {
    LOCKED.load(Ordering::Relaxed)
}

fn seat_path() -> String {
    std::env::var("FRESCOD_SEAT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| portcullis_peer::seat::ACTIVE_SESSION.to_string())
}

/// Follow the lock files on a thread: on a change, apply it to the
/// compositor and the input routing at once and redraw, so a lock takes
/// effect within [`POLL`] even on a screen that is not otherwise redrawn.
pub fn watch(comp: std::sync::Arc<Mutex<fresco_scene_server::window::Compositor>>, redraw: crate::redraw::Redraw) {
    let _ = std::thread::Builder::new().name("frescod-lock".into()).spawn(move || {
        let mut last: Option<Vec<u32>> = None;
        loop {
            let v = read(&seat_path(), crate::activation::sheet_uids());
            if v != last {
                // Routing first: from here no input reaches a session window or the WM.
                LOCKED.store(v.is_some(), Ordering::Relaxed);
                comp.lock().unwrap().set_lock(v.clone());
                eprintln!("frescod: screen {}", if v.is_some() { "locked" } else { "unlocked" });
                last = v;
                redraw.wake();
            }
            std::thread::sleep(POLL);
        }
    });
}

/// The uids shown while locked, or `None` when the session is not locked.
/// Read at most every [`POLL`].
pub fn current() -> Option<Vec<u32>> {
    static LAST: Mutex<Option<(Instant, Option<Vec<u32>>)>> = Mutex::new(None);
    let mut g = LAST.lock().unwrap();
    if let Some((at, v)) = g.as_ref() {
        if at.elapsed() < POLL {
            return v.clone();
        }
    }
    let v = read(&seat_path(), crate::activation::sheet_uids());
    LOCKED.store(v.is_some(), Ordering::Relaxed);
    *g = Some((Instant::now(), v.clone()));
    v
}

/// [`current`] against the seat file at `seat`, unpolled (tests).
pub fn read(seat: &str, sheet_uids: &[u32]) -> Option<Vec<u32>> {
    if !portcullis_peer::seat::is_locked_at(seat) {
        return None;
    }
    let registry = std::env::var("FRESCOD_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
    let reg = portcullis_peer::AppRegistry::load(&registry).unwrap_or_default();
    let mut uids: Vec<u32> = sheet_uids.to_vec();
    uids.extend(reg.uid_for(LOCK_APP.0, LOCK_APP.1));
    Some(uids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seat_lock_file_names_who_is_shown() {
        let d = std::env::temp_dir().join(format!("frescod-lock-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let seat = d.join("active-session");
        let seat_s = seat.to_str().unwrap();
        assert_eq!(read(seat_s, &[65030]), None, "no session");
        portcullis_peer::seat::set_active_at(seat_s, "alice").unwrap();
        assert_eq!(read(seat_s, &[65030]), None, "in use");
        portcullis_peer::seat::set_locked_at(&portcullis_peer::seat::locked_path(seat_s), Some("alice")).unwrap();
        let u = read(seat_s, &[65030]).expect("locked");
        assert!(u.contains(&65030));
        // A stale lock (another user's) does not lock this session.
        portcullis_peer::seat::set_locked_at(&portcullis_peer::seat::locked_path(seat_s), Some("bob")).unwrap();
        assert_eq!(read(seat_s, &[65030]), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
