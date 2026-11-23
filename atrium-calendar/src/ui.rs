//! Calendar's first native day view. Fasti is the only persistent writer.
use atrium_calendar::{
    wire::{call, Request},
    Day, Entry, Kind, Span,
};
use chrono::{Local, NaiveDate};
use pergola::{
    text_edit::TextEdit, view::Ctx, window::StepResult, App, Button, Label, Mutable, SizeSpec,
    TextField, View, Window, WindowDesc,
};
use std::{
    io,
    path::PathBuf,
    sync::{atomic::AtomicBool, mpsc, Arc},
    time::Duration,
};
use uuid::Uuid;

const PAGE: usize = 6;
const WORKDAY: Span = Span {
    start: 540,
    end: 1020,
};

#[derive(Clone, Copy)]
enum Action {
    Load,
    Save(Kind),
    Hold(Span),
    Release(Uuid),
    Find,
    Page(usize),
}

struct Planner {
    calendar_name: String,
    date: Mutable<TextEdit>,
    title: Mutable<TextEdit>,
    start: Mutable<TextEdit>,
    end: Mutable<TextEdit>,
    shown: Mutable<NaiveDate>,
    day: Mutable<Option<Day>>,
    status: Mutable<String>,
    finding: Mutable<bool>,
    page: Mutable<usize>,
    waiting: Mutable<bool>,
    actions: mpsc::Sender<Action>,
}

fn clock(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}
fn time(field: &TextEdit) -> Result<u16, String> {
    let (h, m) = field
        .text()
        .trim()
        .split_once(':')
        .ok_or("Enter times as HH:MM")?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return Err("Enter times as HH:MM".into());
    }
    let (h, m) = (h.parse::<u16>().unwrap(), m.parse::<u16>().unwrap());
    if h > 24 || m >= 60 || (h == 24 && m != 0) {
        return Err("Invalid local time".into());
    }
    Ok(h * 60 + m)
}

impl Planner {
    fn button(&self, ctx: &mut Ctx, x: f32, y: f32, w: f32, label: &str, action: Action) {
        let tx = self.actions.clone();
        Button::secondary(label)
            .at(x, y)
            .width(w)
            .on_click(move || {
                let _ = tx.send(action);
            })
            .render(ctx);
    }

    fn request(&self, action: Action) -> Result<(NaiveDate, Request), String> {
        let date = self
            .date
            .get_cloned()
            .text()
            .trim()
            .parse::<NaiveDate>()
            .map_err(|_| "Enter a date as YYYY-MM-DD")?;
        if matches!(action, Action::Load) {
            return Ok((date, Request::Day { date }));
        }
        if date != self.shown.get() {
            return Err("Open the selected day first".into());
        }
        let revision = self
            .day
            .get_cloned()
            .ok_or("Refresh the day first")?
            .revision;
        if let Action::Release(id) = action {
            return Ok((date, Request::Release { revision, date, id }));
        }
        let (kind, span, title) = match action {
            Action::Hold(span) => (Kind::Hold, span, "Preparation".to_string()),
            Action::Save(kind) => {
                let span = match kind {
                    Kind::AllDay => Span {
                        start: 0,
                        end: 1440,
                    },
                    _ => Span {
                        start: time(&self.start.get_cloned())?,
                        end: time(&self.end.get_cloned())?,
                    },
                };
                (
                    kind,
                    span,
                    self.title.get_cloned().text().trim().to_string(),
                )
            }
            _ => return Err("Unexpected calendar action".into()),
        };
        let entry = Entry {
            id: Uuid::new_v4(),
            date,
            title,
            span,
            kind,
        };
        entry.validate()?;
        Ok((date, Request::Add { revision, entry }))
    }
}

impl View for Planner {
    fn render(&self, ctx: &mut Ctx) {
        let theme = ctx.theme;
        for (rect, fill) in [
            (
                pergola::geom::Rect::new(0., 0., 700., 540.),
                theme.bg_canvas(),
            ),
            (
                pergola::geom::Rect::new(0., 0., 700., 156.),
                theme.bg_surface(),
            ),
            (
                pergola::geom::Rect::new(0., 156., 700., 1.),
                theme.border_default(),
            ),
        ] {
            ctx.add(pergola::node::Node::Rect {
                rect,
                fill,
                radius: 0.0,
            });
        }
        let shown = self.shown.get();
        Label::new(format!(
            "{} · {}",
            if shown == Local::now().date_naive() {
                "Today"
            } else {
                "Day"
            },
            shown
        ))
        .size(22.)
        .at(18., 16.)
        .render(ctx);
        Label::new(&self.calendar_name)
            .at(320., 18.)
            .size(16.)
            .render(ctx);
        for (label, field, x, w) in [
            ("Date", &self.date, 18., 140.),
            ("Title", &self.title, 174., 238.),
            ("Start", &self.start, 426., 82.),
            ("End", &self.end, 522., 82.),
        ] {
            Label::new(label).at(x, 60.).render(ctx);
            TextField::new(field.clone())
                .at(x, 82.)
                .width(w)
                .render(ctx);
        }
        if self.waiting.get() {
            Label::new("Loading calendar…").at(18., 130.).render(ctx);
            return;
        }
        self.button(ctx, 18., 122., 124., "Open / Refresh", Action::Load);
        let Some(day) = self.day.get_cloned() else {
            Label::new(self.status.get_cloned())
                .at(18., 176.)
                .render(ctx);
            return;
        };
        self.button(
            ctx,
            151.,
            122.,
            114.,
            "Add event",
            Action::Save(Kind::Appointment),
        );
        self.button(
            ctx,
            273.,
            122.,
            114.,
            "Add weekly",
            Action::Save(Kind::Weekly),
        );
        self.button(
            ctx,
            395.,
            122.,
            114.,
            "Add all-day",
            Action::Save(Kind::AllDay),
        );
        self.button(
            ctx,
            518.,
            122.,
            136.,
            if self.finding.get() {
                "Show agenda"
            } else {
                "Find 45 min"
            },
            Action::Find,
        );
        let page = self.page.get();
        let mut rows = Vec::new();
        if self.finding.get() {
            Label::new("45 minutes between 09:00 and 17:00")
                .at(18., 176.)
                .render(ctx);
            for gap in day.openings(WORKDAY, 45).expect("constant valid window") {
                let span = Span {
                    start: gap.start,
                    end: gap.start + 45,
                };
                rows.push((
                    format!(
                        "{}–{}   Open for {} minutes",
                        clock(gap.start),
                        clock(gap.end),
                        gap.end - gap.start
                    ),
                    Some(("Hold 45 min", Action::Hold(span))),
                ));
            }
        } else {
            Label::new("Agenda · local time").at(18., 176.).render(ctx);
            for e in day.entries {
                let label = match e.kind {
                    Kind::Weekly => "Weekly · ",
                    Kind::AllDay => "All day · ",
                    Kind::Hold => "Private hold · ",
                    Kind::Appointment => "",
                };
                let time = if e.kind == Kind::AllDay {
                    String::new()
                } else {
                    format!("{}–{}   ", clock(e.span.start), clock(e.span.end))
                };
                let title: String = e.title.chars().take(40).collect();
                rows.push((
                    format!("{time}{label}{title}"),
                    (e.kind == Kind::Hold).then_some(("Release", Action::Release(e.id))),
                ));
            }
        }
        if rows.is_empty() {
            Label::new(if self.finding.get() {
                "No 45-minute opening"
            } else {
                "No events on this day"
            })
            .at(18., 216.)
            .render(ctx);
        }
        for (i, (label, action)) in rows.iter().skip(page * PAGE).take(PAGE).enumerate() {
            let y = 208. + i as f32 * 38.;
            ctx.add(pergola::node::Node::Rect {
                rect: pergola::geom::Rect::new(18., y + 31., 636., 1.),
                fill: theme.border_default(),
                radius: 0.,
            });
            Label::new(label).at(18., y + 6.).render(ctx);
            if let Some((label, a)) = action {
                self.button(ctx, 534., y, 120., label, *a);
            }
        }
        if page > 0 {
            self.button(ctx, 18., 445., 105., "Previous", Action::Page(page - 1));
        }
        if (page + 1) * PAGE < rows.len() {
            self.button(ctx, 132., 445., 105., "Next", Action::Page(page + 1));
        }
        Label::new(self.status.get_cloned())
            .at(18., 492.)
            .render(ctx);
    }
}

pub fn run() -> io::Result<()> {
    let today = Local::now().date_naive();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let calendar_name = match args.as_slice() {
        [] => "Calendar".to_owned(),
        [flag, name]
            if flag == "--persona"
                && (name == "personal" || portcullis_peer::valid_person_persona(name)) =>
        {
            let mut name = name.clone();
            name[..1].make_ascii_uppercase();
            format!("{name} calendar")
        }
        _ => return Err(io::Error::other("Invalid Calendar launch arguments")),
    };
    let mut win = Window::open(&WindowDesc::new(&calendar_name, SizeSpec::Fixed(700, 540)))?;
    let dirty = Arc::new(AtomicBool::new(true));
    let field = |s: &str| Mutable::with_dirty(TextEdit::new(s), dirty.clone());
    let (tx, rx) = mpsc::channel();
    let planner = Arc::new(Planner {
        calendar_name,
        date: field(&today.to_string()),
        title: field(""),
        start: field("09:00"),
        end: field("09:30"),
        shown: Mutable::with_dirty(today, dirty.clone()),
        day: Mutable::with_dirty(None, dirty.clone()),
        status: Mutable::with_dirty(String::new(), dirty.clone()),
        finding: Mutable::with_dirty(false, dirty.clone()),
        page: Mutable::with_dirty(0, dirty.clone()),
        waiting: Mutable::with_dirty(false, dirty.clone()),
        actions: tx.clone(),
    });
    #[derive(Clone)]
    struct Root(Arc<Planner>);
    impl View for Root {
        fn render(&self, ctx: &mut Ctx) {
            self.0.render(ctx)
        }
    }
    let mut app = App::new_with_flag(Root(planner.clone()), dirty);
    let socket = std::env::var_os("FASTI_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/atrium/sockets/calendar/calendar.sock".into());
    let (done, answers) = mpsc::channel();
    tx.send(Action::Load).unwrap();
    loop {
        while let Ok(action) = rx.try_recv() {
            if planner.waiting.get() {
                continue;
            }
            match action {
                Action::Find => {
                    planner.finding.set(!planner.finding.get());
                    planner.page.set(0);
                }
                Action::Page(p) => planner.page.set(p),
                action => match planner.request(action) {
                    Ok((date, req)) => {
                        planner.waiting.set(true);
                        let tx = done.clone();
                        let path = socket.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send((date, action, call(&path, &req)));
                        });
                    }
                    Err(e) => planner.status.set(e),
                },
            }
        }
        while let Ok((date, action, reply)) = answers.try_recv() {
            planner.waiting.set(false);
            match reply {
                Ok(day) => {
                    planner.shown.set(date);
                    planner.day.set(Some(day));
                    planner.page.set(0);
                    if matches!(action, Action::Save(_)) {
                        planner.title.set(TextEdit::new(""));
                    }
                    planner.status.set(
                        match action {
                            Action::Hold(_) => "Held on this device",
                            Action::Release(_) => "Hold released",
                            Action::Save(_) => "Event saved",
                            _ => "On this device",
                        }
                        .into(),
                    );
                }
                Err(e) => {
                    planner.day.set(None);
                    planner.status.set(e);
                }
            }
        }
        if matches!(
            win.step(&mut app, Some(Duration::from_millis(80)))?,
            StepResult::Closed
        ) {
            return Ok(());
        }
    }
}
