//! End a weekly series by original recurrence date, including an unbounded tail.
use super::*;
use crate::invitation::{SkipEffect, SkipReview, MAX_MOVES};
use chrono::Days;
use std::collections::{BTreeMap, BTreeSet};

impl Store {
    pub(super) fn prepare_range_cancellation(
        rows: &[Entry],
        history: &[Cancellation],
        view: Tz,
        revision: u64,
        source: &Source,
        p: &Proposal,
        key: MeetingKey,
        master: Option<&Entry>,
        mut review: Review,
        pending: &mut Option<Pending>,
    ) -> Result<Review, String> {
        if !source.verified {
            return Ok(review.blocked("Mail could not verify the sender of this cancellation."));
        }
        let Some(base) = master.filter(|e| e.kind == Kind::Weekly) else {
            return Ok(review.blocked("No saved weekly series matches this cancellation."));
        };
        let original = p.original.ok_or("Missing range start")?;
        let Ok(anchor) = original.in_series(base) else {
            return Ok(review.blocked("The starting slot does not match the saved weekly series."));
        };
        let cut = anchor.date;
        let origin = base
            .invitation
            .as_ref()
            .ok_or("Missing invitation identity")?;
        if origin.accepted != base.fields_on(base.date) {
            return Ok(review.blocked("You edited the weekly schedule locally. Review it first."));
        }
        let children: BTreeMap<_, _> = rows
            .iter()
            .filter_map(|e| match e.kind {
                Kind::Occurrence { series, original } if series == base.id => Some((original, e)),
                _ => None,
            })
            .collect();
        if origin.sequence >= p.sequence
            || base.range_at(cut).is_some_and(|r| r.sequence >= p.sequence)
            || base
                .ranges()
                .iter()
                .any(|r| r.anchor >= cut && r.sequence >= p.sequence)
            || children.range(cut..).any(|(_, e)| {
                e.invitation
                    .as_ref()
                    .is_some_and(|o| o.sequence >= p.sequence)
            })
            || history.iter().any(|c| {
                c.key == key
                    && c.sequence >= p.sequence
                    && (c.this_and_future || c.original.is_none_or(|d| d >= cut))
            })
        {
            return Ok(
                review.blocked("A saved revision conflicts with or supersedes this cancellation.")
            );
        }

        let mut dates = BTreeSet::from([cut]);
        let bounded = origin
            .last
            .is_some_and(|last| (last - cut).num_days() / 7 < MAX_MOVES as i64);
        if bounded {
            let mut day = cut;
            while let Some(next) = day
                .checked_add_days(Days::new(7))
                .filter(|d| Some(*d) <= origin.last)
            {
                dates.insert(next);
                day = next;
            }
        } else {
            dates.extend(origin.last);
            // Include a regular example even when the starting slot was moved or skipped.
            let mut day = cut;
            for _ in 0..base.excluded.len() + 3 {
                let Some(next) = day.checked_add_days(Days::new(7)) else {
                    break;
                };
                if origin.last.is_some_and(|last| next > last) {
                    break;
                }
                day = next;
                if base.appears(day)
                    && base
                        .zone
                        .is_none_or(|z| z.interval(day, base.span, true).is_ok())
                {
                    dates.insert(day);
                    break;
                }
            }
        }
        dates.extend(base.excluded.iter().copied().filter(|d| *d >= cut));
        dates.extend(base.ranges().iter().map(|r| r.anchor).filter(|d| *d >= cut));
        let source_zone = base.zone.map_or(view, |z| z.name);
        for (&original, e) in children.range(..cut) {
            if e.fields_on(e.date).agenda_date(source_zone)? >= cut {
                dates.insert(original);
            }
        }

        let mut local_moves = Vec::new();
        let mut private_ids = HashSet::new();
        for date in dates {
            let child = children.get(&date).copied();
            let mut current = child.cloned().or_else(|| base.inherited(date)).or_else(|| {
                (base.appears(date)
                    && base
                        .zone
                        .is_none_or(|z| z.interval(date, base.span, true).is_ok()))
                .then(|| {
                    let mut copy = base.clone();
                    copy.date = date;
                    copy
                })
            });
            let effect = if date < cut {
                SkipEffect::KeepEarlier
            } else if let Some(child) = child.filter(|e| {
                e.invitation
                    .as_ref()
                    .is_none_or(|o| o.original.is_none() || o.accepted != e.fields_on(e.date))
            }) {
                private_ids.insert(child.id);
                local_moves.push(super::super::invitation_skips::LocalMove::private_copy(
                    date, child.id,
                ));
                SkipEffect::ChooseLocalMove
            } else if current.is_some() {
                SkipEffect::Cancel
            } else {
                SkipEffect::AlreadyAbsent
            };
            if let Some(row) = &mut current {
                row.excluded.clear();
                row.invitation = None;
            }
            review.skips.push(SkipReview {
                original: child
                    .and_then(|e| e.invitation.as_ref()?.original)
                    .unwrap_or(crate::invitation::OccurrenceId { date, ..anchor }),
                current,
                effect,
            });
        }

        let mut next = rows.to_vec();
        next.retain_mut(|row| {
            if row.id == base.id {
                if cut == base.date {
                    return false;
                }
                row.excluded.retain(|date| *date < cut);
                let saved = row.invitation.as_mut().unwrap();
                // A later local time edit must respect the cancelled end of the series.
                saved.last = cut.checked_sub_days(Days::new(7));
                saved.limit = None;
                saved.ranges.retain(|rule| rule.anchor < cut);
                saved.range_sequence = saved.ranges.iter().map(|rule| rule.sequence).max();
                saved.included.retain(|slot| slot.date < cut);
                saved.skips.retain(|slot| slot.date < cut);
                saved.owned_skips.retain(|date| *date < cut);
            } else if let Kind::Occurrence { series, original } = row.kind {
                if series == base.id && original >= cut {
                    if !private_ids.contains(&row.id) {
                        return false;
                    }
                    row.kind = Kind::Appointment;
                    row.invitation = None;
                    row.import_hash = None;
                }
            }
            true
        });
        let mut marks = history.to_vec();
        marks.retain(|c| c.key != key || c.original.is_none_or(|d| d < cut));
        marks.push(Cancellation {
            key,
            original: Some(cut),
            sequence: p.sequence,
            this_and_future: true,
        });
        let scope = match origin.last {
            Some(end) => format!("Cancel from {cut} through {end}"),
            None => format!("Cancel from {cut} onward · no end date"),
        };
        review.range_summary = Some(if bounded {
            scope
        } else {
            format!("{scope} · examples and saved exceptions")
        });
        let agenda_date = match base.zone {
            Some(zone) => zone
                .interval(cut, base.span, true)?
                .0
                .with_timezone(&view)
                .date_naive(),
            None => cut,
        };
        let token = Uuid::new_v4();
        review.can_save = true;
        review.token = Some(token);
        *pending = Some(Pending {
            token,
            created: Instant::now(),
            revision,
            date: agenda_date,
            local_moves,
            changes: Some((next, marks)),
        });
        Ok(review)
    }
}
