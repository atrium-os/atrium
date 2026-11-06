//! Media keys (atrium-player-pip-keys.md §4.3): the keyboard's or headset's
//! Play/Pause, Next, Previous and Stop go to "the thing that is playing",
//! whatever has focus. forum-wm never forwards them to the focused app; it
//! sends choragusd a `Control`, and choragusd picks the active session.
//! forum-wm also tells choragusd which app has focus, so a background app
//! can't take the keys by starting silent audio (§4.2).
//!
//! frescod hands consumer-page (HID page 0x0C) usages to the WM as key
//! events with [`CONSUMER`] set on the usage, so they can't be mistaken for
//! keyboard-page ones.

use lyra_protocol::wire::{MediaAction, ToMedia};

/// Set on a key event's usage for a consumer-page usage.
pub const CONSUMER: u16 = 0x0C00;
pub const PLAY: u16 = CONSUMER | 0xB0;
pub const PAUSE: u16 = CONSUMER | 0xB1;
pub const NEXT: u16 = CONSUMER | 0xB5;
pub const PREVIOUS: u16 = CONSUMER | 0xB6;
pub const STOP: u16 = CONSUMER | 0xB7;
pub const PLAY_PAUSE: u16 = CONSUMER | 0xCD;

/// choragusd's media-control socket, in forum-wm's jail.
pub const SOCKET: &str = "/atrium/sockets/media-control/media-control.sock";

/// The action a key press asks for, if it is a media key.
pub fn action(hid_usage: u16) -> Option<MediaAction> {
    Some(match hid_usage {
        PLAY_PAUSE => MediaAction::PlayPause,
        PLAY => MediaAction::Play,
        PAUSE => MediaAction::Pause,
        STOP => MediaAction::Stop,
        NEXT => MediaAction::Next,
        PREVIOUS => MediaAction::Previous,
        _ => return None,
    })
}

/// Where `Control` and `Focused` go: choragusd, or a test's fake.
pub trait Sink {
    fn send(&mut self, m: &ToMedia) -> std::io::Result<()>;
}

/// forum-wm's media-control link: connects when first needed, again after
/// choragusd restarts, and sends `Focused` only when focus moved to another
/// app (and again on a new connection, which starts with none).
pub struct Link<S: Sink> {
    connect: Box<dyn FnMut() -> std::io::Result<S> + Send>,
    sink: Option<S>,
    focused: Option<String>,
    told: bool,
}

impl<S: Sink> Link<S> {
    pub fn new(connect: impl FnMut() -> std::io::Result<S> + Send + 'static) -> Self {
        Self { connect: Box::new(connect), sink: None, focused: None, told: false }
    }

    fn with(&mut self, m: &ToMedia) -> std::io::Result<()> {
        for _ in 0..2 {
            if self.sink.is_none() {
                self.sink = Some((self.connect)()?);
                self.told = false;
                if let Some(app) = self.focused.clone() {
                    self.sink.as_mut().unwrap().send(&ToMedia::Focused { app_id: app })?;
                    self.told = true;
                }
                if matches!(m, ToMedia::Focused { .. }) {
                    return Ok(());
                }
            }
            match self.sink.as_mut().unwrap().send(m) {
                Ok(()) => return Ok(()),
                // choragusd went away: once more on a new connection.
                Err(_) => self.sink = None,
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "choragusd media-control"))
    }

    /// A media key was pressed.
    pub fn key(&mut self, a: MediaAction) -> std::io::Result<()> {
        self.with(&ToMedia::Control { action: a, seek_offset: None, seek_time: None })
    }

    /// Focus is now on `app` (its registry id).
    pub fn focus(&mut self, app: &str) -> std::io::Result<()> {
        if self.focused.as_deref() == Some(app) && self.told {
            return Ok(());
        }
        self.focused = Some(app.to_string());
        self.told = false;
        self.with(&ToMedia::Focused { app_id: app.to_string() })?;
        self.told = true;
        Ok(())
    }
}

impl Sink for lyra_protocol::client::Media {
    fn send(&mut self, m: &ToMedia) -> std::io::Result<()> {
        lyra_protocol::client::Media::send(self, m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<(Vec<String>, bool)>>);
    impl Sink for Fake {
        fn send(&mut self, m: &ToMedia) -> std::io::Result<()> {
            let mut g = self.0.lock().unwrap();
            if g.1 {
                g.1 = false;
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            g.0.push(format!("{m:?}"));
            Ok(())
        }
    }

    #[test]
    fn media_keys_become_controls_and_focus_is_told_once_per_app() {
        assert_eq!(action(PLAY_PAUSE), Some(MediaAction::PlayPause));
        assert_eq!(action(NEXT), Some(MediaAction::Next));
        assert_eq!(action(0xCD), None, "keyboard page 0xCD is not Play/Pause");
        let f = Fake::default();
        let conns = Arc::new(Mutex::new(0));
        let (f2, c2) = (f.clone(), conns.clone());
        let mut l = Link::new(move || {
            *c2.lock().unwrap() += 1;
            Ok(f2.clone())
        });
        l.focus("org.example.mail").unwrap();
        l.focus("org.example.mail").unwrap();
        l.key(MediaAction::PlayPause).unwrap();
        l.focus("org.atrium.navigator").unwrap();
        // choragusd restarted: a new connection, told the focus first.
        f.0.lock().unwrap().1 = true;
        l.key(MediaAction::Next).unwrap();
        let got = f.0.lock().unwrap().0.clone();
        assert_eq!(got, [
            r#"Focused { app_id: "org.example.mail" }"#,
            "Control { action: PlayPause, seek_offset: None, seek_time: None }",
            r#"Focused { app_id: "org.atrium.navigator" }"#,
            r#"Focused { app_id: "org.atrium.navigator" }"#,
            "Control { action: Next, seek_offset: None, seek_time: None }",
        ]);
        assert_eq!(*conns.lock().unwrap(), 2);
    }
}
