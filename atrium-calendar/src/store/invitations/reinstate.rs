//! Rejoin a cancelled weekly tail to the current prefix by original recurrence date.
use super::*;
use crate::invitation::{CancelledInterval, RangeDate, RangeRule, MAX_MOVES};
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
        let restart = anchor.date;
        let cut = mark.original.ok_or("Missing cancellation cutoff")?;
        let proposed = p.row.as_ref().ok_or("Missing organizer schedule")?;
        if restart < cut
            || base.remains_cancelled(restart)
            || base.zone.is_some() != proposed.zone.is_some()
        {
            return Ok(review.blocked("The proposed range does not match the cancelled schedule."));
        }
        // Only independent organizer exceptions beyond the new anchor survive its rule.
        let mut exceptions = BTreeMap::new();
        for row in &tail.exceptions {
            let Kind::Occurrence { original, .. } = row.kind else {
                return Err("Invalid archived occurrence".into());
            };
            if original > restart
                && row
                    .invitation
                    .as_ref()
                    .is_some_and(|o| o.range_from.is_none_or(|a| a > restart))
            {
                exceptions.insert(original, row);
            } else {
                base.excluded.retain(|d| *d != original);
            }
        }
        base.excluded.retain(|d| *d >= restart);
        let archived = base
            .invitation
            .as_mut()
            .ok_or("Missing saved range identity")?;
        archived.ranges.retain(|r| r.anchor > restart);
        archived.skips.retain(|o| o.date >= restart);
        archived.owned_skips.retain(|d| *d >= restart);
        archived.included.retain(|o| o.date >= restart);
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
            base.excluded.splice(0..0, live.excluded.iter().copied());
            let mut joined = saved.clone();
            joined.last = last;
            joined.limit = archived.limit;
            joined.skips.append(&mut archived.skips);
            joined.owned_skips.append(&mut archived.owned_skips);
            joined.included.append(&mut archived.included);
            joined.ranges.append(&mut archived.ranges);
            base.invitation = Some(joined);
        } else if cut != base.date
            || history
                .iter()
                .any(|c| c.key == mark.key && c.this_and_future && c.original < mark.original)
        {
            return Ok(review.blocked("Restore the earlier weekly schedule first."));
        }
        let rule = RangeRule {
            anchor: restart,
            sequence: p.sequence,
            schedule: proposed.fields_on(proposed.date),
            location: p.location.clone(),
        };
        rule.occurrence(&base, restart)?;
        let origin = base.invitation.as_mut().unwrap();
        if restart > cut {
            let mut gap = CancelledInterval {
                sequence: Some(p.sequence),
                copies: BTreeMap::new(),
                first: cut,
                restart,
            };
            review.cancelled_before_restart = Some(gap.clone());
            // Fold intersecting or adjacent gaps together; storage is independent of their length.
            gap.copies.extend(
                tail.copies
                    .iter()
                    .copied()
                    .filter(|(d, _)| *d >= cut && *d < restart),
            );
            let mut merged = gap;
            origin.cancelled_intervals.retain(|g| {
                if g.restart < merged.first || g.first > merged.restart {
                    return true;
                }
                merged
                    .copies
                    .extend(g.copies.iter().map(|(&d, &id)| (d, id)));
                merged.sequence = merged.sequence.max(g.sequence);
                merged.first = merged.first.min(g.first);
                merged.restart = merged.restart.max(g.restart);
                false
            });
            let at = origin
                .cancelled_intervals
                .partition_point(|g| g.first < merged.first);
            origin.cancelled_intervals.insert(at, merged);
        }
        let at = origin.ranges.partition_point(|r| r.anchor < restart);
        origin.ranges.insert(at, rule);
        origin.range_sequence = Some(origin.range_sequence.unwrap_or_default().max(p.sequence));
        let mut next = rows.to_vec();
        if current.is_some() {
            next.retain(|r| r.id != base.id);
        }
        next.extend(exceptions.values().map(|r| (*r).clone()));
        let copies: BTreeMap<_, _> = tail
            .copies
            .iter()
            .filter_map(|(d, id)| {
                rows.iter()
                    .find(|r| r.id == *id && r.kind == Kind::Appointment && r.invitation.is_none())
                    .map(|r| (*d, r))
            })
            .collect();
        let mut dates = BTreeSet::from([cut, restart]);
        if restart > cut {
            let last_cancelled = restart
                .checked_sub_days(Days::new(7))
                .ok_or("Invalid restart date")?;
            dates.insert(last_cancelled);
            if (restart - cut).num_days() / 7 <= MAX_MOVES as i64 {
                let mut day = cut;
                while day < last_cancelled {
                    day = day
                        .checked_add_days(Days::new(7))
                        .ok_or("Invalid cancelled date")?;
                    dates.insert(day);
                }
            }
            dates.extend(
                tail.master
                    .excluded
                    .iter()
                    .copied()
                    .filter(|d| *d < restart),
            );
            dates.extend(
                tail.master
                    .ranges()
                    .iter()
                    .map(|r| r.anchor)
                    .filter(|d| *d < restart),
            );
        }
        let short = last.is_some_and(|end| (end - restart).num_days() / 7 < MAX_MOVES as i64);
        let mut d = restart;
        while let Some(day) = d
            .checked_add_days(Days::new(7))
            .filter(|d| last.is_none_or(|end| *d <= end))
        {
            dates.insert(day);
            d = day;
            if !short && base.inherited(day).is_some() {
                break;
            }
            if !short && (day - restart).num_days() / 7 > base.excluded.len() as i64 + 3 {
                break;
            }
        }
        dates.extend(last);
        dates.extend(base.excluded.iter().copied().filter(|d| *d >= cut));
        dates.extend(base.ranges().iter().map(|r| r.anchor).filter(|d| *d >= cut));
        dates.extend(copies.keys().copied());
        let mut moves = Vec::new();
        for date in dates {
            let returning = exceptions
                .get(&date)
                .map(|r| (**r).clone())
                .or_else(|| base.inherited(date));
            let private = copies.get(&date);
            if let Some(copy) = private.filter(|_| date >= restart) {
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
            one_off: None,
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
            "Restore from {restart} · {}{}",
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
