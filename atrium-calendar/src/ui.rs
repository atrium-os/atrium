//! Calendar's native planner. Fasti is the only persistent writer.
use atrium_calendar::{
    wire::{page, Request},
    CalendarPage, CalendarView, Entry, Kind, Span,
};
use chrono::{Datelike, Local, NaiveDate};
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

mod overview;
use overview::rect;

const PAGE: usize = 6;
const WORKDAY: Span = Span {
    start: 540,
    end: 1020,
};

#[derive(Clone, Copy)]
enum Action {
    Resume,
    Load,
    Browse(CalendarView, NaiveDate),
    Settings(bool),
    SetOpening(CalendarView),
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
    data: Mutable<Option<CalendarPage>>,
    view: Mutable<Option<CalendarView>>,
    settings: Mutable<bool>,
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
        self.control(ctx, Button::secondary(label), x, y, w, action);
    }

    fn control(&self, ctx: &mut Ctx, button: Button, x: f32, y: f32, w: f32, action: Action) {
        let tx = self.actions.clone();
        button
            .at(x, y)
            .width(w)
            .on_click(move || {
                let _ = tx.send(action);
            })
            .render(ctx);
    }

    fn request(&self, action: Action) -> Result<(NaiveDate, Request), String> {
        if matches!(action, Action::Resume) {
            let date = self.shown.get();
            return Ok((
                date,
                Request::Browse {
                    date,
                    view: self.view.get(),
                },
            ));
        }
        if let Action::Browse(view, date) = action {
            return Ok((
                date,
                Request::Browse {
                    date,
                    view: Some(view),
                },
            ));
        }
        if let Action::SetOpening(opening_view) = action {
            let data = self.data.get_cloned().ok_or("Refresh Calendar first")?;
            let date = self.shown.get();
            return Ok((
                date,
                Request::SetOpeningView {
                    revision: data.revision,
                    opening_view,
                    date,
                    view: data.view,
                },
            ));
        }
        let date = self
            .date
            .get_cloned()
            .text()
            .trim()
            .parse::<NaiveDate>()
            .map_err(|_| "Enter a date as YYYY-MM-DD")?;
        if matches!(action, Action::Load) {
            return Ok((
                date,
                Request::Browse {
                    date,
                    view: self.view.get(),
                },
            ));
        }
        if date != self.shown.get() {
            return Err("Open the selected day first".into());
        }
        let revision = self
            .data
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
        let shown = self.shown.get();
        let heading = match self.view.get() {
            None => "Calendar".to_owned(),
            Some(CalendarView::Today) if shown == Local::now().date_naive() => {
                format!("Today · {shown}")
            }
            Some(CalendarView::Today) => format!("Day · {shown}"),
            Some(view @ CalendarView::Week) => format!(
                "Week of {}",
                view.dates(shown).next().unwrap().format("%d %b %Y")
            ),
            Some(CalendarView::Month) => shown.format("%B %Y").to_string(),
        };
        let theme = ctx.theme;
        rect(ctx, 0., 0., 700., 600., theme.bg_canvas());
        rect(ctx, 0., 0., 700., 84., theme.bg_surface());
        rect(ctx, 0., 84., 700., 1., theme.border_default());
        Label::new(heading).size(22.).at(18., 18.).render(ctx);
        Label::new(&self.calendar_name)
            .at(350., 19.)
            .size(16.)
            .render(ctx);
        let data = self.data.get_cloned();
        if self.waiting.get() {
            Label::new("Loading calendar…").at(18., 110.).render(ctx);
            return;
        }
        let Some(data) = data else {
            self.button(ctx, 18., 44., 130., "Refresh", Action::Resume);
            Label::new(self.status.get_cloned())
                .at(18., 110.)
                .render(ctx);
            return;
        };
        let view = data.view;
        if self.settings.get() {
            self.button(
                ctx,
                18.,
                44.,
                124.,
                "Back to calendar",
                Action::Settings(false),
            );
            Label::new("Opening view")
                .at(18., 120.)
                .size(20.)
                .render(ctx);
            Label::new(format!(
                "{} opens in {}.",
                self.calendar_name,
                data.opening_view.label()
            ))
            .at(18., 154.)
            .render(ctx);
            for (i, choice) in CalendarView::ALL.into_iter().enumerate() {
                let button = if choice == data.opening_view {
                    Button::primary(choice.label())
                } else {
                    Button::secondary(choice.label())
                };
                self.control(
                    ctx,
                    button,
                    18. + i as f32 * 112.,
                    178.,
                    104.,
                    Action::SetOpening(choice),
                );
            }
            return;
        }
        for (i, choice) in CalendarView::ALL.into_iter().enumerate() {
            let button = if choice == view {
                Button::primary(choice.label())
            } else {
                Button::ghost(choice.label())
            };
            let date = if choice == CalendarView::Today {
                Local::now().date_naive()
            } else {
                shown
            };
            self.control(
                ctx,
                button,
                18. + i as f32 * 88.,
                44.,
                82.,
                Action::Browse(choice, date),
            );
        }
        self.button(
            ctx,
            531.,
            44.,
            150.,
            "Opening view…",
            Action::Settings(true),
        );
        Label::new(self.status.get_cloned())
            .at(18., 565.)
            .render(ctx);
        if view != CalendarView::Today {
            self.overview(ctx, shown, &data);
            return;
        }
        let day = data.into_day();
        for (label, field, x, w) in [
            ("Date", &self.date, 18., 140.),
            ("Title", &self.title, 174., 238.),
            ("Start", &self.start, 426., 82.),
            ("End", &self.end, 522., 82.),
        ] {
            Label::new(label).at(x, 106.).render(ctx);
            TextField::new(field.clone())
                .at(x, 128.)
                .width(w)
                .render(ctx);
        }
        self.button(ctx, 18., 168., 124., "Open / Refresh", Action::Load);
        self.button(
            ctx,
            151.,
            168.,
            114.,
            "Add event",
            Action::Save(Kind::Appointment),
        );
        self.button(
            ctx,
            273.,
            168.,
            114.,
            "Add weekly",
            Action::Save(Kind::Weekly),
        );
        self.button(
            ctx,
            395.,
            168.,
            114.,
            "Add all-day",
            Action::Save(Kind::AllDay),
        );
        self.button(
            ctx,
            518.,
            168.,
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
                .at(18., 222.)
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
            Label::new("Agenda · local time").at(18., 222.).render(ctx);
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
            .at(18., 262.)
            .render(ctx);
        }
        for (i, (label, action)) in rows.iter().skip(page * PAGE).take(PAGE).enumerate() {
            let y = 254. + i as f32 * 38.;
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
            self.button(ctx, 18., 498., 105., "Previous", Action::Page(page - 1));
        }
        if (page + 1) * PAGE < rows.len() {
            self.button(ctx, 132., 498., 105., "Next", Action::Page(page + 1));
        }
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
    let mut win = Window::open(&WindowDesc::new(&calendar_name, SizeSpec::Fixed(700, 600)))?;
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
        data: Mutable::with_dirty(None, dirty.clone()),
        view: Mutable::with_dirty(None, dirty.clone()),
        settings: Mutable::with_dirty(false, dirty.clone()),
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
    tx.send(Action::Resume).unwrap();
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
                Action::Settings(open) => planner.settings.set(open),
                Action::Page(p) => planner.page.set(p),
                action => match planner.request(action) {
                    Ok((date, req)) => {
                        planner.waiting.set(true);
                        let tx = done.clone();
                        let path = socket.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send((date, action, page(&path, &req)));
                        });
                    }
                    Err(e) => planner.status.set(e),
                },
            }
        }
        while let Ok((date, action, reply)) = answers.try_recv() {
            planner.waiting.set(false);
            match reply {
                Ok(data) => {
                    planner.view.set(Some(data.view));
                    if matches!(action, Action::Resume | Action::Load | Action::Browse(..)) {
                        planner.date.set(TextEdit::new(&date.to_string()));
                        planner.finding.set(false);
                    }
                    planner.settings.set(false);
                    planner.shown.set(date);
                    planner.data.set(Some(data));
                    planner.page.set(0);
                    if matches!(action, Action::Save(_)) {
                        planner.title.set(TextEdit::new(""));
                    }
                    planner.status.set(
                        match action {
                            Action::SetOpening(_) => "Opening view saved",
                            Action::Hold(_) => "Held on this device",
                            Action::Release(_) => "Hold released",
                            Action::Save(_) => "Event saved",
                            _ => "On this device",
                        }
                        .into(),
                    );
                }
                Err(e) => {
                    planner.data.set(None);
                    planner.settings.set(false);
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
