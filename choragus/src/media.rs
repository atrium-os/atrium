//! Media sessions (atrium-player-pip-keys.md §4): what each app connection is
//! playing, which one is *active* for the media keys and now-playing, and the
//! actions a controller may send it.
//!
//! The rule for the active session (§4.2): it is the one that most recently
//! went to playing. A session that pauses stays active, so Play/Pause resumes
//! it. Only a session whose app had the keyboard focus since the session began
//! can become active, so a background app can't take the keys by starting to
//! play (if no controller reports focus, as in development, any session can).
//! Everything is per user; the daemon shows and steers only the active seat's.

use lyra_protocol::wire::{action, MediaAction, NowPlaying, SessionState, ToApp, MAX_SESSION_TEXT};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Longest seek a controller may ask for (seconds).
const MAX_SEEK: f64 = 24.0 * 3600.0;

pub struct SessionRec {
    pub user: String,
    pub app_id: String,
    pub state: SessionState,
    pub title: String,
    pub artist: String,
    pub actions: u16,
    pub duration: Option<f64>,
    pub position: Option<f64>,
    last_position: Option<Instant>,
    /// The app's focus count when the session began, and whether it had the
    /// focus then.
    focus_at_start: u64,
    focused_at_start: bool,
    /// Order in which sessions last went to playing while eligible (0: never).
    play_seq: u64,
}

#[derive(Default)]
pub struct MediaRegistry {
    sessions: HashMap<u64, SessionRec>,
    seq: u64,
    /// The active session per user.
    active: HashMap<String, u64>,
    /// How often each (user, app) gained focus, and who has it now.
    focus_count: HashMap<(String, String), u64>,
    focused: HashMap<String, String>,
    /// Users for whom some controller reports focus.
    focus_known: HashSet<String>,
}

/// Page text as now-playing may show it: no control or bidi-override
/// characters, at most [`MAX_SESSION_TEXT`] bytes (cut on a character).
pub fn clean(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if c.is_control() || bidi {
            continue;
        }
        if out.len() + c.len_utf8() > MAX_SESSION_TEXT {
            break;
        }
        out.push(c);
    }
    out
}

fn finite(x: Option<f64>) -> Option<f64> {
    x.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1e9))
}

impl MediaRegistry {
    /// An app connection's `Session` message. Returns whether `user`'s
    /// now-playing may have changed.
    #[allow(clippy::too_many_arguments)]
    pub fn update(&mut self, conn: u64, user: &str, app_id: &str, state: SessionState, title: &str, artist: &str,
        actions: u16, duration: Option<f64>, position: Option<f64>, now: Instant) -> bool {
        if state == SessionState::None {
            return self.remove(conn).is_some();
        }
        let focus = self.focus_count.get(&(user.to_string(), app_id.to_string())).copied().unwrap_or(0);
        let focused_now = self.focused.get(user).map(String::as_str) == Some(app_id);
        let r = self.sessions.entry(conn).or_insert_with(|| SessionRec {
            user: user.to_string(),
            app_id: app_id.to_string(),
            state: SessionState::None,
            title: String::new(),
            artist: String::new(),
            actions: 0,
            duration: None,
            position: None,
            last_position: None,
            focus_at_start: focus,
            focused_at_start: focused_now,
            play_seq: 0,
        });
        let started = state == SessionState::Playing && r.state != SessionState::Playing;
        let changed_state = state != r.state;
        r.state = state;
        r.title = clean(title);
        r.artist = clean(artist);
        r.actions = actions & action::ALL;
        r.duration = finite(duration);
        // Position at most once a second, unless the state changed.
        let due = r.last_position.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
        if changed_state || due {
            r.position = finite(position);
            r.last_position = Some(now);
        }
        let eligible = !self.focus_known.contains(user) || focused_now || r.focused_at_start || focus > r.focus_at_start;
        if started && eligible {
            self.seq += 1;
            r.play_seq = self.seq;
            self.active.insert(user.to_string(), conn);
        }
        true
    }

    /// The connection is gone (or ended its session). Returns its user.
    pub fn remove(&mut self, conn: u64) -> Option<String> {
        let r = self.sessions.remove(&conn)?;
        if self.active.get(&r.user) == Some(&conn) {
            // The most recent other session that was ever active takes over.
            let next = self.sessions.iter()
                .filter(|(_, s)| s.user == r.user && s.play_seq > 0)
                .max_by_key(|(_, s)| s.play_seq)
                .map(|(c, _)| *c);
            match next {
                Some(c) => { self.active.insert(r.user.clone(), c); }
                None => { self.active.remove(&r.user); }
            }
        }
        Some(r.user)
    }

    /// A controller says `app_id` has the keyboard focus for `user`.
    pub fn focused(&mut self, user: &str, app_id: &str) {
        self.focus_known.insert(user.to_string());
        if self.focused.get(user).map(String::as_str) != Some(app_id) {
            *self.focus_count.entry((user.to_string(), app_id.to_string())).or_default() += 1;
            self.focused.insert(user.to_string(), app_id.to_string());
        }
    }

    pub fn active(&self, user: &str) -> Option<(u64, &SessionRec)> {
        let c = *self.active.get(user)?;
        self.sessions.get(&c).map(|s| (c, s))
    }

    /// `user`'s now-playing; titles left out unless `titles`.
    pub fn now_playing(&self, user: &str, titles: bool) -> Option<NowPlaying> {
        let (_, s) = self.active(user)?;
        let mut app_id = s.app_id.clone();
        app_id.truncate(lyra_protocol::wire::MAX_APP_ID);
        Some(NowPlaying {
            app_id,
            title: if titles { s.title.clone() } else { String::new() },
            artist: if titles { s.artist.clone() } else { String::new() },
            state: s.state,
            actions: s.actions,
            duration: s.duration,
            position: s.position,
        })
    }

    /// A controller's action for `user`'s active session: the connection to
    /// send it to and the message, or `None` if there is no active session or
    /// it doesn't offer the action.
    pub fn control(&self, user: &str, action: MediaAction, seek_offset: Option<f64>, seek_time: Option<f64>) -> Option<(u64, ToApp)> {
        let (conn, s) = self.active(user)?;
        let action = match action {
            MediaAction::PlayPause if s.state == SessionState::Playing => MediaAction::Pause,
            MediaAction::PlayPause => MediaAction::Play,
            a => a,
        };
        if s.actions & action.bit() == 0 {
            return None;
        }
        let (seek_offset, seek_time) = match action {
            MediaAction::SeekBackward | MediaAction::SeekForward => {
                (Some(seek_offset.filter(|v| v.is_finite()).map_or(10.0, |v| v.abs().min(MAX_SEEK))), None)
            }
            MediaAction::SeekTo => {
                let t = seek_time.filter(|v| v.is_finite())?.clamp(0.0, s.duration.unwrap_or(MAX_SEEK));
                (None, Some(t))
            }
            _ => (None, None),
        };
        Some((conn, ToApp::SessionAction { action, seek_offset, seek_time }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionState::{Paused, Playing};

    fn play(m: &mut MediaRegistry, conn: u64, app: &str, state: SessionState) {
        m.update(conn, "alice", app, state, "Song", "Band", action::ALL, Some(100.0), Some(0.0), Instant::now());
    }

    #[test]
    fn the_last_to_start_playing_is_active_and_a_paused_one_stays() {
        let mut m = MediaRegistry::default();
        play(&mut m, 1, "a", Playing);
        play(&mut m, 2, "b", Playing);
        assert_eq!(m.active("alice").unwrap().0, 2);
        play(&mut m, 2, "b", Paused);
        assert_eq!(m.active("alice").unwrap().0, 2, "pausing keeps it active");
        let (c, msg) = m.control("alice", MediaAction::PlayPause, None, None).unwrap();
        assert_eq!((c, msg), (2, ToApp::SessionAction { action: MediaAction::Play, seek_offset: None, seek_time: None }));
        // It goes away: the one before takes over.
        m.remove(2);
        assert_eq!(m.active("alice").unwrap().0, 1);
        play(&mut m, 1, "a", SessionState::None);
        assert!(m.active("alice").is_none());
        assert!(m.active("bob").is_none());
    }

    #[test]
    fn a_background_app_cant_take_the_keys() {
        let mut m = MediaRegistry::default();
        m.focused("alice", "player");
        play(&mut m, 1, "player", Playing);
        assert_eq!(m.active("alice").unwrap().0, 1);
        // An app that never had focus starts playing: not active.
        play(&mut m, 2, "sneaky", Playing);
        assert_eq!(m.active("alice").unwrap().0, 1);
        // Once the user focuses it and it plays again, it is.
        m.focused("alice", "sneaky");
        m.focused("alice", "player");
        play(&mut m, 2, "sneaky", Paused);
        play(&mut m, 2, "sneaky", Playing);
        assert_eq!(m.active("alice").unwrap().0, 2);
    }

    #[test]
    fn actions_are_checked_and_seeks_clamped() {
        let mut m = MediaRegistry::default();
        m.update(1, "alice", "p", Playing, "t", "a", action::PLAY | action::PAUSE | action::SEEK_TO, Some(60.0), None, Instant::now());
        assert!(m.control("alice", MediaAction::Next, None, None).is_none(), "not offered");
        let (_, msg) = m.control("alice", MediaAction::SeekTo, None, Some(1e9)).unwrap();
        assert_eq!(msg, ToApp::SessionAction { action: MediaAction::SeekTo, seek_offset: None, seek_time: Some(60.0) });
        assert!(m.control("alice", MediaAction::SeekTo, None, Some(f64::NAN)).is_none());
        let (_, msg) = m.control("alice", MediaAction::PlayPause, None, None).unwrap();
        assert_eq!(msg, ToApp::SessionAction { action: MediaAction::Pause, seek_offset: None, seek_time: None });
    }

    #[test]
    fn page_text_is_cleaned_and_titles_are_opt_in() {
        assert_eq!(clean("a\u{202e}b\nc\u{0}d"), "abcd");
        let long = "é".repeat(100);
        let c = clean(&long);
        assert!(c.len() <= MAX_SESSION_TEXT && c.len() % 2 == 0);
        let mut m = MediaRegistry::default();
        play(&mut m, 1, "p", Playing);
        let np = m.now_playing("alice", false).unwrap();
        assert_eq!((np.title.as_str(), np.artist.as_str(), np.state), ("", "", Playing));
        assert_eq!(m.now_playing("alice", true).unwrap().title, "Song");
    }

    #[test]
    fn position_updates_are_rate_limited() {
        let mut m = MediaRegistry::default();
        let t0 = Instant::now();
        m.update(1, "u", "p", Playing, "", "", 0, None, Some(1.0), t0);
        m.update(1, "u", "p", Playing, "", "", 0, None, Some(2.0), t0 + Duration::from_millis(100));
        assert_eq!(m.now_playing("u", true).unwrap().position, Some(1.0));
        m.update(1, "u", "p", Paused, "", "", 0, None, Some(3.0), t0 + Duration::from_millis(200));
        assert_eq!(m.now_playing("u", true).unwrap().position, Some(3.0), "a state change always carries it");
        m.update(1, "u", "p", Paused, "", "", 0, None, Some(f64::INFINITY), t0 + Duration::from_secs(2));
        assert_eq!(m.now_playing("u", true).unwrap().position, None);
    }
}
