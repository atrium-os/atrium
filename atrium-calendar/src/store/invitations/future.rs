//! Review a weekly range on its original slots and retain separately edited meetings.
use super::super::ranges::shifted;
use super::*;
use crate::invitation::{RangeDate, RangeRule, MAX_MOVES};
use chrono::Days;
use std::collections::BTreeSet;

impl Store {
    pub(super) fn review_future(
        &mut self,
        revision: u64,
        source: &Source,
        mut p: Proposal,
    ) -> Result<Review, String> {
        let anchor = p.original.ok_or("Missing original range slot")?;
        let proposed = p.row.clone().ok_or("Missing proposed range schedule")?;
        let mut group = None;
        let view = self.viewing_zone();
        let mut review = Self::prepare(
            &self.state.entries,
            &self.state.cancelled,
            view,
            revision,
            source,
            p.clone(),
            None,
            Some(anchor.date),
            &mut group,
        )?;
        let Some(ref mut plan) = group else {
            return Ok(review);
        };
        let id = review
            .previous
            .as_ref()
            .and_then(Entry::series_id)
            .ok_or("Missing weekly series")?;
        let base = self
            .state
            .entries
            .iter()
            .find(|r| r.id == id)
            .ok_or("Missing weekly master")?;
        if base
            .range_at(anchor.date)
            .is_some_and(|r| r.sequence > p.sequence)
        {
            return Ok(review.blocked("A newer range already covers this occurrence."));
        }
        let last = base.invitation.as_ref().and_then(|o| o.last);
        let summarized =
            last.is_none_or(|end| (end - anchor.date).num_days() / 7 >= MAX_MOVES as i64);
        let rule = RangeRule {
            anchor: anchor.date,
            sequence: p.sequence,
            schedule: proposed.fields_on(proposed.date),
            location: p.location.clone(),
        };
        let mut dates = BTreeSet::new();
        let next = anchor.date.checked_add_days(Days::new(7));
        if summarized {
            dates.extend(next.filter(|d| last.is_none_or(|end| *d <= end)));
            let mut sample = next;
            while let Some(d) = sample.filter(|d| last.is_none_or(|end| *d <= end)) {
                if base.excluded.binary_search(&d).is_err() {
                    dates.insert(d);
                    break;
                }
                sample = d.checked_add_days(Days::new(7));
            }
            dates.extend(last.filter(|d| *d > anchor.date));
            dates.extend(base.excluded.iter().copied().filter(|d| *d > anchor.date));
            dates.extend(
                base.ranges()
                    .iter()
                    .map(|r| r.anchor)
                    .filter(|d| *d > anchor.date),
            );
            review.range_summary = Some(
                if let Some(next_rule) = base.ranges().iter().find(|r| r.anchor > anchor.date) {
                    format!(
                        "Weekly until the separate range starting {}",
                        next_rule.anchor
                    )
                } else {
                    match last {
                        None => "Weekly · no end date · examples and saved exceptions".into(),
                        Some(end) => {
                            format!("Weekly through {end} · examples and saved exceptions")
                        }
                    }
                },
            );
        } else {
            let mut date = next;
            while let Some(d) = date.filter(|d| Some(*d) <= last) {
                dates.insert(d);
                date = d.checked_add_days(Days::new(7));
            }
        }
        if proposed.zone.is_some() != base.zone.is_some() {
            return Ok(review.blocked("A range cannot mix floating and zoned schedules."));
        }
        if anchor.in_series(base).is_err() {
            return Ok(review.blocked("The original range slot differs from the weekly schedule."));
        }
        if review
            .previous
            .as_ref()
            .is_some_and(|r| r.kind == Kind::Weekly && !base.appears(anchor.date))
        {
            review.previous = None;
        }
        review.range_dates.push(RangeDate {
            original: anchor.date,
            before: review.previous.as_ref().map(|e| e.fields_on(e.date)),
            after: Some(proposed.fields_on(proposed.date)),
        });
        for date in dates {
            let virtual_child = base.inherited(date);
            let child = self.state.entries.iter().find(|e| {
                e.kind
                    == Kind::Occurrence {
                        series: id,
                        original: date,
                    }
            });
            let physical = child.is_some();
            let child = child.or(virtual_child.as_ref());
            let before = child.map(|e| e.fields_on(e.date)).or_else(|| {
                (base.appears(date)
                    && base
                        .zone
                        .is_none_or(|z| z.interval(date, base.span, true).is_ok()))
                .then(|| base.fields_on(date))
            });
            let inherited = child.is_some_and(|e| {
                e.invitation.as_ref().is_some_and(|o| {
                    o.range_from.is_some_and(|start| start <= anchor.date)
                        && o.accepted == e.fields_on(e.date)
                })
            });
            if before.is_none() || child.is_some() && !inherited {
                review.range_dates.push(RangeDate {
                    original: date,
                    before,
                    after: None,
                });
                continue;
            }

            let row = match shifted(base, anchor.date, &proposed, date) {
                Ok(row) => row,
                Err(reason) => return Ok(review.blocked(reason)),
            };
            let after = row.fields_on(row.date);
            if summarized && !physical {
                review.range_dates.push(RangeDate {
                    original: date,
                    before,
                    after: Some(after),
                });
                continue;
            }
            p.row = Some(row);
            p.original = Some(crate::invitation::OccurrenceId { date, ..anchor });
            let (rows, marks) = plan
                .changes
                .as_ref()
                .map(|(r, m)| (r.as_slice(), m.as_slice()))
                .unwrap_or((&self.state.entries, &self.state.cancelled));
            let mut part = None;
            let checked = Self::prepare(
                rows,
                marks,
                view,
                revision,
                source,
                p.clone(),
                None,
                Some(anchor.date),
                &mut part,
            )?;
            let Some(part) = part else {
                return Ok(review.blocked(&format!("{date} · {}", checked.notice)));
            };
            if let Some(changes) = part.changes {
                if !review.can_save {
                    return Ok(review.blocked("The saved range differs from this revision."));
                }
                plan.changes = Some(changes);
            }
            review.range_dates.push(RangeDate {
                original: date,
                before,
                after: Some(after),
            });
        }
        if let Some((rows, _)) = &mut plan.changes {
            let master = rows
                .iter_mut()
                .find(|r| r.id == id)
                .and_then(|r| r.invitation.as_mut())
                .ok_or("Missing range master")?;
            match master
                .ranges
                .binary_search_by_key(&rule.anchor, |r| r.anchor)
            {
                Ok(index) => master.ranges[index] = rule,
                Err(index) => master.ranges.insert(index, rule),
            }
            master.range_sequence = Some(master.range_sequence.unwrap_or_default().max(p.sequence));
        }
        self.invite = group;
        Ok(review)
    }
}
