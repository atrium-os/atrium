//! Calendar's native planner. Fasti is the only persistent writer.
use atrium_calendar::{
    wire::{page, Request},
    CalendarPage, CalendarView, EditScope, Entry, EventEdit, Kind, Span,
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

mod search;
use search::{Results, SearchForm};
mod editor;
use editor::{Editor, Stage};
mod import;
mod invitations;
use import::{Picker, Screen};
mod overview;
use overview::rect;

const PAGE: usize = 6;
const WORKDAY: Span = Span {
    start: 540,
    end: 1020,
};

#[derive(Clone, Copy)]
enum Action {
    Undo(atrium_calendar::UndoReceipt),
    Resume,
    Load,
    Browse(CalendarView, NaiveDate),
    Settings(bool),
    SetOpening(CalendarView),
    SetZone(bool),
    Today(Option<CalendarView>),
    Save(Kind),
    Hold(Span),
    Release(Uuid),
    Edit(Uuid, EditScope),
    Review(Stage),
    CloseEditor,
    Change(bool),
    SearchPanel(bool),
    Search(Option<usize>),
    OpenHit(Uuid, NaiveDate, u64),
    PickImport,
    CloseImport,
    ApplyInvitation { cancellation: bool },
    InvitationPage(usize),
    ReplyInvitation(portcullis_ipc::calendar::Answer),
    SaveImport,
    ImportPage(usize),
    NewZoned,
    CreateZoned(Kind),
    ChooseFold(bool, atrium_calendar::Fold),
    Find,
    Page(usize),
}

enum Reply {
    Calendar(CalendarPage),
    Results(Results),
}

struct Planner {
    import: Mutable<Option<Screen>>,
    undo: Mutable<Option<(atrium_calendar::UndoReceipt, &'static str)>>,
    search: Mutable<Option<SearchForm>>,
    selected_row: Mutable<Option<usize>>,
    calendar_name: String,
    date: Mutable<TextEdit>,
    title: Mutable<TextEdit>,
    start: Mutable<TextEdit>,
    end: Mutable<TextEdit>,
    shown: Mutable<NaiveDate>,
    data: Mutable<Option<CalendarPage>>,
    view: Mutable<Option<CalendarView>>,
    settings: Mutable<bool>,
    zone_field: Mutable<TextEdit>,
    editor: Mutable<Option<Editor>>,
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
    fn viewing_zone(&self) -> chrono_tz::Tz {
        self.data
            .get_cloned()
            .map_or(chrono_tz::UTC, |d| d.viewing_zone)
    }

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

    fn request(&self, action: Action) -> Result<Request, String> {
        if matches!(action, Action::ApplyInvitation { .. }) {
            let Some(Screen::Invitation(sheet)) = self.import.get_cloned() else {
                return Err("Review the invitation first".into());
            };
            return sheet.save();
        }
        if matches!(action, Action::SaveImport) {
            let Some(Screen::Ready {
                revision, token, ..
            }) = self.import.get_cloned()
            else {
                return Err("Review the file first".into());
            };
            return Ok(Request::SaveImport { revision, token });
        }
        if let Action::Undo(receipt) = action {
            return Ok(Request::Undo { receipt });
        }
        if let Action::Search(offset) = action {
            let form = self.search.get_cloned().ok_or("Open Search first")?;
            return form.request(offset);
        }
        if let Action::OpenHit(id, date, revision) = action {
            return Ok(Request::OpenSearchHit { revision, id, date });
        }
        if matches!(action, Action::Resume) {
            let date = self.shown.get();
            return Ok(Request::Browse {
                date,
                view: self.view.get(),
            });
        }
        if let Action::Browse(view, date) = action {
            return Ok(Request::Browse {
                date,
                view: Some(view),
            });
        }
        if let Action::Today(view) = action {
            return Ok(Request::Today { view });
        }
        if let Action::SetZone(follow) = action {
            let data = self.data.get_cloned().ok_or("Refresh Calendar first")?;
            let pinned = if follow {
                None
            } else {
                Some(
                    atrium_calendar::zone::parse_name(self.zone_field.get_cloned().text())
                        .ok_or("Enter a time zone such as Europe/London")?,
                )
            };
            return Ok(Request::SetViewingZone {
                revision: data.revision,
                pinned,
                date: data.date,
                view: data.view,
            });
        }
        if let Action::SetOpening(opening_view) = action {
            let data = self.data.get_cloned().ok_or("Refresh Calendar first")?;
            let date = self.shown.get();
            return Ok(Request::SetOpeningView {
                revision: data.revision,
                opening_view,
                date,
                view: data.view,
            });
        }
        if let Action::CreateZoned(kind) = action {
            return self
                .editor
                .get_cloned()
                .ok_or("Open the zoned event form first")?
                .create(kind);
        }
        if let Action::Change(delete) = action {
            return self
                .editor
                .get_cloned()
                .ok_or("Open the event first")?
                .request(delete);
        }
        let date = self
            .date
            .get_cloned()
            .text()
            .trim()
            .parse::<NaiveDate>()
            .map_err(|_| "Enter a date as YYYY-MM-DD")?;
        if matches!(action, Action::Load) {
            return Ok(Request::Browse {
                date,
                view: self.view.get(),
            });
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
            return Ok(Request::Release { revision, date, id });
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
            import_hash: None,
            invitation: None,
            excluded: Vec::new(),
            zone: None,
            source: None,
            id: Uuid::new_v4(),
            date,
            title,
            span,
            kind,
        };
        entry.validate()?;
        Ok(Request::Add { revision, entry })
    }
}

impl View for Planner {
    fn render(&self, ctx: &mut Ctx) {
        let shown = self.shown.get();
        let heading = match self.view.get() {
            None => "Calendar".to_owned(),
            Some(CalendarView::Today)
                if shown
                    == chrono::Utc::now()
                        .with_timezone(&self.viewing_zone())
                        .date_naive() =>
            {
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
        let search = self.search.get_cloned();
        let editor = self.editor.get_cloned();
        Label::new(if search.is_some() {
            "Search".into()
        } else if editor.is_some() {
            "Calendar".into()
        } else {
            heading
        })
        .size(22.)
        .at(18., 18.)
        .render(ctx);
        Label::new(&self.calendar_name)
            .at(350., 19.)
            .size(16.)
            .render(ctx);
        let data = self.data.get_cloned();
        if self.waiting.get() {
            Label::new("Loading calendar…").at(18., 110.).render(ctx);
            return;
        }
        if let Some(screen) = self.import.get_cloned() {
            self.render_import(ctx, screen);
            return;
        }
        if let Some(form) = search {
            form.render(self, ctx);
            return;
        }
        if let Some(edit) = editor {
            self.render_editor(ctx, &edit);
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
            Label::new("Viewing time zone")
                .at(18., 259.)
                .size(20.)
                .render(ctx);
            let current = if data.pinned_zone.is_none() {
                format!("Follows this device · {}", data.viewing_zone)
            } else {
                format!("Fixed for this calendar · {}", data.viewing_zone)
            };
            Label::new(current).at(18., 292.).render(ctx);
            self.button(ctx, 18., 315., 164., "Follow device", Action::SetZone(true));
            TextField::new(self.zone_field.clone())
                .key(118)
                .at(18., 372.)
                .width(300.)
                .render(ctx);
            self.button(
                ctx,
                330.,
                372.,
                160.,
                "Use this zone",
                Action::SetZone(false),
            );
            Label::new(self.status.get_cloned())
                .at(18., 435.)
                .render(ctx);
            return;
        }
        self.button(ctx, 531., 5., 150., "Import .ics…", Action::PickImport);
        for (i, choice) in CalendarView::ALL.into_iter().enumerate() {
            let button = if choice == view {
                Button::primary(choice.label())
            } else {
                Button::ghost(choice.label())
            };
            let action = if choice == CalendarView::Today {
                Action::Today(Some(choice))
            } else {
                Action::Browse(choice, shown)
            };
            self.control(ctx, button, 18. + i as f32 * 88., 44., 82., action);
        }
        self.button(ctx, 283., 44., 104., "Zoned event…", Action::NewZoned);
        self.button(ctx, 396., 44., 121., "Search…", Action::SearchPanel(true));
        self.button(ctx, 531., 44., 150., "Settings…", Action::Settings(true));
        Label::new(self.status.get_cloned())
            .at(18., 565.)
            .render(ctx);
        if view == CalendarView::Today {
            self.agenda(ctx, data);
        } else {
            self.overview(ctx, shown, &data);
        }
        // Render the footer last so Tab reaches Undo after the agenda or grid.
        if let Some((receipt, label)) = self.undo.get() {
            self.button(ctx, 500., 548., 181., label, Action::Undo(receipt));
        }
    }
}

impl Planner {
    fn agenda(&self, ctx: &mut Ctx, data: CalendarPage) {
        let theme = ctx.theme;
        let viewing_zone = data.viewing_zone;
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
            Label::new(if day.entries.iter().any(|e| e.zone.is_some()) {
                format!("45 minutes between 09:00 and 17:00 · {viewing_zone}")
            } else {
                "45 minutes between 09:00 and 17:00".to_owned()
            })
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
                    ("Hold 45 min", Action::Hold(span)),
                    None,
                ));
            }
        } else {
            Label::new(if day.entries.iter().any(|e| e.zone.is_some()) {
                format!("Agenda · {viewing_zone}")
            } else {
                "Agenda · local time".to_owned()
            })
            .at(18., 222.)
            .render(ctx);
            for e in day.entries {
                let label = match e.kind {
                    Kind::Weekly | Kind::Occurrence { .. } => "Weekly · ",
                    Kind::AllDay => "All day · ",
                    Kind::Hold => "Private hold · ",
                    Kind::Appointment => "",
                };
                let time = if e.kind == Kind::AllDay {
                    String::new()
                } else if let Some(source) = e.source {
                    format!("{}   ", source.clock_label())
                } else {
                    format!("{}–{}   ", clock(e.span.start), clock(e.span.end))
                };
                let title: String = e
                    .title
                    .chars()
                    .take(if e.series_id().is_some() { 24 } else { 36 })
                    .collect();
                rows.push((
                    format!("{time}{label}{title}"),
                    match e.kind {
                        Kind::Hold => ("Release", Action::Release(e.id)),
                        Kind::Weekly | Kind::Occurrence { .. } => {
                            ("Edit occurrence", Action::Edit(e.id, EditScope::ThisEvent))
                        }
                        _ => ("Edit", Action::Edit(e.id, EditScope::ThisEvent)),
                    },
                    e.series_id()
                        .map(|_| Action::Edit(e.id, EditScope::WholeSeries)),
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
        for (i, (label, (action_label, action), series)) in
            rows.iter().skip(page * PAGE).take(PAGE).enumerate()
        {
            let y = 254. + i as f32 * 38.;
            if !self.finding.get() && self.selected_row.get() == Some(page * PAGE + i) {
                rect(ctx, 8., y, 4., 31., theme.accent_fg());
            }
            ctx.add(pergola::node::Node::Rect {
                rect: pergola::geom::Rect::new(18., y + 31., 636., 1.),
                fill: theme.border_default(),
                radius: 0.,
            });
            Label::new(label).at(18., y + 6.).render(ctx);
            if let Some(a) = series {
                self.button(ctx, 393., y, 132., action_label, *action);
                self.button(ctx, 534., y, 120., "Whole series…", *a);
            } else {
                self.button(ctx, 534., y, 120., action_label, *action);
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
        import: Mutable::with_dirty(None, dirty.clone()),
        undo: Mutable::with_dirty(None, dirty.clone()),
        search: Mutable::with_dirty(None, dirty.clone()),
        selected_row: Mutable::with_dirty(None, dirty.clone()),
        zone_field: field(""),
        calendar_name,
        date: field(&today.to_string()),
        title: field(""),
        start: field("09:00"),
        end: field("09:30"),
        shown: Mutable::with_dirty(today, dirty.clone()),
        data: Mutable::with_dirty(None, dirty.clone()),
        view: Mutable::with_dirty(None, dirty.clone()),
        settings: Mutable::with_dirty(false, dirty.clone()),
        editor: Mutable::with_dirty(None, dirty.clone()),
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
    tx.send(Action::Today(None)).unwrap();
    let clock_path = socket.with_file_name("device-zone");
    let mut clock_name = std::fs::read_to_string(&clock_path).ok();
    let mut checked = std::time::Instant::now();
    let mut refresh_zone = false;
    let invitations = invitations::listen();
    let mut picker: Option<Picker> = None;
    let (parsed, previews) = mpsc::channel();

    loop {
        if matches!(
            planner.import.get_cloned(),
            None | Some(Screen::ReplyPrepared | Screen::InvitationProblem(_))
        ) && planner.editor.get_cloned().is_none()
            && planner.search.get_cloned().is_none()
            && !planner.settings.get()
            && !planner.waiting.get()
        {
            if let Some(day) = planner.data.get_cloned() {
                if let Ok(incoming) = invitations.try_recv() {
                    planner.import.set(Some(
                        match incoming.and_then(|i| i.review(&socket, day.revision)) {
                            Ok(sheet) => Screen::Invitation(sheet),
                            Err(e) => Screen::InvitationProblem(e),
                        },
                    ));
                }
            }
        }
        if let Some(result) = picker.as_mut().and_then(Picker::poll) {
            picker.take();
            match result {
                Ok(Some(file)) => {
                    planner.import.set(Some(Screen::Reading));
                    let tx = parsed.clone();
                    let path = socket.clone();
                    let revision = planner.data.get_cloned().unwrap().revision;
                    std::thread::spawn(move || {
                        let _ = tx.send(import::prepare(file, revision, &path));
                    });
                }
                Ok(None) => planner.import.set(None),
                Err(e) => planner.import.set(Some(Screen::Problem(e))),
            }
        }
        if let Ok(result) = previews.try_recv() {
            planner
                .import
                .set(Some(result.unwrap_or_else(Screen::Problem)));
        }
        if checked.elapsed() >= Duration::from_secs(2) {
            let current = std::fs::read_to_string(&clock_path).ok();
            refresh_zone |= current != clock_name;
            clock_name = current;
            checked = std::time::Instant::now();
        }
        if refresh_zone
            && planner.import.get_cloned().is_none()
            && !planner.waiting.get()
            && planner.editor.get_cloned().is_none()
            && planner.search.get_cloned().is_none()
            && !planner.settings.get()
        {
            refresh_zone = false;
            if let Some(data) = planner.data.get_cloned() {
                if data.pinned_zone.is_none() {
                    let was_today = data.date
                        == chrono::Utc::now()
                            .with_timezone(&data.viewing_zone)
                            .date_naive();
                    let _ = tx.send(if was_today {
                        Action::Today(Some(data.view))
                    } else {
                        Action::Resume
                    });
                }
            } else {
                let _ = tx.send(Action::Resume);
            }
        }
        while let Ok(action) = rx.try_recv() {
            if planner.waiting.get() {
                continue;
            }
            match action {
                Action::InvitationPage(page) => {
                    if let Some(Screen::Invitation(mut sheet)) = planner.import.get_cloned() {
                        sheet.set_page(page);
                        planner.import.set(Some(Screen::Invitation(sheet)));
                    }
                }
                Action::ReplyInvitation(answer) => {
                    if let Some(Screen::Invitation(sheet)) = planner.import.get_cloned() {
                        planner.import.set(Some(match sheet.reply(&socket, answer) {
                            Ok(()) => Screen::ReplyPrepared,
                            Err(e) => Screen::InvitationProblem(e),
                        }));
                    }
                }
                Action::PickImport => {
                    match Picker::open(&mut win, args.get(1).map_or("personal", String::as_str)) {
                        Ok(p) => {
                            picker = Some(p);
                            planner.import.set(Some(Screen::Picking));
                        }
                        Err(e) => planner.import.set(Some(Screen::Problem(e))),
                    }
                }
                Action::CloseImport => {
                    if let Some(Screen::Ready { token, .. }) = planner.import.get_cloned() {
                        let path = socket.clone();
                        std::thread::spawn(move || {
                            let _ = atrium_calendar::wire::discard_import(&path, token);
                        });
                    }
                    planner.import.set(None);
                    let _ = tx.send(Action::Resume);
                }
                Action::ImportPage(next) => {
                    if let Some(Screen::Ready {
                        data,
                        revision,
                        token,
                        ..
                    }) = planner.import.get_cloned()
                    {
                        if next < data.rows.len() {
                            planner.import.set(Some(Screen::Ready {
                                data,
                                revision,
                                token,
                                offset: next,
                            }));
                        }
                    }
                }
                Action::Edit(id, scope) => {
                    if let Err(e) = planner.open_editor(id, scope) {
                        planner.status.set(e);
                    }
                }
                Action::NewZoned => {
                    if let Err(e) = planner.new_zoned() {
                        planner.status.set(e);
                    }
                }
                Action::ChooseFold(end, choice) => {
                    if let Some(mut edit) = planner.editor.get_cloned() {
                        edit.choose_fold(end, choice);
                        planner.editor.set(Some(edit));
                        planner.status.set(String::new());
                    }
                }
                Action::Review(stage) => {
                    if let Err(e) = planner.review(stage) {
                        planner.status.set(e);
                    }
                }
                Action::CloseEditor => {
                    planner.editor.set(None);
                    let _ = tx.send(Action::Resume);
                }
                Action::SearchPanel(open) => {
                    planner.search.set(open.then(|| SearchForm::new(&planner)));
                    planner.status.set(String::new());
                }
                Action::Find => {
                    planner.finding.set(!planner.finding.get());
                    planner.page.set(0);
                }
                Action::Settings(open) => {
                    if open {
                        planner
                            .zone_field
                            .set(TextEdit::new(planner.viewing_zone().name()));
                    }
                    planner.settings.set(open);
                    planner.status.set(String::new());
                }
                Action::Page(p) => planner.page.set(p),
                action => match planner.request(action) {
                    Ok(req) => {
                        planner.waiting.set(true);
                        let tx = done.clone();
                        let path = socket.clone();
                        std::thread::spawn(move || {
                            let reply = match req {
                                Request::Search {
                                    query,
                                    offset,
                                    revision,
                                } => atrium_calendar::wire::search(
                                    &path,
                                    query.clone(),
                                    offset,
                                    revision,
                                )
                                .map(|page| {
                                    Reply::Results(Results {
                                        query,
                                        offset,
                                        page,
                                    })
                                }),
                                req => page(&path, &req).map(Reply::Calendar),
                            };
                            let _ = tx.send((action, reply));
                        });
                    }
                    Err(e) => planner.status.set(e),
                },
            }
        }
        while let Ok((action, reply)) = answers.try_recv() {
            planner.waiting.set(false);
            match reply {
                Ok(Reply::Results(results)) => {
                    if let Some(mut form) = planner.search.get_cloned() {
                        form.results = Some(results);
                        planner.search.set(Some(form));
                    }
                    planner.status.set(String::new());
                }
                Ok(Reply::Calendar(data)) => {
                    planner.import.set(None);
                    let date = data.date;
                    if let Some(receipt) = data.undo {
                        let label = match action {
                            Action::ApplyInvitation { .. } => "Undo invitation change",
                            Action::SaveImport => "Undo import",
                            Action::Change(true) => "Undo deletion",
                            Action::Change(false) => "Undo edit",
                            Action::Hold(_) => "Undo hold",
                            Action::Release(_) => "Undo release",
                            _ => "Undo creation",
                        };
                        planner.undo.set(Some((receipt, label)));
                    } else if planner
                        .undo
                        .get()
                        .is_some_and(|(r, _)| r.revision != data.revision)
                        || matches!(action, Action::Undo(_))
                    {
                        planner.undo.set(None);
                    }
                    let selected = match action {
                        Action::OpenHit(id, ..) => data.entries.iter().position(|e| e.id == id),
                        _ => None,
                    };
                    planner.selected_row.set(selected);
                    planner.search.set(None);
                    planner.view.set(Some(data.view));
                    if matches!(
                        action,
                        Action::Undo(_)
                            | Action::ApplyInvitation { .. }
                            | Action::SaveImport
                            | Action::Today(_)
                            | Action::SetZone(_)
                            | Action::Resume
                            | Action::Load
                            | Action::Browse(..)
                            | Action::Change(_)
                            | Action::OpenHit(..)
                            | Action::CreateZoned(_)
                    ) {
                        planner.date.set(TextEdit::new(&date.to_string()));
                        planner.finding.set(false);
                    }
                    planner.settings.set(false);
                    planner.editor.set(None);
                    planner.shown.set(date);
                    planner.data.set(Some(data));
                    planner.page.set(selected.unwrap_or(0) / PAGE);
                    if matches!(action, Action::Save(_) | Action::CreateZoned(_)) {
                        planner.title.set(TextEdit::new(""));
                    }
                    planner.status.set(
                        match action {
                            Action::ApplyInvitation { cancellation: true } => {
                                "Cancellation applied on this device"
                            }
                            Action::ApplyInvitation {
                                cancellation: false,
                            } => "Invitation saved on this device",
                            Action::SaveImport => "Local copies saved",
                            Action::Undo(_) => "Change undone on this device",
                            Action::OpenHit(..) => "Selected search result",
                            Action::Change(true) => "Deleted on this device",
                            Action::Change(false) => "Changes saved on this device",
                            Action::SetZone(_) => "Viewing time zone saved",
                            Action::SetOpening(_) => "Opening view saved",
                            Action::Hold(_) => "Held on this device",
                            Action::Release(_) => "Hold released",
                            Action::Save(_) | Action::CreateZoned(_) => "Event saved",
                            _ => "On this device",
                        }
                        .into(),
                    );
                }
                Err(e) if matches!(action, Action::ApplyInvitation { .. }) => {
                    planner.import.set(Some(Screen::InvitationProblem(e)))
                }
                Err(e) if matches!(action, Action::SaveImport) => {
                    planner.import.set(Some(Screen::Problem(e)))
                }
                Err(e) if matches!(action, Action::Search(_) | Action::OpenHit(..)) => {
                    if let Some(mut form) = planner.search.get_cloned() {
                        form.results = None;
                        planner.search.set(Some(form));
                    }
                    planner.status.set(e);
                }
                Err(e) => {
                    if matches!(action, Action::Undo(_))
                        && e == atrium_calendar::store::UNDO_EXPIRED
                    {
                        planner.undo.set(None);
                    }
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
