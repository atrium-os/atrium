//! Expand a bounded range using original slots. Detached exceptions keep their own schedule.
use super::*;
use crate::invitation::{RangeDate, MAX_MOVES};
use crate::{Fold, Span, Zone};
use chrono::{Days, LocalResult, TimeZone, Timelike};

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
        let Some(last) = base.invitation.as_ref().and_then(|o| o.last) else {
            return Ok(review.blocked("Range review needs a weekly series with an end date."));
        };
        if (last - anchor.date).num_days() / 7 >= MAX_MOVES as i64 {
            return Ok(review.blocked("This range exceeds the eight-date review limit."));
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
        let mut date = anchor.date;
        while date < last {
            date = date
                .checked_add_days(Days::new(7))
                .ok_or("Range date out of bounds")?;
            if date > last {
                break;
            }
            let child = self.state.entries.iter().find(|e| {
                e.kind
                    == Kind::Occurrence {
                        series: id,
                        original: date,
                    }
            });
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
            master.range_sequence = Some(master.range_sequence.unwrap_or_default().max(p.sequence));
        }
        self.invite = group;
        Ok(review)
    }
}

fn shifted(
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
