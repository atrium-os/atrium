//! Project accepted weekly changes only for the dates a caller needs.
use super::*;
use crate::invitation::{OccurrenceId, RangeRule};
use crate::{Fold, Span, Zone};
use chrono::{Days, LocalResult, TimeZone, Timelike};
use sha2::{Digest, Sha256};
use std::borrow::Cow;

impl Entry {
    pub(super) fn ranges(&self) -> &[RangeRule] {
        self.invitation
            .as_ref()
            .filter(|_| self.kind == Kind::Weekly)
            .map_or(&[], |o| o.ranges.as_slice())
    }

    pub(super) fn range_at(&self, date: NaiveDate) -> Option<&RangeRule> {
        let rules = self.ranges();
        let n = rules.partition_point(|r| r.anchor <= date);
        n.checked_sub(1).map(|i| &rules[i])
    }

    pub(super) fn inherited(&self, date: NaiveDate) -> Option<Entry> {
        if !self.appears(date) {
            return None;
        }
        self.range_at(date)?.occurrence(self, date).ok()
    }
}

impl RangeRule {
    pub(super) fn occurrence(&self, base: &Entry, date: NaiveDate) -> Result<Entry, &'static str> {
        let mut template = base.clone();
        let e = &self.schedule;
        template.kind = Kind::Occurrence {
            series: base.id,
            original: date,
        };
        template.date = e.date;
        template.zone = e.zone;
        template.span = e.span;
        template.title.clone_from(&e.title);
        template.excluded.clear();
        let mut hash = Sha256::new();
        hash.update(b"Fasti weekly occurrence");
        hash.update(base.id.as_bytes());
        hash.update((date - NaiveDate::MIN).num_days().to_be_bytes());
        template.id =
            uuid::Builder::from_custom_bytes(hash.finalize()[..16].try_into().unwrap()).into_uuid();
        let mut row = shifted(base, self.anchor, &template, date)?;
        let accepted = row.fields_on(row.date);
        let origin = row.invitation.as_mut().ok_or("Missing weekly invitation")?;
        origin.ranges.clear();
        origin.range_sequence = None;
        origin.range_from = Some(self.anchor);
        origin.original = Some(OccurrenceId {
            date,
            minute: base.span.start,
            zone: base.zone.map(|z| z.name),
        });
        origin.sequence = self.sequence;
        origin.location.clone_from(&self.location);
        origin.accepted = accepted;
        origin.limit = None;
        origin.last = None;
        origin.skips.clear();
        origin.owned_skips.clear();
        origin.included.clear();
        Ok(row)
    }
}

/// Version eight kept only detached occurrences. Their accepted organizer
/// fields still describe the shift even if the user moved the saved copy.
pub(super) fn upgrade(entries: &mut [Entry]) -> Result<(), &'static str> {
    let mut recovered = BTreeMap::new();
    for child in entries.iter() {
        let Kind::Occurrence { series, original } = child.kind else {
            continue;
        };
        let Some(origin) = &child.invitation else {
            continue;
        };
        let Some(anchor) = origin.range_from else {
            continue;
        };
        let key = (series, anchor);
        // Prefer the latest recorded revision, then the original anchor.
        let priority = (origin.sequence, original == anchor);
        let record = recovered.entry(key).or_insert((priority, child, original));
        if priority > record.0 {
            *record = (priority, child, original);
        }
    }
    let mut upgrades = Vec::new();
    for ((series, anchor), (_, child, original)) in recovered {
        let base = entries
            .iter()
            .find(|r| r.id == series)
            .ok_or("Missing weekly master")?;
        let origin = child.invitation.as_ref().unwrap();
        let fields = &origin.accepted;
        let mut template = child.clone();
        template.date = fields.date;
        template.span = fields.span;
        template.zone = fields.zone;
        template.title.clone_from(&fields.title);
        let anchor_row = shifted(base, original, &template, anchor)?;
        upgrades.push((
            series,
            RangeRule {
                anchor,
                sequence: origin.sequence,
                schedule: anchor_row.fields_on(anchor_row.date),
                location: origin.location.clone(),
            },
        ));
    }
    for (id, rule) in upgrades {
        let origin = entries
            .iter_mut()
            .find(|r| r.id == id)
            .and_then(|r| r.invitation.as_mut())
            .ok_or("Missing weekly origin")?;
        origin.ranges.push(rule);
    }
    Ok(())
}

impl Store {
    pub(super) fn window(&self, start: NaiveDate, end: NaiveDate) -> Vec<Cow<'_, Entry>> {
        let mut rows = Vec::new();
        for base in &self.state.entries {
            let rules = base.ranges();
            let Some(first) = rules.first() else {
                rows.push(Cow::Borrowed(base));
                continue;
            };
            if first.anchor > base.date {
                let mut prefix = base.clone();
                let origin = prefix.invitation.as_mut().unwrap();
                origin.last = first.anchor.pred_opt();
                origin.ranges.clear();
                rows.push(Cow::Owned(prefix));
            }
            for (index, rule) in rules.iter().enumerate() {
                let shift = (rule.schedule.date - rule.anchor).num_days();
                // Viewing and source offsets, offset changes and a meeting that
                // ends at midnight can move its display across civil dates.
                let low = crate::zone::nearby(start, -shift - 5).max(rule.anchor);
                let mut high = crate::zone::nearby(end, -shift + 5);
                if let Some(next) = rules.get(index + 1) {
                    high = high.min(next.anchor.pred_opt().unwrap());
                }
                if let Some(last) = base.invitation.as_ref().and_then(|o| o.last) {
                    high = high.min(last);
                }
                let off = (7 - (low - rule.anchor).num_days() % 7) % 7;
                let Some(mut date) = low.checked_add_days(Days::new(off as u64)) else {
                    continue;
                };
                while date <= high {
                    if let Some(row) = base.inherited(date) {
                        rows.push(Cow::Owned(row));
                    }
                    let Some(next) = date.checked_add_days(Days::new(7)) else {
                        break;
                    };
                    date = next;
                }
            }
        }
        rows
    }
}
pub(super) fn shifted(
    base: &Entry,
    anchor: NaiveDate,
    proposed: &Entry,
    date: NaiveDate,
) -> Result<Entry, &'static str> {
    let mut row = proposed.clone();
    if let (Some(source), Some(target)) = (base.zone, proposed.zone) {
        let (original, _) = source.interval(anchor, base.span, true)?;
        let (first, end) = target.interval(proposed.date, proposed.span, false)?;
        let (slot, _) = source.interval(date, base.span, true)?;
        let start = slot
            .checked_add_signed(first - original)
            .ok_or("Range start out of bounds")?;
        let finish = start
            .checked_add_signed(end - first)
            .ok_or("Range end out of bounds")?;
        let start = start.with_timezone(&target.name);
        let finish = finish.with_timezone(&target.name);
        let minute = |t: chrono::DateTime<Tz>| (t.hour() * 60 + t.minute()) as u16;
        let end = if start.date_naive() == finish.date_naive() {
            minute(finish)
        } else if start.date_naive().succ_opt() == Some(finish.date_naive()) && minute(finish) == 0
        {
            1440
        } else {
            return Err("A shifted meeting extends beyond a supported day.");
        };
        let fold = |t: chrono::DateTime<Tz>| match target.name.from_local_datetime(&t.naive_local())
        {
            LocalResult::Ambiguous(first, _) => Some(if first == t {
                Fold::Earlier
            } else {
                Fold::Later
            }),
            _ => None,
        };
        row.date = start.date_naive();
        row.span = Span {
            start: minute(start),
            end,
        };
        row.zone = Some(Zone {
            name: target.name,
            start_fold: fold(start),
            end_fold: fold(finish),
        });
    } else {
        row.date = proposed
            .date
            .checked_add_signed(date - anchor)
            .ok_or("Range date out of bounds")?;
    }
    if !row.span.valid() {
        return Err("A shifted meeting has an unsupported clock interval.");
    }
    row.validate()?;
    Ok(row)
}
