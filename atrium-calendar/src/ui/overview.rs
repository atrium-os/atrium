//! Week and Month are date selectors over the same stored commitments as Today.
use super::*;
use pergola::{geom::Rect, node::Node, Color};

pub(super) fn rect(ctx: &mut Ctx, x: f32, y: f32, w: f32, h: f32, fill: Color) {
    ctx.add(Node::Rect {
        rect: Rect::new(x, y, w, h),
        fill,
        radius: 0.,
    });
}

impl Planner {
    pub(super) fn overview(&self, ctx: &mut Ctx, shown: NaiveDate, data: &CalendarPage) {
        let view = data.view;
        TextField::new(self.date.clone())
            .at(18., 102.)
            .width(137.)
            .render(ctx);
        self.button(ctx, 163., 102., 105., "Go to date", Action::Load);
        for (forward, x, label) in [(false, 462., "Previous"), (true, 578., "Next")] {
            if let Some(date) = view.adjacent(shown, forward) {
                self.button(ctx, x, 102., 104., label, Action::Browse(view, date));
            }
        }
        if view == CalendarView::Week {
            self.week(ctx, shown, data);
        } else {
            self.month(ctx, shown, data);
        }
    }

    fn week(&self, ctx: &mut Ctx, shown: NaiveDate, data: &CalendarPage) {
        let theme = ctx.theme;
        Label::new("Day").at(18., 158.).render(ctx);
        Label::new("Agenda").at(134., 158.).render(ctx);
        for (x, label) in [(446., "00:00"), (543., "12:00"), (644., "24:00")] {
            Label::new(label)
                .at(x, 158.)
                .color(theme.text_secondary())
                .render(ctx);
        }
        for (i, date) in data.view.dates(shown).enumerate() {
            let y = 178. + i as f32 * 52.;
            let label = date.format("%a %d").to_string();
            let button = if date == shown {
                Button::primary(label)
            } else {
                Button::secondary(label)
            };
            self.control(
                ctx,
                button,
                18.,
                y,
                96.,
                Action::Browse(CalendarView::Today, date),
            );
            let mut entries = data.on(date);
            if let Some(first) = entries.next() {
                let title: String = first.title.chars().take(24).collect();
                let extra = if first.title.chars().count() > 24 {
                    "…"
                } else {
                    ""
                };
                Label::new(format!("{title}{extra}"))
                    .at(134., y + 5.)
                    .size(14.)
                    .render(ctx);
                let mut detail = if first.kind == Kind::AllDay {
                    "All day".into()
                } else {
                    format!(
                        "{}–{}{}",
                        clock(first.span.start),
                        clock(first.span.end),
                        if first.zone.is_some() { " UTC" } else { "" }
                    )
                };
                let more = entries.count();
                if more > 0 {
                    detail.push_str(&format!("   +{more} more"));
                }
                let levels = data.half_hours(date);
                if levels.contains(&2) {
                    detail.push_str("   Overlap");
                }
                Label::new(detail)
                    .at(134., y + 26.)
                    .color(theme.text_secondary())
                    .render(ctx);
                for (slot, level) in levels.iter().enumerate() {
                    let fill = match level {
                        0 => theme.border_default(),
                        1 => theme.accent_mark(),
                        _ => theme.warning_fg(),
                    };
                    rect(ctx, 446. + slot as f32 * 4.9, y + 14., 4.9, 8., fill);
                }
            } else {
                Label::new("Open")
                    .at(134., y + 13.)
                    .color(theme.text_secondary())
                    .render(ctx);
                rect(ctx, 446., y + 17., 235., 2., theme.border_default());
            }
            rect(ctx, 18., y + 43., 664., 1., theme.border_default());
        }
    }

    fn month(&self, ctx: &mut Ctx, shown: NaiveDate, data: &CalendarPage) {
        let theme = ctx.theme;
        const WIDTH: f32 = 95.;
        for (column, weekday) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
            .iter()
            .enumerate()
        {
            Label::new(*weekday)
                .at(49. + column as f32 * WIDTH, 159.)
                .render(ctx);
        }
        let offset = shown.with_day(1).unwrap().weekday().num_days_from_monday() as usize;
        for (i, date) in data.view.dates(shown).enumerate() {
            let slot = offset + i;
            let x = 18. + (slot % 7) as f32 * WIDTH;
            let y = 177. + (slot / 7) as f32 * 60.;
            let number = date.day().to_string();
            let button = if date == shown {
                Button::primary(number)
            } else {
                Button::ghost(number)
            };
            self.control(
                ctx,
                button,
                x,
                y,
                WIDTH - 5.,
                Action::Browse(CalendarView::Today, date),
            );
            let mut entries = data.on(date);
            let count = entries.clone().count();
            if count != 0 {
                let label = match count {
                    n if entries.any(|e| e.kind == Kind::AllDay) => {
                        if n == 1 {
                            "All day".into()
                        } else {
                            format!("All day · {n}")
                        }
                    }
                    1 => "1 entry".into(),
                    n => format!("{n} entries"),
                };
                Label::new(label)
                    .at(x + 10., y + 40.)
                    .color(theme.accent_fg())
                    .render(ctx);
            }
            rect(ctx, x, y + 54., WIDTH - 5., 1., theme.border_default());
        }
    }
}
