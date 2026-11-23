//! Local changes use the revision and event selected from the agenda.
use super::*;
use atrium_calendar::wire::EventChange;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Form,
    ReviewChange,
    ReviewDelete,
}

#[derive(Clone)]
pub(super) struct Editor {
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
    fn draft(&self) -> Result<EventEdit, String> {
        let fields = EventEdit {
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
            excluded: Vec::new(),
        }
        .validate()?;
        Ok(fields)
    }

    pub(super) fn request(&self, delete: bool) -> Result<(NaiveDate, Request), String> {
        if delete && self.stage != Stage::ReviewDelete
            || !delete && self.scope == EditScope::WholeSeries && self.stage != Stage::ReviewChange
        {
            return Err("Review the change first".into());
        }
        let replacement = if delete { None } else { Some(self.draft()?) };
        let date = replacement
            .as_ref()
            .filter(|_| self.scope == EditScope::ThisEvent)
            .map_or(self.shown, |f| f.date);
        Ok((
            date,
            Request::Change {
                revision: self.revision,
                id: self.id,
                shown: self.shown,
                scope: self.scope,
                operation: replacement.map_or(EventChange::Delete, EventChange::Update),
            },
        ))
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
                .map(|e| e.fields_on(e.date))
                .or_else(|| data.series.get(&series).cloned())
                .ok_or("Refresh the series first")?
        } else {
            selected.fields_on(shown)
        };
        let field = |value: &str| self.title.sibling(TextEdit::new(value));
        self.editor.set(Some(Editor {
            id,
            revision: data.revision,
            shown,
            scope,
            kind: selected.kind,
            date: field(&before.date.to_string()),
            title: field(&before.title),
            start: field(&clock(before.span.start)),
            end: field(&clock(before.span.end)),
            before,
            stage: Stage::Form,
        }));
        self.status.set(String::new());
        Ok(())
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
            325.,
            151.,
            action,
        );
        self.button(
            ctx,
            187.,
            325.,
            147.,
            "Delete…",
            Action::Review(Stage::ReviewDelete),
        );
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
            "{}   {}–{}",
            fields.date,
            clock(fields.span.start),
            clock(fields.span.end)
        ))
        .at(18., line_y + 5.)
        .render(ctx);
        line_y + 38.
    }
}
