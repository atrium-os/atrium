//! The route in the persona chip (vpn.md §7.4, personas.md §7): how the
//! focused window's traffic leaves, drawn by frescod from files no app
//! writes. portcullisd's `persona-vpn` (`<person> <persona> <profile>`)
//! says which VPN a persona uses; cryptad's `vpn-state`
//! (`<profile> up|connecting machine|persona`) says which tunnels run. Both
//! are read again at most every 2 s.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What the chip adds after the persona's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Through this VPN, connected.
    Via(String),
    /// This VPN is starting or reconnecting.
    Connecting(String),
    /// The persona's VPN is not running: its apps reach nothing.
    Down(String),
    /// A VPN is in use elsewhere, but not for this window.
    Direct,
}

impl Route {
    /// The chip's text for persona `name`.
    pub fn label(&self, name: &str) -> String {
        match self {
            Route::Via(p) => format!("{name} - via {p}"),
            Route::Connecting(_) => format!("{name} - connecting"),
            Route::Down(p) => format!("{name} - {p} down: no network"),
            Route::Direct => format!("{name} - direct"),
        }
    }

    /// Down is drawn amber, whatever the persona's colour.
    pub fn warns(&self) -> bool {
        matches!(self, Route::Down(_))
    }
}

/// The amber of a persona whose VPN is down.
pub const DOWN_COLOUR: [u8; 3] = [0xf5, 0x9e, 0x0b];

/// One running tunnel, from `vpn-state`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tunnel {
    profile: String,
    up: bool,
    machine: bool,
}

fn parse_state(text: &str) -> Vec<Tunnel> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            match f.as_slice() {
                [p, s @ ("up" | "connecting"), sc @ ("machine" | "persona")] if plain(p) => {
                    Some(Tunnel { profile: p.to_string(), up: *s == "up", machine: *sc == "machine" })
                }
                _ => None,
            }
        })
        .collect()
}

fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// `person`'s persona `persona`'s VPN profile, from `persona-vpn` text.
fn vpn_of<'a>(text: &'a str, person: &str, persona: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        (f.len() == 3 && f[0] == person && f[1] == persona && plain(f[2])).then_some(f[2])
    })
}

/// The route for `person`'s `persona` (None: Personal), given both files'
/// text. `None`: no VPN anywhere, so the chip says nothing more.
fn decide(routes: &str, state: &str, person: Option<&str>, persona: Option<&str>) -> Option<Route> {
    let tunnels = parse_state(state);
    if let (Some(u), Some(p)) = (person, persona) {
        if let Some(v) = vpn_of(routes, u, p) {
            return Some(match tunnels.iter().find(|t| t.profile == v) {
                Some(t) if t.up => Route::Via(v.to_string()),
                Some(_) => Route::Connecting(v.to_string()),
                None => Route::Down(v.to_string()),
            });
        }
    }
    // Everything not in a persona's route domain goes the machine's way.
    if let Some(t) = tunnels.iter().find(|t| t.machine) {
        return Some(if t.up { Route::Via(t.profile.clone()) } else { Route::Connecting(t.profile.clone()) });
    }
    (!tunnels.is_empty()).then_some(Route::Direct)
}

fn read_cached(slot: &'static Mutex<Option<(Instant, String)>>, env: &str, default: &str) -> String {
    let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(2)) {
        let path = std::env::var(env).unwrap_or_else(|_| default.into());
        *g = Some((Instant::now(), std::fs::read_to_string(path).unwrap_or_default()));
    }
    g.as_ref().map(|(_, t)| t.clone()).unwrap_or_default()
}

fn files() -> (String, String) {
    static ROUTES: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    static STATE: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    (
        read_cached(&ROUTES, "FRESCOD_PERSONA_VPN", "/var/run/atrium/persona-vpn"),
        read_cached(&STATE, "FRESCOD_VPN_STATE", "/var/run/atrium/vpn-state"),
    )
}

/// The route of `person`'s `persona` now (None: Personal).
pub fn route(person: Option<&str>, persona: Option<&str>) -> Option<Route> {
    let (routes, state) = files();
    decide(&routes, &state, person, persona)
}

/// Redraw when either file changes (checked every 2 s), so the route is
/// right on a screen nothing else redraws.
pub fn watch(redraw: crate::redraw::Redraw) {
    let _ = std::thread::Builder::new().name("frescod-route".into()).spawn(move || {
        let mut last = files();
        loop {
            std::thread::sleep(Duration::from_millis(2100));
            let now = files();
            if now != last {
                last = now;
                redraw.wake();
            }
        }
    });
}

/// Whether any tunnel runs, so Personal windows are labelled too.
pub fn any_vpn() -> bool {
    !parse_state(&files().1).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str = "alice work acme\nbob work other\n";

    #[test]
    fn a_personas_vpn_up_connecting_and_down() {
        let r = |state| decide(ROUTES, state, Some("alice"), Some("work"));
        assert_eq!(r("acme up persona\n"), Some(Route::Via("acme".into())));
        assert_eq!(r("acme connecting persona\n"), Some(Route::Connecting("acme".into())));
        assert_eq!(r(""), Some(Route::Down("acme".into())));
        assert_eq!(r("other up persona\n"), Some(Route::Down("acme".into())));
        assert_eq!(Route::Down("acme".into()).label("work"), "work - acme down: no network");
        assert!(Route::Down("acme".into()).warns());
    }

    #[test]
    fn personal_is_direct_or_the_machines() {
        let p = |state| decide(ROUTES, state, Some("alice"), None);
        assert_eq!(p(""), None);
        assert_eq!(p("acme up persona\n"), Some(Route::Direct));
        assert_eq!(p("acme up persona\nhome up machine\n"), Some(Route::Via("home".into())));
        // A persona with no VPN of its own goes the machine's way too.
        assert_eq!(decide(ROUTES, "home connecting machine\n", Some("alice"), Some("school")), Some(Route::Connecting("home".into())));
        assert_eq!(Route::Direct.label("personal"), "personal - direct");
    }

    #[test]
    fn bad_lines_are_ignored() {
        assert!(parse_state("acme maybe persona\nx y\n../x up machine\n").is_empty());
        assert_eq!(decide("alice work ../etc\n", "", Some("alice"), Some("work")), None);
    }
}
