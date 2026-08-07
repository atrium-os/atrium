//! `Window` — Pergola owns the window lifecycle and the event loop.
//!
//! Before this existed every app hand-rolled the same 40 lines:
//! connect → window_create → initial tick/commit/present → wait_event
//! → translate → handle_event → tick → commit/present. Five copies had
//! five sets of bugs (hardcoded sizes, missed initial paint, dropped
//! close events). The loop lives here now; apps provide a `View`, an
//! `App`, and optionally a raw-event hook for app-level chords
//! (Esc-to-quit, debug dumps) that runs *before* translation.
//!
//! Sizing comes from `display_info()` for full-screen/anchored
//! surfaces — never assume a screen size (the dock/WM/scanout
//! disagreement that motivated OP_DISPLAY_INFO).

use std::io;
use std::time::Duration;

use fresco_client::Connection;
use fresco_protocol::WindowHints;

use crate::app::App;
use crate::input::translate;
use crate::surface::{commit, FrescoSurface, Surface};
use crate::text::{install_measurer, WireMeasurer};
use crate::view::View;

/// How big the window wants to be.
#[derive(Debug, Clone, Copy)]
pub enum SizeSpec {
    Fixed(u32, u32),
    /// The full scanout mode, asked from the server.
    FullScreen,
}

pub struct WindowDesc {
    pub title: String,
    pub size: SizeSpec,
    pub hints: WindowHints,
}

impl WindowDesc {
    pub fn new(title: impl Into<String>, size: SizeSpec) -> Self {
        Self { title: title.into(), size, hints: WindowHints::default() }
    }
}

/// Loop control handed to the raw-event hook.
pub struct Flow {
    exit: bool,
    /// When set, the event handled by the hook is not translated into
    /// a Pergola event (the hook consumed it).
    consumed: bool,
}

impl Flow {
    pub fn exit(&mut self) { self.exit = true; }
    pub fn consume(&mut self) { self.consumed = true; }
}

pub struct Window {
    surface: FrescoSurface,
    pub width: f32,
    pub height: f32,
}

/// Default socket path, overridable with `FRESCO_SOCKET`.
fn socket_path() -> String {
    std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into())
}

impl Window {
    /// Connect, size, create the window, and install the global text
    /// measurer (its own connection — measurement replies must not
    /// interleave with this window's event stream). Measurer failure
    /// is non-fatal: layout falls back to estimates.
    pub fn open(desc: &WindowDesc) -> io::Result<Self> {
        let socket = socket_path();
        let mut conn = Connection::connect(&socket)?;

        let (w, h) = match desc.size {
            SizeSpec::Fixed(w, h) => (w, h),
            SizeSpec::FullScreen => {
                let info = conn.display_info()?;
                if info.width == 0 || info.height == 0 {
                    log::warn!("window: display reports no mode; assuming 1280x720");
                    (1280, 720)
                } else {
                    (info.width, info.height)
                }
            }
        };

        let win = conn.window_create(w, h, &desc.title, desc.hints.clone())?;

        match WireMeasurer::connect(&socket) {
            Ok(m) => install_measurer(Box::new(m)),
            Err(e) => log::warn!("window: no text measurer ({e}); using estimates"),
        }

        Ok(Self {
            surface: FrescoSurface::new(conn, win),
            width: w as f32,
            height: h as f32,
        })
    }

    /// Escape hatch for callers that need connection-level ops the
    /// loop doesn't cover yet.
    pub fn connection(&mut self) -> &mut Connection {
        self.surface.connection()
    }

    /// Paint one frame now (initial paint, or after out-of-loop state
    /// changes).
    pub fn paint<V: View>(&mut self, app: &mut App<V>) -> io::Result<()> {
        let deltas = app.tick();
        if !deltas.is_empty() {
            commit(&mut self.surface, &deltas)?;
            self.surface.present()?;
        }
        Ok(())
    }

    /// One loop iteration: wait up to `timeout` for an event, default-
    /// dispatch it into the app, repaint if dirty. Returns the raw
    /// event (`None` on timeout) so callers with periodic work — a
    /// clock, a poll — can run their own loop around it. Returns
    /// `Ok(None)` *and* sets `exited` via the return when the server
    /// asked the window to close — see `StepResult`.
    pub fn step<V: View>(
        &mut self,
        app: &mut App<V>,
        timeout: Option<Duration>,
    ) -> io::Result<StepResult> {
        let ev = self.surface.connection().wait_event(timeout)?;
        let result = match ev {
            Some(ev) => {
                if matches!(ev, fresco_client::Event::CloseRequested { .. }) {
                    return Ok(StepResult::Closed);
                }
                if let Some(pev) = translate(&ev) {
                    app.handle_event(pev);
                }
                StepResult::Event(ev)
            }
            None => StepResult::Timeout,
        };
        self.paint(app)?;
        Ok(result)
    }

    /// Run the event loop until the hook calls `flow.exit()` or the
    /// server asks the window to close. The hook sees every raw
    /// `fresco_client::Event` before translation (and may mutate the
    /// app — theme toggles, state changes); `flow.consume()` stops the
    /// event from reaching the `App`'s default dispatch.
    pub fn run<V: View>(
        mut self,
        app: &mut App<V>,
        mut hook: impl FnMut(&fresco_client::Event, &mut App<V>, &mut Flow),
    ) -> io::Result<()> {
        self.paint(app)?;

        loop {
            let ev = match self.surface.connection().wait_event(None)? {
                Some(ev) => ev,
                None => continue,
            };

            let mut flow = Flow { exit: false, consumed: false };
            hook(&ev, app, &mut flow);
            if flow.exit {
                return Ok(());
            }

            if !flow.consumed {
                if matches!(ev, fresco_client::Event::CloseRequested { .. }) {
                    return Ok(());
                }
                if let Some(pev) = translate(&ev) {
                    app.handle_event(pev);
                }
            }

            self.paint(app)?;
        }
    }
}

/// What `Window::step` observed.
#[derive(Debug)]
pub enum StepResult {
    /// An event arrived and was dispatched.
    Event(fresco_client::Event),
    /// The wait timed out — do periodic work and step again.
    Timeout,
    /// The server asked the window to close.
    Closed,
}
