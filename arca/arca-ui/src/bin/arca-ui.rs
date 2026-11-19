//! arca-ui: the Limen `credential` provider (arca-autofill.md §8).
//!
//! One process serves every credential field on the screen. For each
//! instance limend attaches, it opens an `embed_pending` window (frescod
//! shows it only once limend binds it into the parent as a credential
//! slot), runs a [`Field`] in it, and talks to arcad as the trusted UI.
//!
//! Environment (defaults in brackets):
//!   LIMEN_SOCKET          [/var/run/atrium/limend.sock]
//!   FRESCO_SOCKET         [fresco-client's default]
//!   FRESCO_ACTIVATION     [/var/run/atrium/fresco-activation.sock]
//!   ARCA_SOCKET           [/atrium/sockets/arca/arcad.sock]
//!   ARCA_APP_REGISTRY     [/var/run/atrium/app-registry]
//!   ARCA_ALLOW_UNSIGNED   [unset; dev only, as for arcad]

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use arca_ui::arca::{Arcad, Parent};
use arca_ui::field::{Do, Field, Notice, Offer};
use arca_ui::gate::{Frescod, Gate};
use arca_ui::{keymap, view};
use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;
use limen::client::Child;
use limen::credential::{Purpose, ToChild, ToParent};
use limen::proto::{ChildEvent, EmbedId};

/// Until frescod's FocusChanged says pointer or keyboard, a focus this soon
/// after a press in the window is taken as a click.
const RING_AFTER_PRESS: Duration = Duration::from_millis(100);
/// How long the chip must have been on screen for a press to hit it.
const CHIP_SEEN: Duration = Duration::from_millis(100);

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

struct PrinterBinding {
    user: String,
    persona: String,
    key: [u8; 32],
    uuid: String,
}

struct Embed {
    win: Connection,
    window_id: u32,
    font: view::Font,
    parent: Option<Parent>,
    print_sheet: bool,
    printer: Option<PrinterBinding>,
    field: Field,
    /// The slot's size in points.
    size: (f32, f32),
    scale: view::Scale,
    look: view::Look,
    /// The chip as last drawn (pt), and since when it has been: a press
    /// can only hit a chip that was on screen (CHIP_SEEN) before it.
    /// frescod's focus event comes just before the press that focused, and
    /// the chip it brings may cover where the user pressed (all of a
    /// narrow field at a large text size).
    shown_chip: Option<(view::Rect, Instant)>,
    /// The last pointer press in this window: a focus that follows one
    /// within RING_AFTER_PRESS came by click and shows no ring.
    pressed_at: Option<Instant>,
    dirty: bool,
}

struct Ctx {
    child: Child,
    arcad: Arcad,
    gate: Frescod,
    me: u32,
}

impl Ctx {
    fn run(&mut self, id: EmbedId, e: &mut Embed, dos: Vec<Do>) {
        let mut queue: std::collections::VecDeque<Do> = dos.into();
        while let Some(d) = queue.pop_front() {
            let more = match d {
                Do::Redraw => {
                    e.dirty = true;
                    Vec::new()
                }
                Do::Emit(m) => {
                    if let Err(err) = self.child.emit(id, m.encode()) {
                        eprintln!("arca-ui: emit to embed {id}: {err:?}");
                    }
                    Vec::new()
                }
                Do::FocusOut { forward } => {
                    let _ = self.child.focus_out(id, forward);
                    Vec::new()
                }
                Do::Drop(h) => {
                    let _ = self.arcad.drop_handle(&h);
                    Vec::new()
                }
                Do::Hold => match &e.parent {
                    None => e.field.refused(Notice::UnknownApp),
                    Some(p) => match self.arcad.hold(p, e.field.typed(), arca_ui::field::hold_tag(e.field.purpose)) {
                        Ok(h) => {
                            eprintln!("arca-ui: held what was typed");
                            e.field.held(h)
                        }
                        Err(err) => {
                            eprintln!("arca-ui: hold refused: {err:?}");
                            e.field.refused(if err.locked() { Notice::Locked } else { Notice::Failed })
                        }
                    },
                },
                Do::Fill { offer, token } => self.fill(e, offer, token),
            };
            queue.extend(more);
        }
    }

    fn submit_print_pin(&mut self, id: EmbedId, e: &mut Embed, request: ToChild) {
        let Some(binding) = &e.printer else { return };
        let saved = matches!(request, ToChild::UseSavedPrintPin);
        let typed = (!saved).then(|| e.field.typed());
        let result = self.arcad.print_pin(&binding.user, &binding.persona, &binding.key, &binding.uuid,
            typed, matches!(request, ToChild::SavePrintPin))
            .map_err(|err| if err.locked() { Notice::Locked } else { Notice::Failed });
        match result {
            Ok(handle) => {
                e.field.clear_print_secret();
                // The print sheet now owns the grant. Detach must allow the queued worker to redeem it.
                if self.child.emit(id, ToParent::Handle { handle }.encode()).is_err() {
                    let _ = self.arcad.drop_handle(&handle);
                }
            }
            Err(notice) => {
                e.field.notice = Some(notice);
                let _ = self.child.emit(id, ToParent::Cleared.encode());
            }
        }
        e.dirty = true;
    }

    fn save_print_login(&mut self, id: EmbedId, e: &mut Embed, account: &str) {
        let Some(binding) = &e.printer else { return };
        let result = self.arcad.store_print_login(&binding.user, &binding.persona, &binding.key, &binding.uuid, account, e.field.typed());
        let event = match result {
            Ok(()) => {
                e.field.clear_print_secret();
                ToParent::PrintLoginSaved
            }
            Err(error) => {
                e.field.notice = Some(if error.locked() { Notice::Locked } else { Notice::Failed });
                ToParent::Cleared
            }
        };
        let _ = self.child.emit(id, event.encode());
        e.dirty = true;
    }

    /// The press first, then the handle: arcad is asked for nothing until
    /// frescod has vouched the user pressed this field while it was seen.
    fn fill(&mut self, e: &mut Embed, offer: Offer, token: [u8; 16]) -> Vec<Do> {
        let Some(p) = e.parent.clone() else { return e.field.refused(Notice::UnknownApp) };
        match self.gate.fill(token, self.me) {
            Ok(w) if w == e.window_id => {}
            Ok(_) => return e.field.refused(Notice::NoPress),
            Err(n) => return e.field.refused(n),
        }
        let h = match offer {
            Offer::Account(i) => match e.field.accounts.get(i) {
                Some(a) => self.arcad.handle_for(&p, &a.id),
                None => return e.field.refused(Notice::Failed),
            },
            Offer::Generate => self.arcad.generate(&p),
        };
        match h {
            Ok(h) => e.field.filled(offer, h),
            Err(err) => e.field.refused(if err.locked() { Notice::Locked } else { Notice::Failed }),
        }
    }

    fn refresh_accounts(&mut self, e: &mut Embed) {
        let Some(p) = &e.parent else { return };
        match self.arcad.accounts(p) {
            Ok(a) => {
                e.field.set_accounts(a);
            }
            Err(err) if err.locked() => e.field.notice = Some(Notice::Locked),
            Err(_) => {}
        }
        e.dirty = true;
    }
}

/// A window frescod keeps hidden until limend binds it, and its embed token.
fn open_window(sock: &str) -> std::io::Result<(Connection, u32, [u8; 16])> {
    let mut c = Connection::connect(sock)?;
    let id = c.window_create(240, 36, "Arca", WindowHints { embed_pending: true, ..Default::default() })?;
    c.set_default_window(id as u16);
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Some(Event::EmbedToken { token, .. }) = c.wait_event(Some(Duration::from_millis(100)))? {
            return Ok((c, id, token));
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no embed token from frescod"))
}

fn main() -> ExitCode {
    // Typed passwords live in mlocked pages; no core, no tracing.
    if let Err(e) = arca_store::secret::harden_process() {
        eprintln!("arca-ui: harden: {e}");
        return ExitCode::FAILURE;
    }
    let fresco = std::env::var("FRESCO_SOCKET")
        .unwrap_or_else(|_| fresco_client::default_socket_path().to_string_lossy().into_owned());
    let registry = env("ARCA_APP_REGISTRY", "/var/run/atrium/app-registry");
    let prelum_uid = std::env::var("ARCA_PRELUM_UID").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(65046);
    let allow_unsigned = std::env::var_os("ARCA_ALLOW_UNSIGNED").is_some();
    let child = match Child::provider(env("LIMEN_SOCKET", limen::client::DEFAULT_SOCKET), "credential") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("arca-ui: limend: {e:?}");
            return ExitCode::FAILURE;
        }
    };
    let mut cx = Ctx {
        child,
        arcad: Arcad::new(env("ARCA_SOCKET", "/atrium/sockets/arca/arcad.sock")),
        gate: Frescod::new(env("FRESCO_ACTIVATION", limen::fresco::DEFAULT_SOCKET)),
        // SAFETY: getuid cannot fail.
        me: unsafe { libc::getuid() },
    };
    let mut embeds: HashMap<EmbedId, Embed> = HashMap::new();
    eprintln!("arca-ui: providing credential");

    loop {
        let wait = if embeds.is_empty() { 500 } else { 10 };
        match cx.child.poll(Duration::from_millis(wait)) {
            Ok(Some(ev)) => match ev {
                ChildEvent::Attach { embed, role, parent_app, parent_uid } => {
                    if role != "credential" {
                        continue;
                    }
                    let (mut win, window_id, token) = match open_window(&fresco) {
                        Ok(w) => w,
                        Err(e) => {
                            eprintln!("arca-ui: window for embed {embed}: {e}");
                            continue;
                        }
                    };
                    let font = view::Font::open(&mut win);
                    let scale = view::Scale { k: win.window_scale(window_id), text: win.appearance().map(|a| a.text_scale()).unwrap_or(1.0) };
                    // The parent as the registry has it, by the uid limend
                    // read off its connection; `parent_app` is only a check.
                    let parent = Parent::resolve(&registry, parent_uid, allow_unsigned)
                        .filter(|p| p.app_id == parent_app);
                    let mut field = match Field::new() {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("arca-ui: field: {e}");
                            continue;
                        }
                    };
                    let print_sheet = parent_uid == prelum_uid && parent_app == limen::catalogue::PRELUM;
                    if parent.is_none() && !print_sheet {
                        field.notice = Some(Notice::UnknownApp);
                    }
                    let mut e = Embed { win, window_id, font, parent, print_sheet, printer: None, field, size: (240.0, 36.0), scale, look: view::Look::default(), shown_chip: None, pressed_at: None, dirty: true };
                    if let Err(err) = cx.child.ready(embed, token) {
                        eprintln!("arca-ui: ready {embed}: {err:?}");
                        let _ = e.win.window_destroy(window_id);
                        continue;
                    }
                    cx.refresh_accounts(&mut e);
                    embeds.insert(embed, e);
                }
                ChildEvent::Message { embed, bytes } => {
                    let Some(e) = embeds.get_mut(&embed) else { continue };
                    match postcard::from_bytes(&bytes) {
                        Ok(ToChild::Configure { purpose, username_field }) if !e.print_sheet && !matches!(purpose, Purpose::PrintPin | Purpose::PrintPassword) => {
                            let dos = e.field.configure(purpose, username_field);
                            cx.run(embed, e, dos);
                            cx.refresh_accounts(e);
                        }
                        Ok(ToChild::ConfigurePrintPin { user, persona, key, uuid }) if e.print_sheet && e.printer.is_none() => {
                            e.printer = Some(PrinterBinding { user, persona, key, uuid });
                            let dos = e.field.configure(Purpose::PrintPin, false);
                            cx.run(embed, e, dos);
                        }
                        Ok(request @ (ToChild::SubmitPrintPin | ToChild::SavePrintPin | ToChild::UseSavedPrintPin))
                            if e.print_sheet && e.field.purpose == Purpose::PrintPin => cx.submit_print_pin(embed, e, request),
                        Ok(ToChild::ConfigurePrintLogin { user, persona, key, uuid }) if e.print_sheet && e.printer.is_none() => {
                            e.printer = Some(PrinterBinding { user, persona, key, uuid });
                            let dos = e.field.configure(Purpose::PrintPassword, false);
                            cx.run(embed, e, dos);
                        }
                        Ok(ToChild::SavePrintLogin { account }) if e.print_sheet && e.field.purpose == Purpose::PrintPassword => cx.save_print_login(embed, e, &account),
                        _ => {}
                    }
                }
                ChildEvent::Suspended { embed } => {
                    if let Some(e) = embeds.get_mut(&embed) {
                        let dos = e.field.focus(false);
                        cx.run(embed, e, dos);
                    }
                }
                ChildEvent::Detaching { embed } => {
                    if let Some(mut e) = embeds.remove(&embed) {
                        let dos = e.field.detach();
                        cx.run(embed, &mut e, dos);
                        let _ = e.win.window_destroy(e.window_id);
                    }
                }
                // Resumed, and events for other roles (AudioTicket).
                _ => {}
            },
            Ok(None) => {}
            Err(e) => {
                eprintln!("arca-ui: limend: {e:?}");
                return ExitCode::FAILURE;
            }
        }

        let ids: Vec<EmbedId> = embeds.keys().copied().collect();
        for id in ids {
            let mut gone = false;
            let e = embeds.get_mut(&id).unwrap();
            loop {
                let ev = match e.win.poll_event() {
                    // Laid out in points (resolution-independence.md §3).
                    Ok(Some(ev)) => ev,
                    Ok(None) => break,
                    Err(_) => {
                        gone = true;
                        break;
                    }
                };
                let dos = match ev {
                    Event::Resized { width, height, scale, .. } => {
                        e.size = (width, height);
                        if scale > 0.0 {
                            e.scale.k = scale;
                        }
                        vec![Do::Redraw]
                    }
                    Event::Appearance(a) if a.text_scale() != e.scale.text => {
                        e.scale.text = a.text_scale();
                        vec![Do::Redraw]
                    }
                    Event::PointerMotion { .. } if !e.look.hover => {
                        e.look.hover = true;
                        vec![Do::Redraw]
                    }
                    Event::FocusChanged { gained, .. } => {
                        eprintln!("arca-ui: focus {}", if gained { "gained" } else { "lost" });
                        e.look.hover = false;
                        e.look.ring = gained && e.pressed_at.is_none_or(|t| t.elapsed() > RING_AFTER_PRESS);
                        if gained {
                            cx.refresh_accounts(e);
                        }
                        e.field.focus(gained)
                    }
                    Event::Key { hid_usage, pressed: true, modifiers, activation, .. } => {
                        // Typing in the field is keyboard use: the ring returns.
                        if !e.look.ring {
                            e.look.ring = true;
                            e.dirty = true;
                        }
                        e.field.key(keymap::decode(hid_usage, modifiers), activation)
                    }
                    Event::PointerButton { x, y, pressed: true, activation, .. } => {
                        e.pressed_at = Some(Instant::now());
                        e.look.ring = false;
                        e.dirty = true;
                        if e.shown_chip.is_some_and(|(c, t)| c.contains(x, y) && t.elapsed() >= CHIP_SEEN) {
                            e.field.press_chip(activation)
                        } else if !e.field.focused {
                            // A click in the field shows the chip even if
                            // frescod's focus event is late; it never fills.
                            cx.refresh_accounts(e);
                            e.field.focus(true)
                        } else {
                            Vec::new()
                        }
                    }
                    _ => Vec::new(),
                };
                cx.run(id, e, dos);
            }
            if gone {
                // frescod went away: void the handle; limend detaches us.
                let mut e = embeds.remove(&id).unwrap();
                let dos = e.field.detach();
                cx.run(id, &mut e, dos);
                continue;
            }
            if e.dirty {
                e.dirty = false;
                let who = e.parent.as_ref().map_or("unknown app", |p| p.app_id.as_str()).to_string();
                match view::draw(&mut e.win, e.font, &e.field, &who, e.look, e.size.0, e.size.1, e.scale) {
                    Ok(()) => {
                        let chip = view::layout(&e.field, e.size.0, e.size.1, e.scale.text).chip;
                        e.shown_chip = match (chip, e.shown_chip) {
                            (Some(c), Some((old, t))) if old == c => Some((c, t)),
                            (c, _) => c.map(|c| (c, Instant::now())),
                        };
                    }
                    Err(err) => eprintln!("arca-ui: draw {id}: {err}"),
                }
            }
        }
    }
}
