//! Local changes use the revision and event selected from the agenda.
use super::*;
use atrium_calendar::{wire::EventChange, zone::alternatives, Fold, Zone};
use chrono::{LocalResult, Offset};
use chrono_tz::Tz;
type FoldSelection = (Tz, NaiveDate, u16, Fold);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Form,
    ReviewChange,
    ReviewDelete,
}

#[derive(Clone)]
pub(super) struct Editor {
    creating: bool,
    zone: Option<Mutable<TextEdit>>,
    choices: [Option<FoldSelection>; 2],
    id: Uuid,
    revision: u64,
    shown: NaiveDate,
    scope: EditScope,
    kind: Kind,
    before: EventEdit,
    date: Mutable<TextEdit>,
    title: Mutable<TextEdit>,
    start: Mutable<TextEdit>,
    end: Mutable<TextEdit>,
    stage: Stage,
}

impl Editor {
    fn endpoint(&self, end: bool) -> Result<(Tz, NaiveDate, u16), String> {
        let name = self
            .zone
            .as_ref()
            .ok_or("Missing source zone")?
            .get_cloned();
        let input = name.text().trim();
        let tz = atrium_calendar::zone::parse_name(input)
            .ok_or("Enter an IANA zone, such as America/New_York")?;
        let date = self
            .date
            .get_cloned()
            .text()
            .trim()
            .parse()
            .map_err(|_| "Enter a date as YYYY-MM-DD")?;
        Ok((
            tz,
            date,
            time(&if end {
                self.end.get_cloned()
            } else {
                self.start.get_cloned()
            })?,
        ))
    }

    pub(super) fn choose_fold(&mut self, end: bool, choice: Fold) {
        if let Ok((tz, date, minute)) = self.endpoint(end) {
            self.choices[usize::from(end)] = Some((tz, date, minute, choice));
        }
    }

    fn zone(&self) -> Result<Option<Zone>, String> {
        if self.zone.is_none() {
            return Ok(None);
        }
        let (name, date, start) = self.endpoint(false)?;
        let (_, _, end) = self.endpoint(true)?;
        let selected = |index: usize, minute| {
            self.choices[index]
                .filter(|(z, d, m, _)| (*z, *d, *m) == (name, date, minute))
                .map(|(_, _, _, c)| c)
        };
        Ok(Some(Zone {
            name,
            start_fold: selected(0, start),
            end_fold: selected(1, end),
        }))
    }

    pub(super) fn create(&self, kind: Kind) -> Result<Request, String> {
        if !self.creating || !matches!(kind, Kind::Appointment | Kind::Weekly) {
            return Err("Open the zoned event form first".into());
        }
        let f = self.draft()?;
        let entry = Entry {
            id: self.id,
            date: f.date,
            title: f.title,
            span: f.span,
            zone: f.zone,
            source: None,
            kind,
            import_hash: None,
            invitation: None,
            excluded: Vec::new(),
        };
        Ok(Request::Add {
            revision: self.revision,
            entry,
        })
    }

    pub(super) fn check_saved(&self) -> Result<(), String> {
        if self.draft()? != self.before {
            return Err("Save your edits before changing the reminder".into());
        }
        Ok(())
    }

    fn draft(&self) -> Result<EventEdit, String> {
        let fields = EventEdit {
            zone: self.zone()?,
            date: self
                .date
                .get_cloned()
                .text()
                .trim()
                .parse()
                .map_err(|_| "Enter a date as YYYY-MM-DD")?,
            title: self.title.get_cloned().text().trim().to_owned(),
            span: Span {
                start: time(&self.start.get_cloned())?,
                end: time(&self.end.get_cloned())?,
            },
        };
        Entry {
            id: self.id,
            date: fields.date,
            span: fields.span,
            title: fields.title.clone(),
            kind: self.kind,
            import_hash: None,
            invitation: None,
            excluded: Vec::new(),
            zone: fields.zone,
            source: None,
        }
        .validate()?;
        Ok(fields)
    }

    pub(super) fn request(&self, delete: bool) -> Result<Request, String> {
        if delete && self.stage != Stage::ReviewDelete
            || !delete && self.scope == EditScope::WholeSeries && self.stage != Stage::ReviewChange
        {
            return Err("Review the change first".into());
        }
        let replacement = if delete { None } else { Some(self.draft()?) };
        Ok(Request::Change {
            revision: self.revision,
            id: self.id,
            shown: self.shown,
            scope: self.scope,
            operation: replacement.map_or(EventChange::Delete, EventChange::Update),
        })
    }

    fn subject(&self) -> &'static str {
        match (self.scope, self.kind) {
            (EditScope::WholeSeries, _) => "whole weekly series",
            (_, Kind::Weekly | Kind::Occurrence { .. }) => "this occurrence",
            _ => "event",
        }
    }
}

impl Planner {
    pub(super) fn open_editor(&self, id: Uuid, scope: EditScope) -> Result<(), String> {
        let data = self.data.get_cloned().ok_or("Refresh the day first")?;
        let shown = self.shown.get();
        let selected = data
            .on(shown)
            .find(|e| e.id == id)
            .ok_or("Refresh the event first")?;
        let before = if scope == EditScope::WholeSeries {
            let series = selected.series_id().ok_or("Select a weekly series")?;
            data.entries
                .iter()
                .find(|e| e.id == series)
                .map(|e| {
                    let mut fields = e.fields_on(e.date);
                    if let Some(s) = e.source {
                        fields.date = s.first;
                    }
                    fields
                })
                .or_else(|| data.series.get(&series).cloned())
                .ok_or("Refresh the series first")?
        } else {
            selected.fields_on(shown)
        };
        self.begin_editor(
            id,
            data.revision,
            shown,
            scope,
            selected.kind,
            before,
            false,
        );
        Ok(())
    }

    pub(super) fn new_zoned(&self) -> Result<(), String> {
        let shown = self.shown.get();
        let revision = self
            .data
            .get_cloned()
            .ok_or("Refresh the calendar first")?
            .revision;
        let before = EventEdit {
            date: shown,
            title: self.title.get_cloned().text().to_owned(),
            span: Span {
                start: time(&self.start.get_cloned())?,
                end: time(&self.end.get_cloned())?,
            },
            zone: Some(Zone {
                name: self.viewing_zone(),
                start_fold: None,
                end_fold: None,
            }),
        };
        self.begin_editor(
            Uuid::new_v4(),
            revision,
            shown,
            EditScope::ThisEvent,
            Kind::Appointment,
            before,
            true,
        );
        Ok(())
    }

    fn begin_editor(
        &self,
        id: Uuid,
        revision: u64,
        shown: NaiveDate,
        scope: EditScope,
        kind: Kind,
        before: EventEdit,
        creating: bool,
    ) {
        let field = |value: &str| self.title.sibling(TextEdit::new(value));
        let choices = if let Some(z) = before.zone {
            [
                z.start_fold
                    .map(|f| (z.name, before.date, before.span.start, f)),
                z.end_fold
                    .map(|f| (z.name, before.date, before.span.end, f)),
            ]
        } else {
            [None, None]
        };
        self.editor.set(Some(Editor {
            id,
            revision,
            shown,
            scope,
            kind,
            creating,
            choices,
            zone: before.zone.map(|z| field(z.name.name())),
            date: field(&before.date.to_string()),
            title: field(&before.title),
            start: field(&clock(before.span.start)),
            end: field(&clock(before.span.end)),
            before,
            stage: Stage::Form,
        }));
        self.status.set(String::new());
    }

    pub(super) fn review(&self, stage: Stage) -> Result<(), String> {
        let mut edit = self.editor.get_cloned().ok_or("Open the event first")?;
        if stage == Stage::ReviewChange {
            edit.draft()?;
        }
        edit.stage = stage;
        self.editor.set(Some(edit));
        self.status.set(String::new());
        Ok(())
    }

    pub(super) fn render_editor(&self, ctx: &mut Ctx, edit: &Editor) {
        self.button(ctx, 18., 44., 115., "Cancel", Action::CloseEditor);
        let subject = edit.subject();
        let heading = match edit.stage {
            Stage::Form if edit.creating => "New zoned meeting".into(),
            Stage::Form => format!("Edit {subject}"),
            Stage::ReviewChange => "Review weekly series change".into(),
            Stage::ReviewDelete => format!("Delete {subject}?"),
        };
        Label::new(heading).at(18., 113.).size(21.).render(ctx);
        Label::new(self.status.get_cloned())
            .at(18., 560.)
            .render(ctx);
        let whole = edit.scope == EditScope::WholeSeries;
        if edit.stage != Stage::Form {
            let mut bottom = self.edit_summary(ctx, 156., "Currently", &edit.before);
            if edit.stage == Stage::ReviewChange {
                if let Ok(draft) = edit.draft() {
                    bottom = self.edit_summary(ctx, bottom.max(250.), "Change to", &draft);
                }
            }
            let detail = match (edit.stage, whole) {
                (Stage::ReviewDelete, true) => {
                    "All occurrences, including individually moved meetings, will be removed."
                }
                (Stage::ReviewChange, true) => {
                    "Individually edited and skipped dates keep their exceptions."
                }
                (_, false)
                    if edit.kind == Kind::Weekly
                        || matches!(edit.kind, Kind::Occurrence { .. }) =>
                {
                    "The other occurrences keep their schedules."
                }
                _ => "This event will be removed from this calendar.",
            };
            let y = bottom.max(363.);
            Label::new(detail).at(18., y).render(ctx);
            let delete = edit.stage == Stage::ReviewDelete;
            self.button(
                ctx,
                18.,
                y + 38.,
                148.,
                "Back to editor",
                Action::Review(Stage::Form),
            );
            self.button(
                ctx,
                181.,
                y + 38.,
                185.,
                if delete {
                    "Confirm deletion"
                } else {
                    "Save series changes"
                },
                Action::Change(delete),
            );
            return;
        }
        Label::new("Title").at(18., 160.).render(ctx);
        TextField::new(edit.title.clone())
            .key(100)
            .autofocus(true)
            .at(18., 177.)
            .width(450.)
            .render(ctx);
        Label::new(if whole { "Series starts" } else { "Date" })
            .at(18., 238.)
            .render(ctx);
        if whole {
            Label::new(edit.before.date.to_string())
                .at(18., 270.)
                .render(ctx);
        } else {
            TextField::new(edit.date.clone())
                .key(101)
                .at(18., 256.)
                .width(146.)
                .render(ctx);
        }
        if edit.kind == Kind::AllDay {
            Label::new("All day").at(184., 270.).render(ctx);
        } else {
            for (x, label, field, key) in [
                (185., "Start", &edit.start, 102),
                (294., "End", &edit.end, 103),
            ] {
                Label::new(label).at(x, 238.).render(ctx);
                TextField::new(field.clone())
                    .key(key)
                    .at(x, 256.)
                    .width(91.)
                    .render(ctx);
            }
        }
        let button_y = if let Some(zone) = &edit.zone {
            Label::new("Source time zone").at(404., 238.).render(ctx);
            TextField::new(zone.clone())
                .key(104)
                .at(404., 256.)
                .width(268.)
                .render(ctx);
            self.render_zone(ctx, edit);
            492.
        } else {
            325.
        };
        if edit.creating {
            self.control(
                ctx,
                Button::primary("Save event"),
                18.,
                button_y,
                145.,
                Action::CreateZoned(Kind::Appointment),
            );
            self.button(
                ctx,
                180.,
                button_y,
                145.,
                "Save weekly",
                Action::CreateZoned(Kind::Weekly),
            );
            return;
        }
        if !whole
            && matches!(
                edit.kind,
                Kind::Appointment | Kind::Weekly | Kind::Occurrence { .. }
            )
        {
            if let Some(page) = self.data.get_cloned() {
                if let Some(row) = page.on(edit.shown).find(|r| r.id == edit.id) {
                    let slot = atrium_calendar::store::reminders::Slot::for_row(&row, edit.shown);
                    let enabled = page.reminders.contains(&slot);
                    self.button(
                        ctx,
                        345.,
                        button_y,
                        235.,
                        if enabled {
                            "Turn reminder off"
                        } else {
                            "Remind at saved start"
                        },
                        Action::SetReminder {
                            revision: edit.revision,
                            id: edit.id,
                            shown: edit.shown,
                            enabled: !enabled,
                        },
                    );
                }
            }
        }
        let action = if whole {
            Action::Review(Stage::ReviewChange)
        } else {
            Action::Change(false)
        };
        self.control(
            ctx,
            Button::primary(if whole {
                "Review changes…"
            } else {
                "Save changes"
            }),
            18.,
            button_y,
            151.,
            action,
        );
        self.button(
            ctx,
            187.,
            button_y,
            147.,
            "Delete…",
            Action::Review(Stage::ReviewDelete),
        );
    }

    fn render_zone(&self, ctx: &mut Ctx, edit: &Editor) {
        for (end, y, label) in [(false, 321., "Start"), (true, 375., "End")] {
            let (name, date, minute) = match edit.endpoint(end) {
                Ok(p) => p,
                Err(e) => {
                    Label::new(e).at(18., y).render(ctx);
                    return;
                }
            };
            match alternatives(name, date, minute) {
                LocalResult::None => {
                    Label::new(format!(
                        "{label} time does not exist on this date in {name}."
                    ))
                    .at(18., y)
                    .render(ctx);
                }
                LocalResult::Single(at) => {
                    Label::new(format!(
                        "{label}   UTC{}   ·   {}",
                        at.offset().fix(),
                        at.to_utc().format("%Y-%m-%d %H:%M UTC")
                    ))
                    .at(18., y)
                    .render(ctx);
                }
                LocalResult::Ambiguous(first, second) => {
                    Label::new(format!("{label} repeats. Choose an offset"))
                        .at(18., y)
                        .render(ctx);
                    for (x, fold, at) in [(254., Fold::Earlier, first), (467., Fold::Later, second)]
                    {
                        let label = format!(
                            "{} · UTC{}",
                            if fold == Fold::Earlier {
                                "First"
                            } else {
                                "Second"
                            },
                            at.offset().fix()
                        );
                        let button =
                            if edit.choices[usize::from(end)] == Some((name, date, minute, fold)) {
                                Button::primary(label)
                            } else {
                                Button::secondary(label)
                            };
                        self.control(ctx, button, x, y - 15., 199., Action::ChooseFold(end, fold));
                    }
                }
            }
        }
        if let Ok(f) = edit.draft() {
            if let Some(z) = f.zone {
                if let Ok((start, end)) = z.interval(f.date, f.span, false) {
                    Label::new(format!(
                        "Agenda   {} – {}",
                        start
                            .with_timezone(&self.viewing_zone())
                            .format("%b %d %H:%M %Z"),
                        end.with_timezone(&self.viewing_zone())
                            .format("%b %d %H:%M %Z")
                    ))
                    .at(18., 448.)
                    .render(ctx);
                }
            }
        }
    }

    fn edit_summary(&self, ctx: &mut Ctx, y: f32, label: &str, fields: &EventEdit) -> f32 {
        Label::new(label)
            .at(18., y)
            .color(ctx.theme.text_secondary())
            .render(ctx);
        let mut line_y = y + 24.;
        let chars: Vec<_> = fields.title.chars().collect();
        for line in chars.chunks(45) {
            Label::new(line.iter().collect::<String>())
                .at(18., line_y)
                .size(14.)
                .render(ctx);
            line_y += 19.;
        }
        Label::new(format!(
            "{}   {}–{} {}",
            fields.date,
            clock(fields.span.start),
            clock(fields.span.end),
            fields.zone.map(|z| z.name.name()).unwrap_or("")
        ))
        .at(18., line_y + 5.)
        .render(ctx);
        if let Some(zone) = fields.zone {
            if let Ok((start, end)) = zone.interval(fields.date, fields.span, false) {
                line_y += 20.;
                Label::new(format!(
                    "{} – {} UTC",
                    start.format("%b %d %H:%M"),
                    end.format("%b %d %H:%M")
                ))
                .at(18., line_y + 5.)
                .render(ctx);
            }
        }
        line_y + 38.
    }
}
