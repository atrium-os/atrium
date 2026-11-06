//! forum-wm — the Forum WM daemon (`docs/spec/forum.md`).
//!
//! Connects to frescod over the display socket (with the `window-management`
//! capability), then runs the reconcile loop the core defines:
//!
//!   enumerate the session's surfaces → arrange by role → declare the atomic
//!   layout → set per-surface rendering (gate the fully-occluded).
//!
//! This binary is the real I/O binding: `ClientConn` wraps a
//! `fresco_client::Connection` and implements the `FrescoConn` seam the pure
//! core (`lib.rs`) is written against. The placement/occlusion policy lives in
//! the lib and is unit-tested without a live frescod; here we just drive it.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forum_wm::daemon::{FrescoConn, Wm};
use forum_wm::{Screen, Snap};
use fresco_client::{Connection, Event};
use fresco_protocol::{WmDeclareLayoutPayload, WmRect, WmSetRenderingPayload, WmSurfaceInfo};

mod config;

/// The WM's shared mutable core: the policy state + the live frescod connection.
/// Two threads touch it — the forum-ctl server (chrome intents) and the input
/// poller (focus-follows-click) — so they serialize on this lock. The single
/// connection is fine: every reader (`wait_event`/`poll_event`) and request
/// (`enumerate`/`declare`) happens under the lock, so they never race on the
/// stream, and the one subscriber is drained by the poller every tick.
type Core = Arc<Mutex<(Wm, ClientConn)>>;

/// The switcher hotkey: Super(GUI)+Tab cycles document focus. HID usage 0x2B =
/// Tab (USB HID Usage Page 0x07); MOD_GUI = either GUI/Super modifier bit (left
/// 0x08 | right 0x80). Super rather than Alt so it doesn't collide with app Tab.
const KEY_TAB: u16 = 0x2B;
const MOD_GUI: u8 = 0x08 | 0x80;
/// Super+1..Super+N switches workspace. HID usages 0x1E..=0x27 are the digit
/// row '1'..'9','0'; we map '1'.. to workspace 0.. up to the workspace count.
const KEY_1: u16 = 0x1E;
/// Either Shift modifier bit (left 0x02 | right 0x20). Super+Shift+N moves the
/// focused window to workspace N (vs plain Super+N which switches to it).
const MOD_SHIFT: u8 = 0x02 | 0x20;
/// Super+S toggles the split (tiled) layout for the active workspace. HID 0x16 = 'S'.
const KEY_S: u16 = 0x16;
/// Super+F toggles zoom (fullscreen) for the focused surface. HID 0x09 = 'F'.
const KEY_F: u16 = 0x09;
/// Snap the focused document: Super+Left/Right snap to a half, Super+Up un-snaps.
/// HID arrow usages: Right 0x4F, Left 0x50, Up 0x52.
const KEY_RIGHT: u16 = 0x4F;
const KEY_LEFT: u16 = 0x50;
const KEY_UP: u16 = 0x52;

/// The live frescod connection — the production implementation of the seam the
/// reconcile loop drives. Each method is one window-management protocol op.
struct ClientConn {
    conn: Connection,
}

impl FrescoConn for ClientConn {
    fn enumerate(&mut self) -> io::Result<Vec<WmSurfaceInfo>> {
        self.conn.wm_enumerate()
    }
    fn declare_layout(&mut self, layout: &WmDeclareLayoutPayload) -> io::Result<()> {
        self.conn.wm_declare_layout(layout)
    }
    fn set_rendering(&mut self, decisions: &[WmSetRenderingPayload]) -> io::Result<()> {
        // The protocol marks one surface at a time; the WM batches the set.
        // Log the gated surfaces — this is the F1 engine tie made visible: a
        // fully-occluded surface stops compositing, its GPU work idles, and the
        // idle blocks let the GPU power-gate (forum.md §2.5).
        for d in decisions {
            if !d.rendering {
                eprintln!("forum-wm: render-gating surface {} (fully occluded)", d.surface_id);
            }
            self.conn.wm_set_rendering(d)?;
        }
        Ok(())
    }
    fn display_modes(&mut self) -> io::Result<fresco_protocol::WmDisplayModesReply> {
        self.conn.wm_display_modes()
    }
    fn set_display_mode(&mut self, width: u32, height: u32, refresh_mhz: u32) -> io::Result<()> {
        self.conn.wm_set_display_mode(width, height, refresh_mhz)
    }
    fn outputs(&mut self) -> io::Result<Vec<fresco_protocol::WmOutput>> {
        self.conn.wm_outputs().map(|r| r.outputs)
    }
    fn arrange_outputs(&mut self, placements: Vec<fresco_protocol::WmOutputPlacement>) -> io::Result<()> {
        self.conn.wm_arrange_outputs(placements)
    }
    fn set_output_scale(&mut self, identity: u64, scale_milli: u32) -> io::Result<()> {
        self.conn.wm_set_output_scale(identity, scale_milli)
    }
    fn set_appearance(&mut self, a: fresco_protocol::AppearancePayload) -> io::Result<()> {
        self.conn.wm_set_appearance(a)
    }
    fn mirror_output(&mut self, identity: u64, of: u64) -> io::Result<()> {
        self.conn.wm_mirror_output(identity, of)
    }
    fn span_group(&mut self, p: fresco_protocol::WmSpanGroupPayload) -> io::Result<fresco_protocol::WmRect> {
        self.conn.wm_span_group(p.window_id, p.members, p.gaps_x, p.gaps_y)
    }
}


/// The output geometry, ASKED of frescod (the mode query this comment used to
/// say was missing). The WM places every surface, so if it believes a different
/// screen than the compositor is scanning out, every window is laid out for a
/// screen that does not exist. FORUM_SCREEN still overrides, for driving a
/// layout the hardware isn't showing.
///
/// The chrome reservations (bar/dock) stay the shell's own: frescod reports the
/// mode, not what the session has reserved from it.
fn screen(conn: &mut Connection) -> Screen {
    let (w, h, top) = std::env::var("FORUM_SCREEN")
        .ok()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .map(|(w, h)| (w, h, 0))
        .or_else(|| match conn.display_info() {
            Ok(d) => screen_geometry(&d),
            Err(e) => { eprintln!("forum-wm: display_info failed: {e}"); None }
        })
        .unwrap_or((1920, 1080, 0));
    screen_from(w, h, top)
}

/// frescod's reported mode → (w, h, top rows reserved), or None if no mode.
fn screen_geometry(d: &fresco_protocol::DisplayInfoResponse) -> Option<(i32, i32, i32)> {
    if d.width == 0 || d.height == 0 {
        eprintln!("forum-wm: frescod reports no mode");
        return None;
    }
    eprintln!("forum-wm: screen {}x{} @ {} mHz, top {} rows frescod's (from frescod)",
              d.width, d.height, d.refresh_mhz, d.reserved_top);
    let top = (d.reserved_top as i32).min(d.height as i32 / 2);
    Some((d.width as i32, d.height as i32, top))
}

fn screen_from(w: i32, h: i32, top: i32) -> Screen {
    /* frescod's trusted strip (F4d) covers the top `top` rows: the session's
     * screen, bar included, starts below it. */
    Screen { rect: WmRect { x: 0, y: top, w, h: h - top }, bar_h: 24, dock_h: 48 }
}

/// Tell the user (Praeco, as a system poster: praecod's
/// `PRAECO_SYSTEM_POSTERS` lists Forum's uid), off the event loop.
fn post_notices(notices: Vec<(String, String)>) {
    if notices.is_empty() {
        return;
    }
    let sock = std::env::var_os("PRAECO_SOCKET").map_or_else(|| "/var/run/atrium/praecod.sock".into(), std::path::PathBuf::from);
    std::thread::spawn(move || {
        for (title, body) in notices {
            match praeco::client::post(&sock, praeco::proto::urgency::NORMAL, &title, &body) {
                Ok(id) => eprintln!("forum-wm: told the user: {title} (notification {id})"),
                Err(e) => eprintln!("forum-wm: could not tell the user \"{title}: {body}\" ({}: {e})", sock.display()),
            }
        }
    });
}

fn main() -> io::Result<()> {
    // The WM drives its cross-app ops over the DEDICATED window-management socket
    // (reachability = the window-management grant; frescod reads no policy), not
    // the shared client socket. See fresco_client::default_wm_socket_path.
    let path = fresco_client::default_wm_socket_path();
    eprintln!("forum-wm: connecting to frescod window-management socket at {}", path.display());
    let conn = Connection::connect(&path)?;
    // frescod now replies to a refused WM op with an IS_ERROR response, so a
    // denied enumerate surfaces a PermissionDenied immediately. This read timeout
    // is the secondary safety net for a genuinely unresponsive server (crash /
    // stall), so the reconcile can't block forever.
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut io_conn = ClientConn { conn };

    // Per-user workspace config (count / names / app rules); defaults if absent.
    let cfg = config::ForumConfig::load();
    let mut wm = Wm::new(screen(&mut io_conn.conn));
    wm.workspaces = cfg.workspaces;
    // The monitors (display 3.0); the first reconcile lays out on them.
    // and how the session last left them (arrangement per set, scales).
    let store = forum_wm::displays::default_path();
    wm.displays = forum_wm::displays::load(&store);
    wm.display_store = Some(store);
    match io_conn.conn.wm_outputs() {
        Ok(r) => {
            wm.outputs = r.outputs.clone();
            if let Err(e) = wm.apply_remembered_displays(&mut io_conn, &r.outputs, true) {
                eprintln!("forum-wm: restoring the monitors: {e}");
            }
        }
        Err(e) => eprintln!("forum-wm: outputs: {e} (one screen)"),
    }
    wm.names = cfg.names;
    wm.assign_rules = cfg.assign;
    // Overlay the learned placements (persisted manual moves) — they win over the
    // config defaults so "where I last put this app" survives reboot.
    for (app, ws) in config::load_state() {
        wm.assign_rules.insert(app, ws);
    }
    // The launch registry (uid → app-id) resolves a surface's owner_uid to its
    // app-id so per-app workspace rules apply. Reloaded on each new window
    // (it's append-only) to pick up apps launched after startup.
    wm.registry = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default();
    let layout = wm.reconcile(&mut io_conn)?;
    eprintln!(
        "forum-wm: declared layout — {} surface(s), focus={}",
        layout.slots.len(),
        layout.focus
    );

    if std::env::var("FORUM_WM_ONESHOT").is_ok() {
        // F0 behaviour: reconcile once and exit. No event loop, no daemon.
        return Ok(());
    }

    // Become the WM core daemon. Two concurrent drivers share the core:
    //
    //   - the INPUT POLLER reads frescod's event stream (the WM connection is a
    //     subscriber like any client) and applies focus-follows-click — a
    //     pointer press over an app surface moves keyboard focus there;
    //   - the FORUM-CTL SERVER answers the chrome apps' intents (dock/bar/
    //     overview) over forum-ctl, each re-driving Fresco through reconcile.
    //
    // Both re-derive the layout through the same `Wm`, so they must not run
    // concurrently against the one connection — `Core` (Arc<Mutex>) serializes
    // them. forum-ctl's path resolves to the canonical in-jail location (or
    // $FORUM_CTL_SOCKET / a dev fallback), so a jailed forum-wm serves the same
    // socket the jailed chrome apps reach through their shared
    // /atrium/sockets/forum-ctl/ mount.
    let core: Core = Arc::new(Mutex::new((wm, io_conn)));

    spawn_input_poller(Arc::clone(&core));

    let ctl = forum_ctl::default_socket_path();
    let ctl = ctl.to_string_lossy();
    eprintln!("forum-wm: serving forum-ctl at {ctl}");
    let r = serve_forum_ctl(&ctl, &core);
    remove_ctl_socket(); // also covers serve_forum_ctl failing after the bind
    r
}

/// The input poller — focus-follows-click. frescod broadcasts every input event
/// to every connection (no ownership filter), and tags each pointer-button event
/// with the hit-test target (the surface under the cursor), so the WM learns
/// which surface the human clicked just by reading its own event stream.
///
/// A short non-blocking poll keeps the core lock free almost all the time: we
/// grab it, drain whatever events are queued (so a burst of motion events can't
/// back up the socket), note the last primary-button press, release the lock,
/// then act on that press under a fresh lock. The forum-ctl server therefore
/// sees the lock contended only for the brief drain + an actual focus change.
/// Is this poll error the compositor going away, rather than a transient hiccup?
///
/// A dead frescod shows up as an EOF mid-message ("failed to fill whole buffer"
/// — `UnexpectedEof`) or a reset/closed pipe. Read timeouts (`WouldBlock`,
/// `TimedOut`) are the NORMAL idle case for a polling reader and must never
/// count here, or an idle session would shut its own WM down.
fn is_compositor_gone(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    )
}

fn spawn_input_poller(core: Core) {
    std::thread::Builder::new()
        .name("forum-wm-input".into())
        .spawn(move || loop {
            // Drain the event backlog under one short lock; keep the last
            // primary press (a later click supersedes an earlier one this tick)
            // and note whether the surface set changed (a window appeared or
            // disappeared — the WM must re-place the survivors).
            let mut clicked: Option<u32> = None;
            let mut surfaces_changed = false;
            let mut created: Option<u32> = None;
            let mut cycle = false;
            let mut switch_ws: Option<usize> = None;
            let mut move_ws: Option<usize> = None;
            let mut split = false;
            let mut zoom = false;
            let mut snap_dir: Option<Snap> = None;
            let mut unsnap = false;
            let mut next_monitor = false;
            let mut display: Option<fresco_protocol::DisplayInfoResponse> = None;
            {
                let mut g = core.lock().unwrap();
                loop {
                    match g.1.conn.poll_event() {
                        Ok(Some(Event::PointerButton {
                            window_id, pressed: true, button: 1, ..
                        })) if window_id != 0 => clicked = Some(window_id),
                        Ok(Some(Event::WindowCreated { window_id })) => {
                            surfaces_changed = true;
                            created = Some(window_id); // a new doc opens focused
                        }
                        Ok(Some(Event::WindowDestroyed { .. })) => surfaces_changed = true,
                        // The output changed (a mode switch or a monitor swap):
                        // keep the latest, re-lay out once below.
                        Ok(Some(Event::DisplayChanged(d))) => display = Some(d),
                        // The switcher hotkey (Super+Tab): cycle document focus.
                        // The WM sees every key (it's a subscriber); this one is
                        // a window-management gesture, not app text input.
                        Ok(Some(Event::Key { hid_usage: KEY_TAB, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 => cycle = true,
                        // Super+S: toggle split (tiled) layout for the active workspace.
                        Ok(Some(Event::Key { hid_usage: KEY_S, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 => split = true,
                        // Super+F: toggle zoom (fullscreen) for the focused surface.
                        Ok(Some(Event::Key { hid_usage: KEY_F, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 => zoom = true,
                        // Super+Left / Right: snap the focused document to a half.
                        // Super+Shift+Right: send the focused window to the next monitor
                        // (must precede the plain Super+Right snap arm).
                        Ok(Some(Event::Key { hid_usage: KEY_RIGHT, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 && modifiers & MOD_SHIFT != 0 => next_monitor = true,
                        Ok(Some(Event::Key { hid_usage: KEY_LEFT, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 => snap_dir = Some(Snap::Left),
                        Ok(Some(Event::Key { hid_usage: KEY_RIGHT, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 => snap_dir = Some(Snap::Right),
                        // Super+Up: un-snap the focused document.
                        Ok(Some(Event::Key { hid_usage: KEY_UP, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 => unsnap = true,
                        // Super+Shift+1..N: MOVE the focused window to workspace N
                        // (must precede the plain Super+N arm, which it also matches).
                        Ok(Some(Event::Key { hid_usage, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 && modifiers & MOD_SHIFT != 0
                                && hid_usage >= KEY_1 && hid_usage < KEY_1 + 9 =>
                            move_ws = Some((hid_usage - KEY_1) as usize),
                        // Super+1..N: switch the active workspace.
                        Ok(Some(Event::Key { hid_usage, pressed: true, modifiers, .. }))
                            if modifiers & MOD_GUI != 0 && modifiers & MOD_SHIFT == 0
                                && hid_usage >= KEY_1 && hid_usage < KEY_1 + 9 =>
                            switch_ws = Some((hid_usage - KEY_1) as usize),
                        Ok(Some(_)) => {}        // other event — ignore
                        Ok(None) => break,       // backlog drained
                        Err(ref e) if is_compositor_gone(e) => {
                            // frescod has gone away. Every later poll returns
                            // this same error, so treating it like a transient
                            // one means logging it every tick forever: measured
                            // at 2.9 KB/s (~250 MB/day) against a dead socket.
                            // Worse, the process stays up and keeps serving
                            // forum-ctl, so the chrome apps go on believing a
                            // working WM is there when it can no longer reach
                            // the compositor at all. Nothing here works without
                            // frescod — say so once and go.
                            eprintln!("forum-wm: compositor connection lost ({e}) — exiting");
                            remove_ctl_socket();
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("forum-wm: input poll error: {e}");
                            break;
                        }
                    }
                }
            }

            // A new screen size: re-place every surface for it, unless
            // FORUM_SCREEN pins the layout to a screen the hardware isn't showing.
            if let Some(d) = display {
                if std::env::var_os("FORUM_SCREEN").is_none() {
                    if let Some((w, h, top)) = screen_geometry(&d) {
                        let mut g = core.lock().unwrap();
                        let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                        match wm.set_screen(conn, screen_from(w, h, top)) {
                            Ok(Some(l)) => eprintln!(
                                "forum-wm: display now {w}x{h} — re-laid out {} surface(s)", l.slots.len()),
                            Ok(None) => {}       // same geometry (a refresh-only switch)
                            Err(e) => eprintln!("forum-wm: relayout on display change: {e}"),
                        }
                        // The monitors on it: windows follow their monitor,
                        // or are rescued to the primary while it is away.
                        match conn.outputs() {
                            Ok(outs) => {
                                let n = outs.iter().filter(|o| o.present).count();
                                match wm.set_outputs(conn, outs) {
                                    Ok(Some(l)) => eprintln!(
                                        "forum-wm: {n} monitor(s) — re-laid out {} surface(s)", l.slots.len()),
                                    Ok(None) => {}
                                    Err(e) => eprintln!("forum-wm: relayout on monitor change: {e}"),
                                }
                                post_notices(std::mem::take(&mut wm.notices));
                            }
                            Err(e) => eprintln!("forum-wm: outputs: {e}"),
                        }
                    }
                }
            }

            // A surface came or went → re-derive the whole layout so the new
            // window is placed (or the gone one's slot reclaimed). Do this
            // before applying any click, so focus-follows-click sees the
            // up-to-date surface set. A newly-created document opens focused on
            // the active workspace.
            if surfaces_changed {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                let r = match created {
                    Some(id) => {
                        // Refresh the registry so an app launched since startup
                        // resolves to its app-id (→ its configured workspace).
                        wm.registry = portcullis_peer::AppRegistry::load(
                            portcullis_peer::DEFAULT_REGISTRY,
                        )
                        .unwrap_or_default();
                        wm.focus_new(conn, id) // assigns by app rule, else active
                    }
                    None => wm.reconcile(conn),
                };
                match r {
                    Ok(l) => eprintln!(
                        "forum-wm: reconciled on surface change — {} active surface(s), focus={}",
                        l.slots.len(), l.focus,
                    ),
                    Err(e) => eprintln!("forum-wm: reconcile error: {e}"),
                }
            }

            if let Some(ws) = move_ws {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.move_focused_to_workspace(conn, ws) {
                    Ok(Some(s)) => {
                        eprintln!("forum-wm: moved surface {s} to {}", wm.workspace_label(ws));
                        // Persist the learned per-app placement (survives reboot).
                        config::save_state(&wm.assign_rules);
                    }
                    Ok(None) => {}               // nothing focused / out of range
                    Err(e) => eprintln!("forum-wm: move_focused_to_workspace error: {e}"),
                }
            }

            if let Some(ws) = switch_ws {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.switch_workspace(conn, ws) {
                    Ok(Some(w)) => eprintln!("forum-wm: switched to {}", wm.workspace_label(w)),
                    Ok(None) => {}               // out of range or already active
                    Err(e) => eprintln!("forum-wm: switch_workspace error: {e}"),
                }
            }

            if split {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.toggle_split(conn) {
                    Ok(t) => eprintln!("forum-wm: split {} for active workspace", if t { "ON (tiled)" } else { "OFF (stacked)" }),
                    Err(e) => eprintln!("forum-wm: toggle_split error: {e}"),
                }
            }

            if zoom {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.toggle_zoom(conn) {
                    Ok(z) => eprintln!("forum-wm: zoom {}", if z { "ON (fullscreen)" } else { "OFF" }),
                    Err(e) => eprintln!("forum-wm: toggle_zoom error: {e}"),
                }
            }

            if let Some(d) = snap_dir {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.snap_focused(conn, d) {
                    Ok(Some(s)) => eprintln!("forum-wm: snapped surface {s} {d:?}"),
                    Ok(None) => {}               // nothing focusable to snap
                    Err(e) => eprintln!("forum-wm: snap_focused error: {e}"),
                }
            }

            if next_monitor {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.move_focused_to_next_monitor(conn) {
                    Ok(Some(m)) => eprintln!("forum-wm: window sent to monitor {m:016x}"),
                    Ok(None) => {}
                    Err(e) => eprintln!("forum-wm: move to monitor: {e}"),
                }
            }

            if unsnap {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.unsnap_focused(conn) {
                    Ok(Some(s)) => eprintln!("forum-wm: un-snapped surface {s}"),
                    Ok(None) => {}               // wasn't snapped
                    Err(e) => eprintln!("forum-wm: unsnap_focused error: {e}"),
                }
            }

            if cycle {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.cycle_focus(conn) {
                    Ok(Some(f)) => eprintln!("forum-wm: switcher → focus surface {f}"),
                    Ok(None) => {}               // <2 documents — nothing to cycle
                    Err(e) => eprintln!("forum-wm: cycle_focus error: {e}"),
                }
            }

            if let Some(w) = clicked {
                let mut g = core.lock().unwrap();
                let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
                match wm.focus_click(conn, w) {
                    Ok(Some(f)) => eprintln!("forum-wm: focus-follows-click → surface {f}"),
                    Ok(None) => {}               // click didn't change focus
                    Err(e) => eprintln!("forum-wm: focus_click error: {e}"),
                }
            }

            std::thread::sleep(Duration::from_millis(16)); // ~60 Hz, lock released
        })
        .expect("spawn forum-wm input poller");
}

/// Serve forum-ctl: accept chrome connections, carry out one intent each against
/// Fresco.
///
/// ADMISSION = REACHABILITY (object-capability; minimal trust). We do NOT read
/// the owner's policy or the registry to authorize a peer. `forum-control` is
/// enforced by the TCB at the JAIL BOUNDARY: a chrome app can only see + connect
/// to this socket because Portcullis mounted `/atrium/sockets/forum-ctl/` into
/// its jail (`apply_forum_control`, gated by the manifest cap + the owner's
/// launch-time policy grant). An app without the grant has no mount → cannot
/// reach us. So a connection on this socket IS the capability — possession of the
/// channel, not a lookup. forum-wm re-checking a policy would only duplicate (and
/// widen the trust of) a decision the TCB already made + enforced. See docs/spec/
/// portcullis.md §9.0. (Dev `--no-prompt` launches skip the launch-time policy
/// gate — an explicit operator trust decision, not a hole here.)
/// The forum-ctl socket we bound, remembered so every exit path can unlink it:
/// the path as a C string (a signal handler may not allocate), plus the
/// (dev, ino) it had at bind time.
///
/// The identity check matters. `serve_forum_ctl` unlinks a stale socket before
/// binding, so a second forum-wm displaces the first's file — without it, the
/// displaced process would delete the NEW server's socket on its way out and
/// leave a live daemon nobody can reach. We only remove the file if it is still
/// the one we created.
static CTL_SOCKET: std::sync::OnceLock<(std::ffi::CString, u64, u64)> = std::sync::OnceLock::new();

/// Unlink our forum-ctl socket if the path still refers to it. Allocation-free
/// and async-signal-safe (`stat`/`unlink` on a pre-built C string), so the same
/// function serves the normal return, the fatal-error exit, and the signal
/// handler.
fn remove_ctl_socket() {
    let Some((path, dev, ino)) = CTL_SOCKET.get() else { return };
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(path.as_ptr(), &mut st) == 0
            && st.st_dev as u64 == *dev
            && st.st_ino as u64 == *ino
        {
            libc::unlink(path.as_ptr());
        }
    }
}

/// SIGTERM/SIGINT/SIGHUP: `pkill forum-wm` is how this daemon is stopped, and
/// the default disposition would leave the socket file behind. Clean up, then
/// `_exit` — the only async-signal-safe way out.
extern "C" fn on_terminate(sig: libc::c_int) {
    remove_ctl_socket();
    unsafe { libc::_exit(128 + sig) };
}

fn serve_forum_ctl(path: &str, core: &Core) -> io::Result<()> {
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Remember what we just bound, then arrange to remove it on the way out.
    if let Ok(c) = std::ffi::CString::new(path) {
        if let Ok(md) = std::fs::metadata(path) {
            use std::os::unix::fs::MetadataExt;
            let _ = CTL_SOCKET.set((c, md.dev() as u64, md.ino() as u64));
        }
    }
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(sig, on_terminate as libc::sighandler_t);
        }
    }
    // 0666 so the chrome apps (each its own per-app uid) can connect to this
    // socket served by forum-wm (yet another uid). Reachability is the gate: the
    // TCB only mounts /atrium/sockets/forum-ctl/ into a jail holding the
    // `forum-control` cap, so an app without it can't see this socket at all —
    // same pattern as frescod's / portcullisd's sockets. Without it a uid-50001
    // chrome app gets EACCES on forum-wm's uid-50000 socket.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }
    eprintln!("forum-wm: serving forum-ctl on {path}");
    for stream in listener.incoming() {
        let mut s = match stream { Ok(s) => s, Err(_) => continue };
        if let Some(uid) = peer_uid(&s) {
            eprintln!("forum-wm: forum-ctl: chrome connected (uid {uid})");
        }
        let intent = match forum_ctl::read_frame(&mut s)
            .ok()
            .and_then(|b| forum_ctl::decode::<forum_ctl::Intent>(&b).ok())
        {
            Some(i) => i,
            None => continue,
        };
        // Carry out the intent under the core lock — serialized against the
        // input poller's focus-follows-click so the two never drive the one
        // connection at once.
        let reply = {
            let mut g = core.lock().unwrap();
            let (wm, conn) = { let c = &mut *g; (&mut c.0, &mut c.1) };
            forum_wm::control::handle_intent(wm, conn, intent)
        };
        if let Ok(bytes) = forum_ctl::encode(&reply) {
            let _ = forum_ctl::write_frame(&mut s, &bytes);
        }
    }
    Ok(())
}

/// The connecting peer's uid (getpeereid over the UDS) — for the log line only;
/// admission does not depend on it (see `serve_forum_ctl`).
fn peer_uid(s: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0u32, 0u32);
    let rc = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dangerous direction is the false positive: a read timeout is the
    /// NORMAL idle case for the polling reader, so classifying it as "gone"
    /// would make an idle session shut its own WM down after five seconds.
    #[test]
    fn read_timeouts_are_not_the_compositor_going_away() {
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut, io::ErrorKind::Interrupted] {
            assert!(
                !is_compositor_gone(&io::Error::new(kind, "poll tick")),
                "{kind:?} must not be treated as a dead compositor"
            );
        }
    }

    /// The other direction: a dead frescod surfaces as an EOF mid-message
    /// ("failed to fill whole buffer") or a reset pipe. Each must be fatal, or
    /// the poller logs it every tick forever — measured at 2.9 KB/s.
    #[test]
    fn eof_and_reset_are_the_compositor_going_away() {
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
        ] {
            assert!(
                is_compositor_gone(&io::Error::new(kind, "frescod gone")),
                "{kind:?} must end the poller"
            );
        }
    }

    /// read_exact's own EOF error is the exact one seen in the VM; assert on the
    /// real constructor rather than a hand-made ErrorKind.
    #[test]
    fn read_exact_eof_is_recognised() {
        use std::io::Read;
        let mut buf = [0u8; 8];
        let err = (&[1u8, 2, 3][..]).read_exact(&mut buf).unwrap_err();
        assert_eq!(err.to_string(), "failed to fill whole buffer");
        assert!(is_compositor_gone(&err));
    }
}
