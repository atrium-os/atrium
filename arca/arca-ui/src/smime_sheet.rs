//! The trusted S/MIME provisioning sheet. Only this process receives the
//! picked files, and only arcad gets the private-key bytes.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use arca_store::{model::caps, secret::LockedBuf};
use fresco_client::{Connection, Event};
use limen::client::{Child, Parent};
use limen::picker::{Filter, ToChild, ToParent};
use limen::proto::{ChildEvent, EmbedId, ParentEvent, WindowToken};
use limen::smime_provision::Outcome;
use pergola::theme::Semantic;
use portcullis_peer::AppRegistry;

use crate::arca::{Arcad, Error};
use crate::gate::Frescod;
use crate::smime::read_checked_der;
use crate::view::{Font, Px, Rect};

const ROLE: &str = "smime-provision";
const WIDTH: u32 = 680;
const HEIGHT: u32 = 520;
const POLL: Duration = Duration::from_millis(12);

#[derive(PartialEq, Eq)]
struct Identity {
    user: String,
    publisher: [u8; 32],
    persona: Option<String>,
}

impl Identity {
    fn resolve(registry: &str, uid: u32) -> Option<Self> {
        let registry = AppRegistry::load(registry).ok()?;
        let (user, app) = registry.resolve(uid)?;
        let publisher = registry.publisher(uid)??;
        if app != "org.atrium.mail" || publisher == arcad::label::UNSIGNED_PUBLISHER {
            return None;
        }
        Some(Self {
            user: user.into(),
            publisher,
            persona: registry.persona(uid)?.map(str::to_owned),
        })
    }
}

#[derive(Clone, Copy)]
enum Input {
    Certificate,
    Key,
}

impl Input {
    fn title(self) -> &'static str {
        match self {
            Self::Certificate => "Choose DER certificate",
            Self::Key => "Choose PKCS#8 private key",
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::Certificate => arcad::proto::SMIME_CERT_MAX,
            Self::Key => caps::VALUE,
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Pick(Input),
    Install,
    Cancel,
}

struct Pick {
    embed: EmbedId,
    input: Input,
    fd: Option<OwnedFd>,
}

struct Sheet {
    win: Connection,
    window: u32,
    token: WindowToken,
    owner: u32,
    identity: Identity,
    scoped: bool,
    certificate: Option<LockedBuf>,
    key: Option<LockedBuf>,
    pick: Option<Pick>,
    note: &'static str,
    font: Font,
    focus: usize,
    width: f32,
    height: f32,
    scale: f32,
    text_scale: f32,
    dark: bool,
    dirty: bool,
    finished: bool,
}

type OpenWindow = fn(&str, u32, u32, &str) -> io::Result<(Connection, u32, WindowToken)>;

impl Sheet {
    fn open(
        fresco: &str,
        open_window: OpenWindow,
        owner: u32,
        identity: Identity,
    ) -> io::Result<Self> {
        let (mut win, window, token) =
            open_window(fresco, WIDTH, HEIGHT, "Arca · Add S/MIME identity")?;
        let font = Font::open(&mut win);
        let scale = win.window_scale(window);
        let appearance = win.appearance().ok();
        Ok(Self {
            win,
            window,
            token,
            owner,
            identity,
            font,
            scale,
            scoped: false,
            certificate: None,
            key: None,
            pick: None,
            width: WIDTH as f32,
            height: HEIGHT as f32,
            focus: 0,
            note: "",
            text_scale: appearance.as_ref().map_or(1.0, |a| a.text_scale()),
            dark: appearance.is_some_and(|a| a.scheme == fresco_protocol::APPEARANCE_DARK),
            dirty: true,
            finished: false,
        })
    }

    fn clear(&mut self, picker: &mut Parent) {
        if let Some(pick) = self.pick.take() {
            let _ = picker.detach(pick.embed);
        }
        self.key = None;
        self.certificate = None;
    }

    fn can_install(&self) -> bool {
        self.scoped && self.certificate.is_some() && self.key.is_some()
    }

    fn actions(&self) -> [(Action, Rect, &'static str, bool); 4] {
        let x = 22.0;
        let height = self.height;
        let r = |x, y, w| Rect { x, y, w, h: 43.0 };
        let row_w = (self.width - 2.0 * x).max(1.0);
        let footer_w = ((row_w - 12.0) / 2.0).min(220.0);
        [
            (
                Action::Pick(Input::Certificate),
                r(x, height * 0.35, row_w),
                Input::Certificate.title(),
                self.scoped,
            ),
            (
                Action::Pick(Input::Key),
                r(x, height * 0.51, row_w),
                Input::Key.title(),
                self.scoped,
            ),
            (
                Action::Install,
                r(x + footer_w + 12.0, height - 63.0, footer_w),
                "Add identity",
                self.can_install(),
            ),
            (
                Action::Cancel,
                r(x, height - 63.0, footer_w),
                "Cancel",
                true,
            ),
        ]
    }

    fn draw(&mut self) -> io::Result<()> {
        let theme = if self.dark {
            Semantic::DARK
        } else {
            Semantic::LIGHT
        };
        let actions = self.actions();
        let mut frame = self.win.frame()?;
        let mut px = Px {
            fr: &mut frame,
            k: self.scale,
        };
        let f = self.font;
        let body = (13.0 * self.text_scale).min(self.width / 38.0);
        let title = (20.0 * self.text_scale).min(self.width / 18.0);
        px.rect(0.0, 0.0, self.width, self.height, theme.bg_window(), 0.0)?;
        px.text(
            f,
            title,
            22.0,
            self.height * 0.14,
            theme.text_primary(),
            "Add S/MIME identity".into(),
        )?;
        let persona = self.identity.persona.as_deref().map_or_else(
            || "Personal".to_owned(),
            |name| {
                let mut chars = name.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            },
        );
        px.text(
            f,
            body,
            22.0,
            self.height * 0.23,
            theme.text_secondary(),
            format!("Arca will store this identity in {persona}."),
        )?;
        for (i, (_, r, label, enabled)) in actions.into_iter().enumerate() {
            let primary = i == 2 && enabled;
            let focused = self.focus == i && enabled;
            px.rect(
                r.x,
                r.y,
                r.w,
                r.h,
                if focused {
                    theme.accent_fg()
                } else {
                    theme.border_default()
                },
                6.0,
            )?;
            let fill = if primary {
                theme.accent_fg()
            } else {
                theme.bg_elevated()
            };
            px.rect(r.x + 2.0, r.y + 2.0, r.w - 4.0, r.h - 4.0, fill, 4.0)?;
            let color = if primary {
                theme.text_on_accent()
            } else if enabled {
                theme.text_primary()
            } else {
                theme.text_tertiary()
            };
            let selected = (i == 0 && self.certificate.is_some()) || (i == 1 && self.key.is_some());
            let label = if selected {
                format!("{label}  ·  Selected")
            } else {
                label.into()
            };
            px.text(
                f,
                body,
                r.x + 12.0,
                r.y + (r.h + body * 0.65) / 2.0,
                color,
                label,
            )?;
        }
        if !self.note.is_empty() {
            px.text(
                f,
                body,
                22.0,
                self.height * 0.74,
                theme.text_primary(),
                self.note.into(),
            )?;
        }
        frame.finish()
    }

    fn start_pick(
        &mut self,
        input: Input,
        picker: &mut Parent,
        sheet: EmbedId,
    ) -> Result<(), limen::client::Error> {
        let embed = picker.request_smime_picker(sheet, self.token)?;
        let filter = Filter {
            title: input.title().into(),
            ..Filter::file()
        };
        let start = picker
            .send(embed, ToChild::Open(filter).encode())
            .and_then(|()| picker.place(embed, Some([0.0, 0.0, 640.0, 440.0])));
        if start.is_err() {
            let _ = picker.detach(embed);
        }
        start?;
        self.pick = Some(Pick {
            embed,
            input,
            fd: None,
        });
        self.note = "";
        Ok(())
    }

    fn finish_pick(&mut self, picker: &mut Parent, done: bool) {
        let Some(pick) = self.pick.take() else { return };
        let _ = picker.detach(pick.embed);
        if done {
            let input = pick
                .fd
                .ok_or(Error::Malformed)
                .and_then(|fd| read_checked_der(fd.into(), pick.input.limit()));
            match input {
                Ok(data) => match pick.input {
                    Input::Certificate => self.certificate = Some(data),
                    Input::Key => self.key = Some(data),
                },
                Err(_) => self.note = "Arca couldn’t read the selected file.",
            }
        }
        self.dirty = true;
    }

    fn install(&mut self, arcad: &Arcad, registry: &str) -> Outcome {
        let (Some(certificate), Some(key)) = (self.certificate.take(), self.key.take()) else {
            return Outcome::Failed;
        };
        if !self.scoped || Identity::resolve(registry, self.owner).as_ref() != Some(&self.identity)
        {
            return Outcome::Failed;
        }
        match arcad.provision_smime(
            &self.identity.user,
            self.identity.persona.as_deref().unwrap_or(""),
            "Mail S/MIME identity",
            &certificate,
            &key,
        ) {
            Ok(fingerprint) => Outcome::Provisioned { fingerprint },
            Err(error) if error.locked() => Outcome::Locked,
            Err(_) => Outcome::Failed,
        }
    }

    fn step(&mut self, id: EmbedId, service: &mut Service) -> Result<Option<Outcome>, io::Error> {
        if self.finished {
            return Ok(None);
        }
        for _ in 0..32 {
            let Some(event) = self.win.poll_event()? else {
                break;
            };
            let request = match event {
                Event::Resized {
                    width,
                    height,
                    scale,
                    ..
                } => {
                    self.width = width;
                    self.height = height;
                    self.scale = scale.max(0.01);
                    self.dirty = true;
                    None
                }
                Event::Appearance(a) => {
                    self.dark = a.scheme == fresco_protocol::APPEARANCE_DARK;
                    self.text_scale = a.text_scale();
                    self.dirty = true;
                    None
                }
                Event::CloseRequested { .. }
                | Event::Key {
                    hid_usage: 0x29,
                    pressed: true,
                    ..
                } => {
                    return Ok(Some(Outcome::Cancelled));
                }
                Event::Key {
                    hid_usage: 0x2b,
                    pressed: true,
                    modifiers,
                    ..
                } if self.pick.is_none() => {
                    let shift = matches!(
                        crate::keymap::decode(0x2b, modifiers),
                        crate::keymap::Key::Tab { back: true }
                    );
                    let actions = self.actions();
                    for _ in 0..actions.len() {
                        self.focus = (self.focus + if shift { actions.len() - 1 } else { 1 })
                            % actions.len();
                        if actions[self.focus].3 {
                            break;
                        }
                    }
                    self.dirty = true;
                    None
                }
                Event::Key {
                    hid_usage: 0x28 | 0x58,
                    pressed: true,
                    activation,
                    ..
                } => {
                    let (action, _, _, enabled) = self.actions()[self.focus];
                    enabled.then_some((action, activation))
                }
                Event::PointerButton {
                    x,
                    y,
                    pressed: true,
                    activation,
                    ..
                } => self
                    .actions()
                    .into_iter()
                    .find(|(_, r, _, enabled)| *enabled && r.contains(x, y))
                    .map(|(action, _, _, _)| (action, activation)),
                _ => None,
            };
            let Some((action, press)) = request else {
                continue;
            };
            if self.pick.is_some() {
                continue;
            }
            if matches!(action, Action::Cancel) {
                return Ok(Some(Outcome::Cancelled));
            }
            if service.gate.approve(press, service.uid) != Ok(self.window) {
                self.note = "Wait until the sheet is fully visible and try again.";
                self.dirty = true;
                continue;
            }
            match action {
                Action::Pick(input) => {
                    if self.start_pick(input, &mut service.picker, id).is_err() {
                        self.note = "Arca couldn’t open the file picker.";
                    }
                    self.dirty = true;
                }
                Action::Install => {
                    return Ok(Some(self.install(&service.arcad, &service.registry)))
                }
                Action::Cancel => unreachable!(),
            }
        }
        if self.dirty {
            self.draw()?;
            self.dirty = false;
        }
        Ok(None)
    }
}

struct Service {
    child: Child,
    picker: Parent,
    arcad: Arcad,
    gate: Frescod,
    uid: u32,
    registry: String,
}

impl Service {
    fn finish(&mut self, sheets: &mut HashMap<EmbedId, Sheet>, id: EmbedId, outcome: Outcome) {
        if let Some(sheet) = sheets.get_mut(&id) {
            sheet.clear(&mut self.picker);
            sheet.finished = true;
        }
        let _ = self.child.emit(id, outcome.encode());
    }

    fn run(&mut self, fresco: &str, open_window: OpenWindow) -> Result<(), limen::client::Error> {
        let mut sheets = HashMap::new();
        loop {
            let wait = if sheets.is_empty() {
                Duration::from_millis(250)
            } else {
                POLL
            };
            match self.child.poll(wait)? {
                Some(ChildEvent::Attach {
                    embed,
                    role,
                    parent_app,
                    parent_uid,
                }) if role == ROLE && parent_app == "org.atrium.mail" => {
                    let sheet =
                        Identity::resolve(&self.registry, parent_uid).and_then(|identity| {
                            Sheet::open(fresco, open_window, parent_uid, identity).ok()
                        });
                    if let Some(sheet) = sheet {
                        if self.child.ready(embed, sheet.token).is_ok() {
                            sheets.insert(embed, sheet);
                        }
                    } else {
                        self.finish(&mut sheets, embed, Outcome::Failed);
                    }
                }
                Some(ChildEvent::ParentPersona { embed, persona }) => {
                    if let Some(sheet) = sheets.get_mut(&embed) {
                        sheet.scoped = sheet.identity.persona == persona;
                        sheet.dirty = true;
                    }
                }
                Some(ChildEvent::Suspended { embed }) => {
                    if let Some(sheet) = sheets.get_mut(&embed) {
                        sheet.clear(&mut self.picker);
                        sheet.note = "";
                        sheet.dirty = true;
                    }
                }
                Some(ChildEvent::Detaching { embed }) => {
                    if let Some(mut sheet) = sheets.remove(&embed) {
                        sheet.clear(&mut self.picker);
                        let _ = sheet.win.window_destroy(sheet.window);
                    }
                }
                _ => {}
            }
            for _ in 0..32 {
                let Some(event) = self.picker.poll(Duration::ZERO)? else {
                    break;
                };
                let (id, fd, done, failed) = match event {
                    ParentEvent::MessageFd { embed, bytes } => {
                        let file = self.picker.take_fd();
                        let valid = matches!(
                            ToParent::decode(&bytes),
                            Some(ToParent::File { write: false, .. })
                        );
                        (embed, if valid { file } else { None }, false, !valid)
                    }
                    ParentEvent::Message { embed, bytes } => (
                        embed,
                        None,
                        matches!(ToParent::decode(&bytes), Some(ToParent::Done { files: 1 })),
                        true,
                    ),
                    ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed } => {
                        (embed, None, false, true)
                    }
                    _ => continue,
                };
                if let Some(sheet) = sheets
                    .values_mut()
                    .find(|s| s.pick.as_ref().is_some_and(|p| p.embed == id))
                {
                    if done || failed {
                        sheet.finish_pick(&mut self.picker, done);
                    } else if let Some(pick) = sheet.pick.as_mut() {
                        pick.fd = fd;
                    }
                }
            }
            let mut outcomes = Vec::new();
            for (&id, sheet) in &mut sheets {
                match sheet.step(id, self) {
                    Ok(Some(outcome)) => outcomes.push((id, outcome)),
                    Ok(None) => {}
                    Err(_) => outcomes.push((id, Outcome::Failed)),
                }
            }
            for (id, outcome) in outcomes {
                self.finish(&mut sheets, id, outcome);
            }
        }
    }
}

pub fn serve(
    limen: String,
    fresco: String,
    registry: String,
    arcad: String,
    activation: String,
    open_window: OpenWindow,
) {
    std::thread::spawn(move || loop {
        let run = || {
            let mut service = Service {
                child: Child::provider(&limen, ROLE)?,
                picker: Parent::at(&limen)?,
                arcad: Arcad::new(&arcad),
                gate: Frescod::new(&activation),
                uid: unsafe { libc::getuid() },
                registry: registry.clone(),
            };
            service.run(&fresco, open_window)
        };
        if let Err(error) = run() {
            eprintln!("arca-ui: S/MIME sheet connection: {error:?}");
        }
        std::thread::sleep(Duration::from_secs(2));
    });
}
