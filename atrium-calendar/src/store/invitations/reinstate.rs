//! Rejoin a cancelled weekly tail to the current prefix by original recurrence date.
use super::*;
use crate::invitation::{RangeDate, RangeRule, MAX_MOVES};
use chrono::Days;
use std::collections::{BTreeMap, BTreeSet};

impl Store {
    pub(super) fn prepare_reinstatement(
        rows: &[Entry],
        history: &[Cancellation],
        view: Tz,
        revision: u64,
        source: &Source,
        p: &Proposal,
        mark: &Cancellation,
        current: Option<&Entry>,
        mut review: Review,
        pending: &mut Option<Pending>,
    ) -> Result<Review, String> {
        if !source.verified || p.sequence <= mark.sequence {
            return Ok(review.blocked("Restoring dates needs a newer, verified organizer request."));
        }
        let Some(tail) = &mark.tail else {
            return Ok(review.blocked("This older cancellation has no saved schedule to restore."));
        };
        let mut base = tail.master.clone();
        let Ok(anchor) = p.original.ok_or("Missing range start")?.in_series(&base) else {
            return Ok(review.blocked("The original slot differs from the cancelled range."));
        };
        let cut = anchor.date;
        let proposed = p.row.as_ref().ok_or("Missing organizer schedule")?;
        if Some(cut) != mark.original || base.zone.is_some() != proposed.zone.is_some() {
            return Ok(review.blocked("The proposed range does not match the cancelled schedule."));
        }
        let archived = base
            .invitation
            .as_ref()
            .ok_or("Missing saved range identity")?;
        let last = archived.last;
        if let Some(live) = current {
            let saved = live.invitation.as_ref().ok_or("Missing weekly identity")?;
            if live.id != base.id
                || live.kind != Kind::Weekly
                || live.fields_on(live.date) != archived.accepted
                || saved.last != cut.checked_sub_days(Days::new(7))
                || saved.sequence >= p.sequence
            {
                return Ok(
                    review.blocked("The earlier weekly schedule changed. Review it separately.")
                );
            }
            // Earlier one-off edits made since cancellation belong to the live prefix.
            base.excluded.retain(|d| *d >= cut);
            base.excluded.splice(0..0, live.excluded.iter().copied());
            let mut joined = saved.clone();
            joined.last = last;
            joined.limit = archived.limit;
            joined
                .skips
                .extend(archived.skips.iter().filter(|o| o.date >= cut));
            joined
                .owned_skips
                .extend(archived.owned_skips.iter().filter(|d| **d >= cut));
            joined
                .included
                .extend(archived.included.iter().filter(|o| o.date >= cut));
            joined
                .ranges
                .extend(archived.ranges.iter().filter(|r| r.anchor > cut).cloned());
            base.invitation = Some(joined);
        } else if cut != base.date
            || history
                .iter()
                .any(|c| c.key == mark.key && c.this_and_future && c.original < mark.original)
        {
            return Ok(review.blocked("Restore the earlier weekly schedule first."));
        }
        let rule = RangeRule {
            anchor: cut,
            sequence: p.sequence,
            schedule: proposed.fields_on(proposed.date),
            location: p.location.clone(),
        };
        rule.occurrence(&base, cut)?;
        let origin = base.invitation.as_mut().unwrap();
        origin.ranges.retain(|r| r.anchor != cut);
        let at = origin.ranges.partition_point(|r| r.anchor < cut);
        origin.ranges.insert(at, rule);
        origin.range_sequence = Some(origin.range_sequence.unwrap_or_default().max(p.sequence));
        let mut next = rows.to_vec();
        if current.is_some() {
            next.retain(|r| r.id != base.id);
        }
        next.extend(tail.exceptions.iter().cloned());
        let copies: BTreeMap<_, _> = tail
            .copies
            .iter()
            .filter_map(|(d, id)| {
                rows.iter()
                    .find(|r| r.id == *id && r.kind == Kind::Appointment && r.invitation.is_none())
                    .map(|r| (*d, r))
            })
            .collect();
        let mut dates = BTreeSet::from([cut]);
        let short = last.is_some_and(|end| (end - cut).num_days() / 7 < MAX_MOVES as i64);
        let mut d = cut;
        while let Some(day) = d
            .checked_add_days(Days::new(7))
            .filter(|d| last.is_none_or(|end| *d <= end))
        {
            dates.insert(day);
            d = day;
            if !short && base.inherited(day).is_some() {
                break;
            }
            if !short && (day - cut).num_days() / 7 > base.excluded.len() as i64 + 3 {
                break;
            }
        }
        dates.extend(last);
        dates.extend(base.excluded.iter().copied().filter(|d| *d >= cut));
        dates.extend(base.ranges().iter().map(|r| r.anchor).filter(|d| *d >= cut));
        dates.extend(copies.keys().copied());
        let mut moves = Vec::new();
        for date in dates {
            let kind = Kind::Occurrence {
                series: base.id,
                original: date,
            };
            let returning = tail
                .exceptions
                .iter()
                .find(|r| r.kind == kind)
                .cloned()
                .or_else(|| base.inherited(date));
            let private = copies.get(&date);
            if let Some(copy) = private {
                moves.push(super::super::invitation_skips::LocalMove::private_copy(
                    date, copy.id,
                ));
                if let Some(mut row) = returning.clone().filter(|r| r.id == copy.id) {
                    // A materialized range move can share its original projection's stable ID.
                    row.id = Uuid::new_v4();
                    let at = base.excluded.partition_point(|d| *d < date);
                    base.excluded.insert(at, date);
                    next.push(row);
                }
            }
            review.range_dates.push(RangeDate {
                original: date,
                before: None,
                after: returning.map(|r| r.fields_on(r.date)),
                private_copy: private.map(|r| r.fields_on(r.date)),
            });
        }
        next.push(base);
        super::super::validate_records(&next)?;
        let mut marks = history.to_vec();
        marks.retain(|c| !(c.this_and_future && c.key == mark.key && c.original == Some(cut)));
        let token = Uuid::new_v4();
        *pending = Some(Pending {
            token,
            revision,
            created: Instant::now(),
            date: proposed.fields_on(proposed.date).agenda_date(view)?,
            local_moves: moves,
            changes: Some((next, marks)),
        });
        review.previous = None;
        review.weekly = true;
        review.reinstating = true;
        review.range_summary = Some(format!(
            "Restore from {cut} · {}{}",
            last.map_or_else(|| "no end date".into(), |end| format!("through {end}")),
            if short {
                ""
            } else {
                " · examples and exceptions"
            }
        ));
        review.token = Some(token);
        review.can_save = true;
        Ok(review)
    }
}
