//! A bounded civil-date view. Each recurring series travels once in a reply.
use super::{Day, Entry, EventEdit};
use chrono::{Datelike, Days, Months, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalendarView {
    #[default]
    Today,
    Week,
    Month,
}

impl CalendarView {
    pub const ALL: [Self; 3] = [Self::Today, Self::Week, Self::Month];

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Week => "Week",
            Self::Month => "Month",
        }
    }

    pub fn dates(self, anchor: NaiveDate) -> impl Iterator<Item = NaiveDate> {
        let (start, end) = match self {
            Self::Today => (anchor, anchor),
            Self::Week => {
                let offset = u64::from(anchor.weekday().num_days_from_monday());
                (
                    anchor
                        .checked_sub_days(Days::new(offset))
                        .unwrap_or(NaiveDate::MIN),
                    anchor
                        .checked_add_days(Days::new(6 - offset))
                        .unwrap_or(NaiveDate::MAX),
                )
            }
            Self::Month => {
                let first = anchor.with_day(1).unwrap();
                let last = first
                    .checked_add_months(Months::new(1))
                    .and_then(|next| next.pred_opt())
                    .unwrap_or(NaiveDate::MAX);
                (first, last)
            }
        };
        (0..=(end - start).num_days() as u64).map(move |n| start + Days::new(n))
    }

    pub fn adjacent(self, anchor: NaiveDate, forward: bool) -> Option<NaiveDate> {
        if self == Self::Month {
            let month = Months::new(1);
            return if forward {
                anchor.checked_add_months(month)
            } else {
                anchor.checked_sub_months(month)
            };
        }
        let step = Days::new(if self == Self::Week { 7 } else { 1 });
        if forward {
            anchor.checked_add_days(step)
        } else {
            anchor.checked_sub_days(step)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalendarPage {
    pub revision: u64,
    pub entries: Vec<Entry>,
    pub view: CalendarView,
    pub opening_view: CalendarView,
    /// Original schedule for moved occurrences whose series is outside this page.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub series: BTreeMap<Uuid, EventEdit>,
}

impl CalendarPage {
    /// Fasti sorted the series by civil start, end and ID before sending them.
    pub fn on(&self, date: NaiveDate) -> impl Iterator<Item = &Entry> + Clone {
        self.entries.iter().filter(move |e| e.appears(date))
    }

    /// Peak simultaneous commitments per half-hour. A pair of adjacent
    /// meetings within one bin is still a single busy interval.
    pub fn half_hours(&self, date: NaiveDate) -> [u8; 48] {
        let mut changes = [0i32; 1441];
        for entry in self.on(date) {
            changes[usize::from(entry.span.start)] += 1;
            changes[usize::from(entry.span.end)] -= 1;
        }
        let mut levels = [0; 48];
        let mut active = 0;
        for (minute, delta) in changes[..1440].iter().enumerate() {
            active += delta;
            levels[minute / 30] = levels[minute / 30].max(active.min(2) as u8);
        }
        levels
    }

    pub fn into_day(self) -> Day {
        Day {
            revision: self.revision,
            entries: self.entries,
        }
    }
}
