//! Each opt-in belongs to a saved meeting (or its original recurrence slot).
//! The trigger and private receipt share the atomic calendar snapshot.
use super::*;
use crate::Zone;
use chrono::{DateTime, Utc};

const MAX_REMINDERS: usize = 256;
const FRESH_SECONDS: i64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    event: Uuid,
    original: Option<NaiveDate>,
}

impl Slot {
    pub fn for_row(row: &Entry, shown: NaiveDate) -> Self {
        match row.kind {
            Kind::Occurrence { series, original } => Self {
                event: series,
                original: Some(original),
            },
            Kind::Weekly => Self {
                event: row.id,
                original: Some(row.source.map_or(shown, |s| s.date)),
            },
            _ => Self {
                event: row.id,
                original: None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Trigger {
    due: i64,
    end: i64,
    organizer_revision: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Alarm {
    pub(super) slot: Slot,
    revision: u64,
    trigger: Option<Trigger>,
    received: Option<i64>,
}

impl Alarm {
    pub(super) fn same_trigger(&self, other: &Self) -> bool {
        self.slot == other.slot && self.trigger == other.trigger
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub upcoming: usize,
    pub history: usize,
    pub items: Vec<ReviewItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub title: String,
    pub due: i64,
    pub history: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Poll {
    pub token: Uuid,
    pub fresh_due: Option<i64>,
    pub review: Review,
}

impl Slot {
    fn row<'a>(&self, entries: &'a [Entry]) -> Option<std::borrow::Cow<'a, Entry>> {
        use std::borrow::Cow;
        let base = entries.iter().find(|e| e.id == self.event)?;
        let Some(original) = self.original else {
            return (base.kind == Kind::Appointment).then_some(Cow::Borrowed(base));
        };
        if base.kind != Kind::Weekly {
            return None;
        }
        if let Some(child) = entries.iter().find(|e| {
            e.kind
                == (Kind::Occurrence {
                    series: base.id,
                    original,
                })
        }) {
            return Some(Cow::Borrowed(child));
        }
        if !base.appears(original) {
            return None;
        }
        if base.range_at(original).is_some() {
            return base.inherited(original).map(Cow::Owned);
        }
        let mut occurrence = base.clone();
        occurrence.date = original;
        Some(Cow::Owned(occurrence))
    }
}

fn trigger(row: &Entry, zone: Option<Tz>) -> Option<Trigger> {
    let zone = match row.zone {
        Some(zone) => zone,
        None => Zone {
            name: zone?,
            start_fold: None,
            end_fold: None,
        },
    };
    let (start, end) = zone
        .interval(row.date, row.span, row.series_id().is_some())
        .ok()?;
    Some(Trigger {
        due: start.timestamp(),
        end: end.timestamp(),
        organizer_revision: row.invitation.as_ref().map(|i| i.sequence),
    })
}

pub(super) fn valid(alarms: &[Alarm]) -> bool {
    alarms.len() <= MAX_REMINDERS
        && alarms.iter().enumerate().all(|(i, a)| {
            !alarms[..i].iter().any(|b| b.slot == a.slot)
                && a.trigger.is_none_or(|t| t.due < t.end)
                && (a.received.is_none() || a.trigger.is_some())
        })
}

pub(super) fn reschedule(
    alarms: &mut Vec<Alarm>,
    entries: &[Entry],
    zone: Option<Tz>,
    revision: u64,
) {
    alarms.retain_mut(|alarm| {
        let Some(row) = alarm.slot.row(entries) else {
            return false;
        };
        let next = trigger(&row, zone);
        if next != alarm.trigger {
            alarm.trigger = next;
            alarm.received = None;
            alarm.revision = revision;
        }
        true
    });
}

impl Store {
    fn uncertain_reminders(&self) -> HashSet<Uuid> {
        self.outstanding()
            .filter_map(|(row, change)| {
                let accepted = &row.invitation.as_ref()?.accepted;
                let uncertain = change.conflicted
                    || change.proposed.as_ref().is_none_or(|p| {
                        (p.date, p.span, p.zone) != (accepted.date, accepted.span, accepted.zone)
                    });
                uncertain.then_some(row.id)
            })
            .collect()
    }

    pub fn set_reminder(
        &mut self,
        revision: u64,
        id: Uuid,
        shown: NaiveDate,
        enabled: bool,
    ) -> Result<(), String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        let row = self
            .window(shown, shown)
            .iter()
            .find_map(|e| {
                (e.id == id)
                    .then(|| e.on_day(shown, self.viewing_zone()))
                    .flatten()
            })
            .ok_or("Refresh the event first")?;
        if !matches!(
            row.kind,
            Kind::Appointment | Kind::Weekly | Kind::Occurrence { .. }
        ) {
            return Err("Choose a timed meeting".into());
        }
        let slot = Slot::for_row(&row, shown);
        let mut next = self.state.clone();
        let found = next.reminders.iter().position(|a| a.slot == slot);
        match (enabled, found) {
            (true, None) => {
                if next.reminders.len() >= MAX_REMINDERS {
                    return Err("Calendar reminder limit reached".into());
                }
                let raw = slot.row(&next.entries).ok_or("Refresh the event first")?;
                let t = trigger(&raw, Some(self.viewing_zone()))
                    .ok_or("Reminder time is unresolved")?;
                if t.due <= Utc::now().timestamp() {
                    return Err("Choose a future meeting".into());
                }
                next.reminders.push(Alarm {
                    slot,
                    revision: revision.checked_add(1).ok_or("Revision limit reached")?,
                    trigger: Some(t),
                    received: None,
                });
            }
            (false, Some(i)) => {
                next.reminders.remove(i);
            }
            _ => return Ok(()),
        }
        next.revision = revision.checked_add(1).ok_or("Revision limit reached")?;
        next.version = 14;
        self.persist(next)?;
        self.undo = None;
        self.fresh_due = None;
        Ok(())
    }

    /// Receipt publication precedes any handoff. A crash between them leaves
    /// the reminder in the private review and never repeats a public notice.
    pub fn tick_reminders(&mut self, now: DateTime<Utc>) -> Result<(), String> {
        let stamp = now.timestamp();
        let previous = self.last_tick.replace(stamp);
        if self.state.reminders.is_empty() {
            return Ok(());
        }
        let blocked = self.uncertain_reminders();
        let mut next = None;
        let mut fresh = None;
        let mut due_count = 0;
        for (i, alarm) in self.state.reminders.iter().enumerate() {
            let Some(t) = alarm.trigger else { continue };
            if alarm.received.is_some() || t.due > stamp || blocked.contains(&alarm.slot.event) {
                continue;
            }
            if !self.device_available && !self.has_pinned_zone() {
                continue;
            }
            next.get_or_insert_with(|| self.state.clone()).reminders[i].received = Some(stamp);
            due_count += 1;
            if previous.is_some_and(|p| p < t.due && stamp.saturating_sub(p) <= FRESH_SECONDS)
                && t.end > stamp
            {
                fresh = Some((alarm.slot, t.due));
            }
        }
        if let Some(next) = next {
            self.persist(next)?;
            self.fresh_due = if due_count == 1 { fresh } else { None };
        }
        Ok(())
    }

    pub fn reminder_review(&self, stamp: i64) -> Review {
        let mut review = Review::default();
        let blocked = self.uncertain_reminders();
        for a in &self.state.reminders {
            if blocked.contains(&a.slot.event) {
                continue;
            }
            let Some(t) = a.trigger.filter(|_| a.received.is_some()) else {
                continue;
            };
            let Some(row) = a.slot.row(&self.state.entries) else {
                continue;
            };
            let history = t.end <= stamp || stamp.saturating_sub(t.due) > 300;
            if history {
                review.history += 1;
            } else {
                review.upcoming += 1;
            }
            review.items.push(ReviewItem {
                title: row.title.clone(),
                due: t.due,
                history,
            });
        }
        review.items.sort_by_key(|item| {
            (
                item.history,
                if item.history {
                    item.due.saturating_neg()
                } else {
                    item.due
                },
            )
        });
        review
    }

    pub fn poll_reminders(&mut self, token: Option<Uuid>, now: i64) -> Poll {
        let previous_poll = token.zip(self.poll_lease).and_then(|(key, (held, last))| {
            (key == held && (0..=FRESH_SECONDS).contains(&now.saturating_sub(last))).then_some(last)
        });
        let due = self.fresh_due.take().and_then(|(slot, due)| {
            (previous_poll.is_some_and(|last| last <= due)
                && (0..=FRESH_SECONDS).contains(&now.saturating_sub(due))
                && !self.uncertain_reminders().contains(&slot.event)
                && self.state.reminders.iter().any(|a| {
                    a.slot == slot
                        && a.received.is_some()
                        && a.trigger.is_some_and(|t| t.due == due)
                }))
            .then_some(due)
        });
        let token = Uuid::new_v4();
        self.poll_lease = Some((token, now));
        Poll {
            token,
            fresh_due: due,
            review: self.reminder_review(now),
        }
    }
}
