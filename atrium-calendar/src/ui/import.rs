//! A picker grants one descriptor. Only an explicit press in Review can save.
use super::*;
use atrium_calendar::{
    import::{Candidates, Outcome, Parse, Parsed, Prepared, FILE_LIMIT, FRAME_LIMIT},
    wire,
};
use limen::{
    picker::{Filter, ToChild, ToParent},
    proto::ParentEvent,
};
use pergola::{embed::Embeds, geom::Rect, node::Node};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, fs::File, io::Read, path::Path, rc::Rc};
use uuid::Uuid;

const SLOT: u64 = 1;

#[derive(Clone)]
pub(super) enum Screen {
    Picking,
    Reading,
    Problem(String),
    Existing(NaiveDate),
    Ready {
        data: Arc<Candidates>,
        token: Uuid,
        revision: u64,
        offset: usize,
    },
}

pub(super) struct Picker {
    broker: Rc<RefCell<Embeds>>,
    id: u64,
    file: Option<File>,
    attach_deadline: Option<std::time::Instant>,
}
impl Picker {
    pub fn open(win: &mut Window, persona: &str) -> Result<Self, String> {
        let token = win.embed_token().ok_or("Calendar window is not ready")?;
        let broker = Embeds::connect().map_err(|_| "File picker unavailable")?;
        win.set_slot_binder(Embeds::binder(&broker));
        let id = broker
            .borrow_mut()
            .request("picker", token, SLOT)
            .map_err(|_| "Cannot open the file picker")?;
        let picker = Self {
            broker,
            id,
            file: None,
            attach_deadline: Some(std::time::Instant::now() + Duration::from_secs(10)),
        };
        let filter = Filter {
            extensions: vec!["ics".into()],
            title: "Choose a calendar file".into(),
            ..Filter::file()
        };
        picker
            .broker
            .borrow_mut()
            .send(
                id,
                ToChild::OpenInPersona {
                    filter,
                    persona: persona.into(),
                }
                .encode(),
            )
            .map_err(|_| "File picker disconnected")?;
        Ok(picker)
    }

    // None means still choosing, Ok(None) means the person cancelled.
    pub fn poll(&mut self) -> Option<Result<Option<File>, String>> {
        if self
            .attach_deadline
            .is_some_and(|due| std::time::Instant::now() >= due)
        {
            return Some(Err("Persona-aware file picker unavailable".into()));
        }
        let mut broker = self.broker.borrow_mut();
        let event = match broker.poll(Duration::from_millis(1)) {
            Ok(Some(e)) => e,
            Ok(None) => return None,
            Err(_) => return Some(Err("File picker disconnected".into())),
        };
        match event {
            ParentEvent::Attached { embed } if embed == self.id => {
                self.attach_deadline = None;
                None
            }
            ParentEvent::MessageFd { embed, bytes } => {
                let fd = broker.take_fd();
                if embed != self.id {
                    return None;
                }
                if let (
                    Some(ToParent::File {
                        write: false, size, ..
                    }),
                    Some(fd),
                ) = (ToParent::decode(&bytes), fd)
                {
                    if size > FILE_LIMIT as u64 {
                        return Some(Err("File exceeds the 4 MiB import limit".into()));
                    }
                    if self.file.is_none() {
                        self.file = Some(File::from(fd));
                        return None;
                    }
                }
                Some(Err("Expected one read-only calendar file".into()))
            }
            ParentEvent::Message { embed, bytes } if embed == self.id => {
                Some(match ToParent::decode(&bytes) {
                    Some(ToParent::Done { files: 1 }) => self
                        .file
                        .take()
                        .map(Some)
                        .ok_or("File picker returned no file".into()),
                    Some(ToParent::Cancelled) => Ok(None),
                    Some(ToParent::Failed { why })
                        if why == "Cannot verify the file picker persona" =>
                    {
                        Err(why)
                    }
                    _ => Err("File picker could not hand over the selection".into()),
                })
            }
            ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed }
                if embed == self.id =>
            {
                Some(Err("File picker closed".into()))
            }
            _ => None,
        }
    }
}
impl Drop for Picker {
    fn drop(&mut self) {
        let _ = self.broker.borrow_mut().detach_embed(self.id);
    }
}

pub(super) fn prepare(file: File, revision: u64, socket: &Path) -> Result<Screen, String> {
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= FILE_LIMIT as u64)
    {
        return Err("Choose a regular file up to 4 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read selected file")?;
    if bytes.len() > FILE_LIMIT {
        return Err("File exceeds the 4 MiB import limit".into());
    }
    let hash = Sha256::digest(&bytes).into();
    let fd = limen::fdpass::sealed_file("calendar-import", &bytes)
        .map_err(|_| "Could not seal the selected file")?;
    drop(bytes);
    let workers = atrium_worker::Workers::from_root(Path::new("/"), false)
        .map_err(|_| "Calendar format worker unavailable")?;
    let parsed: Parsed = workers
        .max_answer(FRAME_LIMIT + 16)
        .call("ical", &Parse {}, Some(File::from(fd)))
        .map_err(|_| "Calendar format worker failed")?;
    let data: Candidates = match serde_json::from_slice(&parsed.schedule)
        .map_err(|_| "Invalid format worker reply")?
    {
        Outcome::Blocked(issue) => return Err(issue.message().into()),
        Outcome::Ready(c) => c,
    };
    if data.rows.is_empty()
        || data.rows.len() > atrium_calendar::import::ROW_LIMIT
        || data.rows.iter().any(|row| {
            row.source.is_some()
                || row.import_hash.is_some()
                || row.kind == Kind::Hold
                || !row.span.valid()
                || row.validate().is_err()
        })
    {
        return Err("Invalid normalized calendar schedule".into());
    }
    let prepared = wire::prepare_import(socket, revision, hash, data.rows.clone())?;
    Ok(match prepared {
        Prepared::New(token) => Screen::Ready {
            data: Arc::new(data),
            revision,
            token,
            offset: 0,
        },
        Prepared::Existing(date) => Screen::Existing(date),
    })
}

impl Planner {
    pub(super) fn render_import(&self, ctx: &mut Ctx, screen: Screen) {
        Label::new("Import calendar file")
            .at(18., 65.)
            .size(22.)
            .render(ctx);
        Label::new(format!("Destination · {}", self.calendar_name))
            .at(18., 107.)
            .render(ctx);
        match screen {
            Screen::Picking => {
                Label::new("Choose an .ics file in Files.")
                    .at(18., 160.)
                    .render(ctx);
                let node = ctx.add(Node::Rect {
                    rect: Rect::new(0., 0., 700., 600.),
                    fill: pergola::Color::TRANSPARENT,
                    radius: 0.,
                });
                ctx.slot(node, SLOT);
            }
            Screen::Reading => Label::new("Reading the selected file…")
                .at(18., 160.)
                .render(ctx),
            Screen::Problem(why) => {
                Label::new("Import unavailable")
                    .at(18., 160.)
                    .size(18.)
                    .render(ctx);
                Label::new(why).at(18., 205.).render(ctx);
                self.button(
                    ctx,
                    18.,
                    500.,
                    140.,
                    "Back to calendar",
                    Action::CloseImport,
                );
            }
            Screen::Existing(date) => {
                Label::new("This file already has a local copy.")
                    .at(18., 160.)
                    .render(ctx);
                self.button(ctx, 18., 500., 140., "Back", Action::CloseImport);
                self.button(
                    ctx,
                    180.,
                    500.,
                    170.,
                    "Open existing day",
                    Action::Browse(CalendarView::Today, date),
                );
            }
            Screen::Ready { data, offset, .. } => {
                let rows = &data.rows;
                let first = rows.iter().map(|e| e.date).min().unwrap();
                let last = rows.iter().map(|e| e.date).max().unwrap();
                Label::new(format!(
                    "{} events · Start dates {} – {}",
                    rows.len(),
                    first,
                    last
                ))
                .at(18., 148.)
                .render(ctx);
                for (i, e) in rows.iter().skip(offset).take(4).enumerate() {
                    let y = 192. + i as f32 * 64.;
                    let when = if e.kind == Kind::AllDay {
                        "All day".into()
                    } else {
                        format!(
                            "{}–{} · {}",
                            clock(e.span.start),
                            clock(e.span.end),
                            e.zone.map_or("Floating", |z| z.name.name())
                        )
                    };
                    let repeat = match e.kind {
                        Kind::Weekly => format!(" · Weekly ({} exceptions)", e.excluded.len()),
                        Kind::Occurrence { original, .. } => format!(" · Edited from {original}"),
                        _ => String::new(),
                    };
                    Label::new(e.title.chars().take(52).collect::<String>())
                        .at(18., y)
                        .size(17.)
                        .render(ctx);
                    Label::new(format!("{} · {when}{repeat}", e.date))
                        .at(18., y + 25.)
                        .render(ctx);
                }
                if data.omitted_details {
                    Label::new(
                        "Only titles and schedules will be copied. Other file details are omitted.",
                    )
                    .at(18., 450.)
                    .render(ctx);
                }
                if offset >= 4 {
                    self.button(
                        ctx,
                        18.,
                        486.,
                        104.,
                        "Previous",
                        Action::ImportPage(offset - 4),
                    );
                }
                if offset + 4 < rows.len() {
                    self.button(
                        ctx,
                        132.,
                        486.,
                        104.,
                        "Next",
                        Action::ImportPage(offset + 4),
                    );
                }
                self.button(ctx, 18., 542., 115., "Cancel", Action::CloseImport);
                self.control(
                    ctx,
                    Button::primary("Save local copies"),
                    485.,
                    542.,
                    190.,
                    Action::SaveImport,
                );
            }
        }
    }
}
