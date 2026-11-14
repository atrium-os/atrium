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
    /// Open as the pop-up a grant's token names (a status item's panel,
    /// hanging from its icon in the bar; forum-status-items.md §4)
    /// rather than as a window of its own. It closes for good when
    /// frescod dismisses it: [`Window::run`] returns, [`Window::step`]
    /// says [`StepResult::Closed`].
    pub hosted: Option<[u8; 16]>,
}

impl WindowDesc {
    pub fn new(title: impl Into<String>, size: SizeSpec) -> Self {
        Self { title: title.into(), size, hints: WindowHints::default(), hosted: None }
    }

    /// The panel a grant's `token` lets the app hang from the bar,
    /// `width` x `height` (frescod may cut it to the grant's).
    pub fn hosted(token: [u8; 16], width: u32, height: u32) -> Self {
        Self { hosted: Some(token), ..Self::new("", SizeSpec::Fixed(width, height)) }
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
    /// The app clock's zero.
    epoch: std::time::Instant,
    pub width: f32,
    pub height: f32,
    /// Where slot placements go (Limen, or a test's stand-in).
    binder: Option<Box<dyn crate::slot::SlotBinder>>,
    /// frescod's embed token for this window (limen.md §1), once it came.
    embed_token: Option<[u8; 16]>,
    /// The system appearance not yet applied to the app (visual language
    /// §15): set at open and on each change, applied at the next paint.
    appearance: Option<crate::theme::Semantic>,
    /// Whether the app takes the system appearance (default on).
    follow_appearance: bool,
    /// The app's main window (the surface draws into pop-ups too).
    main: u32,
    /// Menus' pop-up windows ([`crate::popup`]).
    popups: crate::popup::Popups,
    /// The main window is a granted pop-up ([`WindowDesc::hosted`]).
    hosted: bool,
    /// The layout's natural minimum is to be measured at the next paint
    /// (at open, and when the text size or the width changes) and told
    /// to the WM (resolution-independence.md §6.3).
    min_due: bool,
    /// The minimum size last told, points.
    min_sent: (u32, u32),
    /// The app's own minimum (`WindowHints.min_size` at open): the natural
    /// one never goes below it.
    min_floor: (u32, u32),
    /// Where the last [`Window::paint`] spent its time.
    pub last_paint: PaintTiming,
}

/// The parts of one [`Window::paint`], in milliseconds: rendering and diffing the
/// tree, sending the frame (writes block while frescod is behind), and how many
/// deltas went out.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaintTiming {
    pub build_ms: f64,
    pub send_ms: f64,
    pub deltas: usize,
    /// Of the send: this thread's CPU time, the time in pop-up sync, and the
    /// involuntary context switches (preemptions) it suffered. Wall time far over
    /// CPU time with preemptions means the thread was waiting for a CPU, not for
    /// frescod.
    pub send_cpu_ms: f64,
    pub popups_ms: f64,
    pub send_preempted: i64,
}

/// This thread's CPU time (ms) and involuntary context switches.
fn thread_cpu() -> (f64, i64) {
    // SAFETY: plain out-parameters.
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_THREAD, &mut ru);
        (ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6, ru.ru_nivcsw as i64)
    }
}

/// Opens pop-ups for [`crate::popup::Popups`] on the window's connection.
struct PopupHost<'a> {
    conn: &'a mut Connection,
    main: u32,
    /// The main window's size (points): an anchor must lie inside it.
    bounds: (f32, f32),
    destroyed: Vec<u32>,
}

impl PopupHost<'_> {
    /// The anchor and size on the wire: points, as the layout has them
    /// (frescod turns them into device pixels at the parent's scale), the
    /// anchor inside the parent (a gap around a control at the edge is
    /// trimmed).
    fn wire(&self, spec: &crate::popup::Spec, rect: crate::geom::Rect) -> ([f32; 4], u32, u32) {
        let a = spec.anchor;
        let (bw, bh) = self.bounds;
        let x0 = a.x().clamp(0.0, bw);
        let y0 = a.y().clamp(0.0, bh);
        let x1 = (a.x() + a.w()).clamp(x0, bw);
        let y1 = (a.y() + a.h()).clamp(y0, bh);
        let size = |v: f32| (v.ceil() as u32).max(1);
        ([x0, y0, x1 - x0, y1 - y0], size(rect.w()), size(rect.h()))
    }
}

impl crate::popup::Host for PopupHost<'_> {
    fn create(&mut self, spec: &crate::popup::Spec, rect: crate::geom::Rect) -> io::Result<u32> {
        let (anchor, w, h) = self.wire(spec, rect);
        self.conn.popup_create(self.main, anchor, spec.side.wire(), spec.align.wire(), w, h, spec.grab)
    }
    fn place(&mut self, window: u32, spec: &crate::popup::Spec, rect: crate::geom::Rect) -> io::Result<()> {
        let (anchor, w, h) = self.wire(spec, rect);
        self.conn.popup_place(window, anchor, spec.side.wire(), spec.align.wire(), w, h)
    }
    fn destroy(&mut self, window: u32) {
        if let Err(e) = self.conn.window_destroy(window) {
            log::warn!("popup: destroy {window}: {e}");
        }
        self.destroyed.push(window);
    }
}

/// The theme for a system appearance.
pub fn theme_for(a: &fresco_protocol::AppearancePayload) -> crate::theme::Semantic {
    if a.scheme == fresco_protocol::APPEARANCE_DARK { crate::theme::Semantic::DARK } else { crate::theme::Semantic::LIGHT }
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
        let conn = Connection::connect(&socket)?;
        match WireMeasurer::connect(&socket) {
            Ok(m) => install_measurer(Box::new(m)),
            Err(e) => log::warn!("window: no text measurer ({e}); using estimates"),
        }
        Self::open_on(conn, desc)
    }

    /// Open the window on a connection the app was handed (a Limen
    /// child's embed-only connection, limen.md §2: it can't dial the
    /// socket). No text measurer is installed here: an app that lays
    /// out Pergola text installs one itself, or gets estimates.
    pub fn open_on(mut conn: Connection, desc: &WindowDesc) -> io::Result<Self> {

        // Points (resolution-independence.md §3): frescod sizes the
        // window's buffer at the scale it opens at.
        let (w, h) = match desc.size {
            SizeSpec::Fixed(w, h) => (w, h),
            SizeSpec::FullScreen => {
                let info = conn.display_info()?;
                if info.width == 0 || info.height == 0 {
                    log::warn!("window: display reports no mode; assuming 1280x720");
                    (1280, 720)
                } else {
                    let pt = |v: u32| fresco_protocol::device_to_pt(v as f32, info.scale_milli).floor() as u32;
                    (pt(info.width), pt(info.height))
                }
            }
        };

        let win = match desc.hosted {
            Some(token) => {
                // Drawn into from now on, as `window_create` does.
                let id = conn.popup_claim(token, w, h)?;
                conn.set_default_window(id as u16);
                id
            }
            None => conn.window_create(w, h, &desc.title, desc.hints.clone())?,
        };
        let appearance = match conn.appearance() {
            Ok(a) => {
                crate::text::set_text_scale(a.text_size_milli);
                Some(theme_for(&a))
            }
            Err(e) => {
                log::warn!("window: no system appearance ({e}); staying light");
                None
            }
        };

        Ok(Self {
            surface: FrescoSurface::new(conn, win),
            epoch: std::time::Instant::now(),
            width: w as f32,
            height: h as f32,
            binder: None,
            last_paint: PaintTiming::default(),
            embed_token: None,
            appearance,
            follow_appearance: true,
            main: win,
            popups: Default::default(),
            hosted: desc.hosted.is_some(),
            min_due: desc.hosted.is_none(),
            min_sent: desc.hints.min_size.unwrap_or((0, 0)),
            min_floor: desc.hints.min_size.unwrap_or((0, 0)),
        })
    }

    /// Keep the app's own theme instead of the system appearance (a
    /// preview, a test). On by default.
    pub fn follow_appearance(&mut self, on: bool) {
        self.follow_appearance = on;
    }

    /// Escape hatch for callers that need connection-level ops the
    /// loop doesn't cover yet.
    pub fn connection(&mut self) -> &mut Connection {
        self.surface.connection()
    }

    /// Paint one frame now (initial paint, or after out-of-loop state
    /// changes).
    pub fn paint<V: View>(&mut self, app: &mut App<V>) -> io::Result<()> {
        app.set_time(self.now_ms());
        if let Some(t) = self.appearance.take().filter(|_| self.follow_appearance) {
            app.set_theme(t);
        }
        app.set_popups(true);
        let min_due = std::mem::take(&mut self.min_due);
        if min_due {
            crate::layout::request_natural_min();
            app.invalidate();
        }
        let t0 = std::time::Instant::now();
        app.frame();
        let deltas = app.tick();
        let t1 = std::time::Instant::now();
        if min_due {
            self.send_min_size(app);
        }
        self.last_paint = PaintTiming { build_ms: (t1 - t0).as_secs_f64() * 1e3, deltas: deltas.len(), ..Default::default() };
        if !deltas.is_empty() {
            let (cpu0, sw0) = thread_cpu();
            // Pop-ups open (or move, or close) before what they show is sent.
            let mut host = PopupHost {
                conn: self.surface.connection(),
                main: self.main,
                bounds: (self.width, self.height),
                destroyed: Vec::new(),
            };
            let refused = self.popups.sync(app.tree(), &mut host);
            for w in std::mem::take(&mut host.destroyed) {
                self.surface.forget(w);
            }
            // Not allowed now (the window is not the active one): closed,
            // so the next press opens it rather than closing an unseen one.
            for root in refused {
                app.popup_dismissed(root);
            }
            self.last_paint.popups_ms = t1.elapsed().as_secs_f64() * 1e3;
            for (win, batch) in self.popups.route(deltas, app.tree(), self.main) {
                if batch.is_empty() {
                    continue;
                }
                self.surface.select(win);
                commit(&mut self.surface, &batch)?;
                self.surface.present()?;
            }
            self.surface.select(self.main);
            self.last_paint.send_ms = t1.elapsed().as_secs_f64() * 1e3;
            let (cpu1, sw1) = thread_cpu();
            self.last_paint.send_cpu_ms = cpu1 - cpu0;
            self.last_paint.send_preempted = sw1 - sw0;
        }
        // After the present, so a slot never sits over a frame that does
        // not yet show the space for it.
        let changes = app.take_slot_changes();
        if let Some(b) = self.binder.as_mut() {
            let (win, s) = (self.main, self.surface.scale);
            for (key, rect) in changes {
                let r = rect.map(|r| [r.x() * s, r.y() * s, r.w() * s, r.h() * s]);
                b.place(win, key, r);
            }
        }
        Ok(())
    }

    /// Tell the WM the layout's natural minimum, if it changed.
    fn send_min_size<V: View>(&mut self, app: &App<V>) {
        let Some(m) = app.tree().roots().next().and_then(crate::layout::take_natural_min) else { return };
        let side = |v: f32, floor: u32| (v.ceil().max(0.0) as u32).max(floor).min(fresco_protocol::MAX_MIN_SIZE_PT);
        let min = (side(m.w, self.min_floor.0), side(m.h, self.min_floor.1));
        if min != self.min_sent {
            self.min_sent = min;
            let hints = WindowHints { min_size: Some(min), ..Default::default() };
            if let Err(e) = self.surface.connection().window_set_hints(self.main, hints) {
                log::warn!("window: minimum size not sent: {e}");
            }
        }
    }

    /// frescod's embed token for this window: what limend names it by,
    /// for [`crate::embed::Embeds::request`] (as a parent) or
    /// `limen::client::Child::ready` (as a child). `None` until frescod
    /// has sent it, shortly after the window opens.
    pub fn embed_token(&self) -> Option<[u8; 16]> { self.embed_token }

    /// The window's id on the connection (for ops the loop does not
    /// cover, such as `popup_grant`).
    pub fn id(&self) -> u32 { self.main }

    /// Device pixels per point: the window's scale, from the monitor it
    /// is on.
    pub fn scale(&self) -> f32 { self.surface.scale }

    /// Install the binder that embeds other processes' surfaces in this
    /// window's slots ([`crate::slot`]).
    pub fn set_slot_binder(&mut self, b: Box<dyn crate::slot::SlotBinder>) {
        self.binder = Some(b);
    }

    /// Window lifecycle the toolkit handles itself: a resize re-lays
    /// out; a scale change rescales every node on the wire (layout stays
    /// in points, the surface multiplies at emission). frescod sends
    /// sizes and pointer positions in points already.
    fn window_event<V: View>(&mut self, ev: &fresco_client::Event, app: &mut App<V>) {
        let main = self.main;
        match *ev {
            // frescod took a menu down (a press outside, which it kept, or
            // focus elsewhere): close it as Escape would.
            fresco_client::Event::PopupDismissed { window_id, reason } => {
                if let Some(root) = self.popups.dismissed(window_id) {
                    log::debug!("popup {window_id} dismissed ({reason})");
                    app.popup_dismissed(root);
                }
            }
            fresco_client::Event::EmbedToken { window_id, token } if window_id == main => {
                self.embed_token = Some(token);
            }
            fresco_client::Event::Appearance(ref a) => {
                self.appearance = Some(theme_for(a));
                // The system text size: everything sized from text is laid
                // out again (resolution-independence.md §6).
                if crate::text::set_text_scale(a.text_size_milli) {
                    app.invalidate();
                    self.min_due = !self.hosted;
                }
            }
            fresco_client::Event::FocusChanged { window_id, gained: true, cause } if window_id == main => {
                // The focus ring shows only for keyboard focus (§14.1), and
                // arriving by Alt-Tab is keyboard focus.
                match cause {
                    fresco_protocol::focus_cause::KEYBOARD => app.set_focus_visible(true),
                    fresco_protocol::focus_cause::POINTER => app.set_focus_visible(false),
                    _ => {}
                }
            }
            // A granted pop-up may land smaller than asked.
            fresco_client::Event::PopupPlaced { window_id, width, height, .. }
                if window_id == main && (width, height) != (self.width, self.height) =>
            {
                self.width = width;
                self.height = height;
                app.invalidate();
            }
            fresco_client::Event::Resized { window_id, width, height, scale, .. } if window_id == main => {
                if scale.is_finite() && scale > 0.0 && scale != self.surface.scale {
                    self.surface.scale = scale;
                    app.resend_all();
                }
                if (width, height) != (self.width, self.height) {
                    self.min_due = !self.hosted && width != self.width;
                    self.width = width;
                    self.height = height;
                    app.invalidate();
                }
            }
            _ => {}
        }
    }

    /// A pop-up's pointer input, moved into the tree's (main window)
    /// coordinates, where its nodes are laid out.
    fn to_tree(&self, mut ev: fresco_client::Event) -> fresco_client::Event {
        use fresco_client::Event as E;
        match &mut ev {
            E::PointerMotion { window_id, x, y }
            | E::PointerButton { window_id, x, y, .. }
            | E::PointerMagnify { window_id, x, y, .. }
            | E::Drop { window_id, x, y, .. } => {
                if let Some((tx, ty)) = self.popups.to_tree(*window_id, *x, *y) {
                    (*x, *y) = (tx, ty);
                }
            }
            _ => {}
        }
        ev
    }

    /// Whether `ev` ends the window: frescod asks it to close or, for a
    /// granted pop-up, took it down (it is never shown again).
    fn closes(&self, ev: &fresco_client::Event) -> bool {
        match *ev {
            fresco_client::Event::CloseRequested { window_id } => window_id == self.main,
            fresco_client::Event::PopupDismissed { window_id, .. } => self.hosted && window_id == self.main,
            _ => false,
        }
    }

    /// Milliseconds on the app clock.
    fn now_ms(&self) -> f64 { self.epoch.elapsed().as_secs_f64() * 1e3 }

    /// How long to wait for an event: until the next frame while the app
    /// animates (client-driven animation, pergola.md §4.1), else `cap`.
    /// Frames are paced at 120 Hz; the server presents at its own vsync.
    fn wait_for<V: View>(&self, app: &App<V>, cap: Option<Duration>) -> Option<Duration> {
        // A socket timeout can't be zero: "now" is the shortest wait.
        const NOW: Duration = Duration::from_micros(100);
        self.wait_raw(app, cap).map(|d| d.max(NOW))
    }

    fn wait_raw<V: View>(&self, app: &App<V>, cap: Option<Duration>) -> Option<Duration> {
        if !app.wants_frame() {
            // A timer (a tooltip's delay) bounds the wait.
            let Some(at) = app.wake_at() else { return cap };
            let d = Duration::from_secs_f64((at - self.now_ms()).max(0.0) / 1e3);
            return Some(cap.map_or(d, |c| c.min(d)));
        }
        const FRAME: Duration = Duration::from_micros(8_333);
        let since = Duration::from_secs_f64((self.now_ms() - app.time()).max(0.0) / 1e3);
        let next = FRAME.saturating_sub(since);
        Some(cap.map_or(next, |c| c.min(next)))
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
        let wait = self.wait_for(app, timeout);
        let ev = self.surface.connection().wait_event(wait)?;
        app.set_time(self.now_ms());
        let result = match ev {
            Some(ev) => {
                let ev = self.to_tree(ev);
                if self.closes(&ev) {
                    return Ok(StepResult::Closed);
                }
                self.window_event(&ev, app);
                if let Some(pev) = translate(&ev) {
                    app.handle_event_activated(pev, activation_of(&ev));
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
            let wait = self.wait_for(app, None);
            let ev = match self.surface.connection().wait_event(wait)? {
                Some(ev) => ev,
                None => {
                    // A frame tick with no input.
                    self.paint(app)?;
                    continue;
                }
            };
            app.set_time(self.now_ms());
            let ev = self.to_tree(ev);

            let mut flow = Flow { exit: false, consumed: false };
            hook(&ev, app, &mut flow);
            if flow.exit {
                return Ok(());
            }

            if !flow.consumed {
                if self.closes(&ev) {
                    return Ok(());
                }
                self.window_event(&ev, app);
                if let Some(pev) = translate(&ev) {
                    app.handle_event_activated(pev, activation_of(&ev));
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

/// The activation token frescod minted for a press (tabula.md §2.1).
/// Only presses and drops carry one; an all-zero token means none (synthetic input).
fn activation_of(ev: &fresco_client::Event) -> Option<crate::clipboard::Token> {
    use fresco_client::Event;
    let tok = match ev {
        Event::Key { pressed: true, activation, .. } => *activation,
        Event::PointerButton { pressed: true, activation, .. } => *activation,
        Event::Drop { activation, .. } => *activation,
        _ => return None,
    };
    (tok != [0u8; 16]).then_some(tok)
}

