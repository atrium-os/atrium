//! Search fields and results belong to this window and are discarded on close.
use super::*;
use atrium_calendar::{SearchPage, SearchQuery, SEARCH_PAGE_SIZE};

#[derive(Clone)]
pub(super) struct Results {
    pub query: SearchQuery,
    pub offset: usize,
    pub page: SearchPage,
}

#[derive(Clone)]
pub(super) struct SearchForm {
    text: Mutable<TextEdit>,
    start: Mutable<TextEdit>,
    end: Mutable<TextEdit>,
    pub results: Option<Results>,
}

impl SearchForm {
    pub fn new(planner: &Planner) -> Self {
        let mut dates = CalendarView::Month.dates(planner.shown.get());
        let first = dates.next().unwrap();
        let last = dates.last().unwrap_or(first);
        let field = |value: &str| planner.title.sibling(TextEdit::new(value));
        Self {
            text: field(""),
            start: field(&first.to_string()),
            end: field(&last.to_string()),
            results: None,
        }
    }

    fn query(&self) -> Result<SearchQuery, String> {
        let date = |field: &Mutable<TextEdit>| {
            field
                .get_cloned()
                .text()
                .trim()
                .parse::<NaiveDate>()
                .map_err(|_| "Enter both dates as YYYY-MM-DD".to_string())
        };
        Ok(SearchQuery {
            text: self.text.get_cloned().text().trim().to_owned(),
            start: date(&self.start)?,
            end: date(&self.end)?,
        })
    }

    pub fn request(&self, offset: Option<usize>) -> Result<Request, String> {
        let query = self.query()?;
        let revision = if offset.is_none() {
            None
        } else {
            Some(
                self.results
                    .as_ref()
                    .filter(|r| r.query == query)
                    .ok_or("Search again with these fields")?
                    .page
                    .revision,
            )
        };
        Ok(Request::Search {
            query,
            offset: offset.unwrap_or(0),
            revision,
        })
    }

    pub fn render(&self, planner: &Planner, ctx: &mut Ctx) {
        planner.button(
            ctx,
            18.,
            44.,
            150.,
            "Back to calendar",
            Action::SearchPanel(false),
        );
        for (label, field, x, width, key) in [
            ("Event title", &self.text, 18., 286., 200),
            ("From", &self.start, 316., 123., 201),
            ("Through", &self.end, 451., 123., 202),
        ] {
            Label::new(label).at(x, 110.).render(ctx);
            let tx = planner.actions.clone();
            TextField::new(field.clone())
                .key(key)
                .autofocus(key == 200)
                .at(x, 129.)
                .width(width)
                .on_submit(move |_| {
                    let _ = tx.send(Action::Search(None));
                })
                .render(ctx);
        }
        planner.control(
            ctx,
            Button::primary("Search"),
            586.,
            129.,
            94.,
            Action::Search(None),
        );
        Label::new(planner.status.get_cloned())
            .at(18., 565.)
            .render(ctx);
        let Some(results) = self
            .results
            .as_ref()
            .filter(|r| self.query().as_ref() == Ok(&r.query))
        else {
            return;
        };
        let Results { offset, page, .. } = results;
        if page.hits.is_empty() {
            Label::new("No matching titles in these dates")
                .at(18., 204.)
                .render(ctx);
            return;
        }
        Label::new(format!(
            "{}–{} of {}",
            offset + 1,
            offset + page.hits.len(),
            page.total
        ))
        .at(18., 200.)
        .render(ctx);
        for (i, hit) in page.hits.iter().enumerate() {
            let y = 227. + 51. * i as f32;
            let mut title = hit.title.chars().take(48).collect::<String>();
            if title.len() < hit.title.len() {
                title.push('…');
            }
            Label::new(title).at(18., y).render(ctx);
            let when = if hit.kind == Kind::AllDay {
                "All day".into()
            } else if let Some(source) = hit.source {
                source.clock_label()
            } else {
                format!("{}–{}", clock(hit.span.start), clock(hit.span.end))
            };
            let detail = match hit.kind {
                Kind::Occurrence { original, .. } if original != hit.date => {
                    format!(" · Moved from {original}")
                }
                Kind::Occurrence { .. } => " · Edited occurrence".into(),
                Kind::Weekly => " · Weekly".into(),
                Kind::Hold => " · Private hold".into(),
                _ => String::new(),
            };
            let basis = hit
                .source_zone
                .map(|z| format!(" {} · {z}", page.viewing_zone))
                .unwrap_or_default();
            Label::new(format!("{}   {when}{basis}{detail}", hit.date))
                .at(18., y + 21.)
                .color(ctx.theme.text_secondary())
                .render(ctx);
            planner.button(
                ctx,
                549.,
                y - 4.,
                131.,
                "Open in day",
                Action::OpenHit(hit.id, hit.date, page.revision),
            );
            rect(ctx, 18., y + 36., 662., 1., ctx.theme.border_default());
        }
        if *offset > 0 {
            planner.button(
                ctx,
                18.,
                504.,
                114.,
                "Previous",
                Action::Search(Some(offset.saturating_sub(SEARCH_PAGE_SIZE))),
            );
        }
        if offset + page.hits.len() < page.total {
            planner.button(
                ctx,
                144.,
                504.,
                114.,
                "Next",
                Action::Search(Some(offset + SEARCH_PAGE_SIZE)),
            );
        }
    }
}
