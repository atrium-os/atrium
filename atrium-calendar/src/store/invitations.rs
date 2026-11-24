mod cancel_future;
mod future;
mod reinstate;
mod restore_one;
use super::*;
use crate::invitation::{Cancellation, MeetingKey, Origin, Proposal, Review};
use portcullis_ipc::calendar::{Source, REVIEW_SECONDS};
use std::time::Instant;

pub(super) struct Pending {
    token: Uuid,
    revision: u64,
    created: Instant,
    date: NaiveDate,
    local_moves: Vec<super::invitation_skips::LocalMove>,
    changes: Option<(Vec<Entry>, Vec<Cancellation>)>,
}

impl Store {
    pub fn review_invitation(
        &mut self,
        revision: u64,
        source: &Source,
        mut p: Proposal,
    ) -> Result<Review, String> {
        self.invite = None;
        if revision != self.state.revision {
            return Err("Calendar changed. Review this invitation again.".into());
        }
        p.skips = p.validate_delivery(source)?;
        if p.this_and_future && p.row.is_some() {
            return self.review_future(revision, source, p);
        }
        if !p.cancelled_dates.is_empty() {
            return self.review_cancelled_dates(revision, source, p);
        }
        let view = self.viewing_zone();
        let children = p
            .exceptions
            .iter()
            .map(|m| Proposal {
                this_and_future: false,
                uid: p.uid.clone(),
                sequence: p.sequence,
                organizer: p.organizer.clone(),
                attendees: p.attendees.clone(),
                location: m.location.clone(),
                original: Some(m.original),
                row: Some(m.row.clone()),
                weekly_cancellation: false,
                limit: None,
                exceptions: vec![],
                skips: vec![],
                cancelled_dates: vec![],
            })
            .collect::<Vec<_>>();
        let mut pending = None;
        let mut review = Self::prepare(
            &self.state.entries,
            &self.state.cancelled,
            view,
            revision,
            source,
            p,
            None,
            None,
            &mut pending,
        )?;
        if let Some(plan) = &mut pending {
            for child in children {
                let previous = review.previous.as_ref().and_then(|master| {
                    let date = child.original?.date;
                    self.state
                        .entries
                        .iter()
                        .find(|e| {
                            e.kind
                                == Kind::Occurrence {
                                    series: master.id,
                                    original: date,
                                }
                        })
                        .cloned()
                        .or_else(|| master.inherited(date))
                        .or_else(|| {
                            master.appears(date).then(|| {
                                let mut row = master.clone();
                                row.date = date;
                                row
                            })
                        })
                });
                let (rows, marks) = plan
                    .changes
                    .as_ref()
                    .map(|(r, m)| (r.as_slice(), m.as_slice()))
                    .unwrap_or((&self.state.entries, &self.state.cancelled));
                let mut child_plan = None;
                let result = Self::prepare(
                    rows,
                    marks,
                    view,
                    revision,
                    source,
                    child,
                    previous.as_ref(),
                    None,
                    &mut child_plan,
                )?;
                review.previous_dates.push(previous);
                if result.token.is_none() {
                    return Ok(review.blocked(&format!("Included move: {}", result.notice)));
                }
                if let Some(changes) = child_plan.and_then(|c| c.changes) {
                    if !review.can_save {
                        return Ok(
                            review.blocked("A move conflicts with the saved series revision.")
                        );
                    }
                    plan.changes = Some(changes);
                }
            }
        }
        self.invite = pending;
        Ok(review)
    }

    fn review_cancelled_dates(
        &mut self,
        revision: u64,
        source: &Source,
        mut p: Proposal,
    ) -> Result<Review, String> {
        let mut group: Option<Pending> = None;
        let mut first_review = None;
        let mut previous_dates = Vec::new();
        let mut conflict = None;
        for date in std::mem::take(&mut p.cancelled_dates) {
            let original = date.original;
            p.sequence = date.sequence;
            p.original = Some(original);
            let (rows, marks) = group
                .as_ref()
                .and_then(|g| g.changes.as_ref())
                .map(|(r, m)| (r.as_slice(), m.as_slice()))
                .unwrap_or((&self.state.entries, &self.state.cancelled));
            let mut part = None;
            let review = Self::prepare(
                rows,
                marks,
                self.viewing_zone(),
                revision,
                source,
                p.clone(),
                None,
                None,
                &mut part,
            )?;
            // A single-date conflict can show the series pattern even when that
            // date has already been removed. Only label a saved date as current.
            previous_dates.push(
                review
                    .previous
                    .as_ref()
                    .filter(|e| e.kind != Kind::Weekly || !e.excluded.contains(&original.date))
                    .cloned(),
            );
            match part {
                Some(plan) => {
                    if let Some(g) = &mut group {
                        g.changes = plan.changes;
                    } else {
                        group = Some(plan);
                    }
                }
                None => {
                    conflict
                        .get_or_insert_with(|| format!("{} · {}", original.date, review.notice));
                }
            }
            first_review.get_or_insert(review);
        }
        let mut review = first_review.ok_or("Missing cancelled dates")?;
        if let Some(id) = review.previous.as_ref().and_then(Entry::series_id) {
            review.previous = self.state.entries.iter().find(|e| e.id == id).cloned();
        }
        review.previous_dates = previous_dates;
        if let Some(reason) = conflict {
            return Ok(review.blocked(&reason));
        }
        self.invite = group;
        Ok(review)
    }

    // Build an in-memory proposal. A bundle publishes only its final snapshot.
    pub(super) fn prepare(
        rows: &[Entry],
        history: &[Cancellation],
        view: Tz,
        revision: u64,
        source: &Source,
        mut p: Proposal,
        prior_occurrence: Option<&Entry>,
        range_from: Option<NaiveDate>,
        pending: &mut Option<Pending>,
    ) -> Result<Review, String> {
        let cancelled = p.row.is_none();
        let organizer = p.organizer.to_ascii_lowercase();
        let mut mine = source.identities.iter().filter(|id| {
            if cancelled && p.attendees.is_empty() {
                return rows
                    .iter()
                    .filter_map(|e| e.invitation.as_ref().map(|o| &o.key))
                    .chain(history.iter().map(|c| &c.key))
                    .any(|k| {
                        k.account == source.account
                            && k.organizer == organizer
                            && k.uid == p.uid
                            && k.attendee.eq_ignore_ascii_case(id)
                    });
            }
            p.attendees.iter().any(|a| a.eq_ignore_ascii_case(id))
        });
        let attendee = mine
            .next()
            .ok_or("No matching invited identity on this Mail account")?
            .to_ascii_lowercase();
        if mine.next().is_some() {
            return Err("More than one invited identity matches this account".into());
        }
        let key = MeetingKey {
            account: source.account,
            organizer,
            uid: p.uid.clone(),
            attendee: attendee.clone(),
        };
        let master = rows.iter().find(|e| {
            !matches!(e.kind, Kind::Occurrence { .. })
                && e.invitation.as_ref().is_some_and(|o| o.key == key)
        });
        let wire_date = p.original.map(|o| o.date);
        let mut review = Review {
            reinstating: false,
            cancelled_before_restart: None,
            range_summary: None,
            range_dates: vec![],
            previous_dates: vec![],
            skips: vec![],
            previous: master.cloned(),
            weekly: master.is_some_and(|e| e.kind == Kind::Weekly)
                || p.row.as_ref().is_some_and(|e| e.kind == Kind::Weekly),
            attendee,
            can_save: false,
            token: None,
            notice: String::new(),
        };
        if !source.from.eq_ignore_ascii_case(&key.organizer) || !source.reply_destination_matches {
            return Ok(
                review.blocked("The Mail sender or reply address differs from the organizer.")
            );
        }

        if p.this_and_future && !cancelled {
            if let Some(mark) = history
                .iter()
                .filter(|c| {
                    if !c.this_and_future || c.key != key {
                        return false;
                    }
                    let Some(cut) = c.original else {
                        return false;
                    };
                    match &c.tail {
                        Some(tail) => p.original.is_some_and(|o| {
                            o.date == cut
                                || o.in_series(&tail.master).is_ok_and(|slot| slot.date >= cut)
                        }),
                        None => wire_date.is_some_and(|date| date >= cut),
                    }
                })
                .max_by_key(|c| c.original)
            {
                return Self::prepare_reinstatement(
                    rows, history, view, revision, source, &p, mark, master, review, pending,
                );
            }
        }
        let mut restoring = None;
        if let Some(base) = master {
            let saved = base.invitation.as_ref().ok_or("Missing weekly identity")?;
            if let Some(original) = p.original {
                let canonical = original.in_series(base).ok();
                let date = canonical.map_or(original.date, |slot| slot.date);
                if let Some(gap) = saved.cancelled_intervals.iter().find(|g| g.contains(date)) {
                    let Some(slot) = canonical else {
                        return Ok(review.blocked(
                            "The original slot differs from the cancelled weekly schedule.",
                        ));
                    };
                    restoring = match restore_one::SingleDate::review(gap, saved, &p, source) {
                        Ok(one) => Some(one),
                        Err(reason) => return Ok(review.blocked(reason)),
                    };
                    p.original = Some(slot);
                } else if let Some(slot) =
                    canonical.filter(|s| !p.this_and_future && *s != original)
                {
                    // A single restoration records its source-clock slot. Match the same
                    // occurrence on replay if Mail used an equivalent clock representation.
                    if rows.iter().any(|r| {
                        r.kind
                            == Kind::Occurrence {
                                series: base.id,
                                original: slot.date,
                            }
                            && r.invitation
                                .as_ref()
                                .is_some_and(|o| o.original == Some(slot))
                    }) {
                        p.original = Some(slot);
                    }
                }
            }
            if p.exceptions
                .iter()
                .any(|m| base.remains_cancelled(m.original.date))
                || p.skips.iter().any(|o| base.remains_cancelled(o.date))
            {
                return Ok(review.blocked("These original dates remain in a cancelled interval."));
            }
        }
        let original_date = p.original.map(|o| o.date);
        if p.this_and_future && cancelled {
            return Self::prepare_range_cancellation(
                rows, history, view, revision, source, &p, key, master, review, pending,
            );
        }
        // Only a saved series can give a RECURRENCE-ID its meaning.
        let inherited = master
            .zip(p.original)
            .and_then(|(m, o)| m.inherited(o.date));
        let mut explicit = master;
        if let Some(original) = p.original {
            let Some(base) =
                master.filter(|e| e.kind == Kind::Weekly && e.scheduled_on(original.date))
            else {
                review.previous = None;
                return Ok(review.blocked("No matching weekly occurrence in this calendar."));
            };
            explicit = rows.iter().find(|e| {
                e.kind
                    == Kind::Occurrence {
                        series: base.id,
                        original: original.date,
                    }
            });
            explicit = explicit.or(inherited.as_ref());
            review.previous = explicit.cloned().or_else(|| {
                let mut occurrence = base.clone();
                occurrence.date = original.date;
                Some(occurrence)
            });
            let original_matches = match explicit.and_then(|e| e.invitation.as_ref()) {
                Some(saved) => saved.original == Some(original),
                None => std::iter::once(base).chain(prior_occurrence).any(|row| {
                    row.span.start == original.minute
                        && row.zone.map(|z| z.name) == original.zone
                        && row
                            .zone
                            .is_none_or(|z| z.interval(original.date, row.span, true).is_ok())
                }),
            };
            if !original_matches {
                return Ok(
                    review.blocked("The original time or zone differs from the saved series.")
                );
            }
        }
        let relevant = |c: &&Cancellation| {
            c.key == key
                && (c.original.is_none()
                    || c.this_and_future && original_date.is_none()
                    || c.original == original_date
                    || cancelled && original_date.is_none())
        };
        if history
            .iter()
            .filter(relevant)
            .any(|c| c.sequence >= p.sequence)
        {
            return Ok(review.blocked("This revision was already cancelled or superseded."));
        }
        if cancelled && master.is_none() {
            return Ok(review.blocked("No saved meeting matches this cancellation."));
        }
        if let Some(base) = master {
            let origin = base.invitation.as_ref().unwrap();
            if p.weekly_cancellation && base.kind != Kind::Weekly {
                return Ok(review.blocked(
                    "This cancellation names a series but the saved event is a single meeting.",
                ));
            }
            if cancelled && origin.sequence == p.sequence {
                return Ok(review.blocked("The cancellation conflicts with the saved revision."));
            }
            if origin.sequence > p.sequence {
                return Ok(review.blocked("An older revision. The saved meeting is shown here."));
            }
            if base.fields_on(base.date) != origin.accepted {
                return Ok(review
                    .blocked("You edited this meeting locally. Review it before replacing it."));
            }
            if let Some(row) = &p.row {
                if p.original.is_none() && row.kind != base.kind {
                    return Ok(
                        review.blocked("The recurrence pattern differs from the saved meeting.")
                    );
                }
                if base.kind == Kind::Weekly
                    && p.original.is_none()
                    && (!base.excluded.is_empty() || !origin.cancelled_intervals.is_empty())
                    && row.date != base.date
                {
                    return Ok(
                        review.blocked("The new series start conflicts with existing exceptions.")
                    );
                }
            }
            if let Some(date) = original_date {
                if restoring.is_none() && base.excluded.contains(&date) && explicit.is_none() {
                    let organizer_skip = origin.owned_skips.contains(&date);
                    if organizer_skip && (cancelled || p.sequence <= origin.sequence) {
                        return Ok(review.blocked("This date is already skipped by the organizer."));
                    }
                    if !organizer_skip
                        && !history
                            .iter()
                            .any(|c| c.key == key && c.original == Some(date))
                    {
                        return Ok(review.blocked("This occurrence was removed locally."));
                    }
                    if cancelled {
                        return Ok(review.blocked("This occurrence was already removed."));
                    }
                }
            }
            if cancelled && original_date.is_none() && base.kind == Kind::Weekly {
                for date in &base.excluded {
                    let child = rows.iter().find(|e| {
                        e.kind
                            == Kind::Occurrence {
                                series: base.id,
                                original: *date,
                            }
                    });
                    let recorded = origin.owned_skips.contains(date)
                        || history
                            .iter()
                            .any(|c| c.key == key && c.original == Some(*date));
                    if child.is_some_and(|e| {
                        !e.invitation.as_ref().is_some_and(|o| {
                            o.original.is_some()
                                && o.accepted == e.fields_on(e.date)
                                && o.sequence < p.sequence
                        })
                    }) || child.is_none() && !recorded
                    {
                        return Ok(review.blocked(
                            "A newer or locally edited exception needs separate review.",
                        ));
                    }
                }
            }
        }
        let last = p
            .limit
            .zip(p.row.as_ref())
            .map(|(rule, row)| rule.last(row))
            .transpose()?;
        if !cancelled
            && history.iter().any(|c| {
                c.key == key
                    && c.this_and_future
                    && c.original.is_some_and(|cut| {
                        original_date
                            .map_or_else(|| last.is_none_or(|end| end >= cut), |date| date >= cut)
                    })
            })
        {
            return Ok(review.blocked("This request includes dates in a cancelled range."));
        }
        if let Some(base) = master.filter(|_| p.original.is_none()) {
            let saved = base.invitation.as_ref().unwrap();
            if let Some(sequence) = saved.range_sequence {
                if p.sequence <= sequence {
                    return Ok(
                        review.blocked("A saved range update supersedes this series revision.")
                    );
                }
                if !cancelled && last != saved.last {
                    if saved.ranges.iter().map(|r| r.sequence).max() != Some(sequence) {
                        return Ok(review.blocked(
                            "A new organizer range update is needed to confirm the inherited time.",
                        ));
                    }
                    if p.row.as_ref().is_some_and(|row| {
                        row.date != base.date || row.span != base.span || row.zone != base.zone
                    }) {
                        return Ok(review.blocked("The extension also changes the underlying weekly time. Review it separately."));
                    }
                    if let Some(old) = saved.last.filter(|old| last.is_none_or(|end| end > *old)) {
                        let mut date = old;
                        while let Some(next) = date.checked_add_days(chrono::Days::new(7)) {
                            date = next;
                            if last.is_some_and(|end| date > end) {
                                break;
                            }
                            if p.skips.iter().any(|s| s.date == date)
                                || p.exceptions.iter().any(|m| m.original.date == date)
                            {
                                continue;
                            }
                            if let Some(row) = base
                                .range_at(date)
                                .and_then(|r| r.occurrence(base, date).ok())
                            {
                                review.range_summary = Some(format!(
                                    "New dates inherit the reviewed time · first {}",
                                    row.date
                                ));
                                review.range_dates.push(crate::invitation::RangeDate {
                                    private_copy: None,
                                    original: date,
                                    before: None,
                                    after: Some(row.fields_on(row.date)),
                                });
                                break;
                            }
                            if (date - old).num_days()
                                > 7 * (crate::invitation::MAX_MOVES as i64 + 2)
                            {
                                return Ok(
                                    review.blocked("Cannot resolve the first extended meeting.")
                                );
                            }
                        }
                    }
                }
            }
            if last.is_some_and(|end| base.excluded.iter().any(|d| *d > end)) {
                return Ok(review
                    .blocked("The new end would drop a saved exception. Review that date first."));
            }
        }
        let included: Vec<_> = p.exceptions.iter().map(|m| m.original).collect();
        let mut replay = false;
        if let Some(saved) = explicit {
            let Some(o) = saved
                .invitation
                .as_ref()
                .filter(|o| o.original == p.original)
            else {
                return Ok(review.blocked("This occurrence was edited locally."));
            };
            if o.accepted != saved.fields_on(saved.date) {
                return Ok(review
                    .blocked("You edited this meeting locally. Review it before replacing it."));
            }
            if o.sequence > p.sequence {
                return Ok(review.blocked("An older revision of this meeting."));
            }
            if o.sequence == p.sequence {
                replay = p.row.as_ref().is_some_and(|r| {
                    r.fields_on(r.date) == o.accepted
                        && o.range_from == range_from
                        && o.location == p.location
                        && o.limit == p.limit
                        && o.included == included
                        && o.skips == p.skips
                });
                if !replay {
                    return Ok(review.blocked("Conflicting changes use the same revision."));
                }
            }
        }
        if !source.verified
            && !replay
            && (cancelled || master.is_some() || history.iter().any(|c| c.key == key))
        {
            return Ok(review.blocked("Mail could not verify the sender of this change."));
        }
        let date = p
            .row
            .as_ref()
            .or(review.previous.as_ref())
            .ok_or("Missing schedule")?;
        let date = date.fields_on(date.date).agenda_date(view)?;
        let mut next = rows.to_vec();
        let mut marks = history.to_vec();
        let mut local_moves = Vec::new();
        if let (Some(original), Some(base)) = (original_date, master) {
            let parent = next.iter_mut().find(|e| e.id == base.id).unwrap();
            if let Some(o) = &mut parent.invitation {
                o.owned_skips.retain(|d| *d != original);
                if let Some(one) = restoring {
                    local_moves.extend(one.prepare(o, original, rows, &p, &mut review)?);
                }
            }
            let exclusions = &mut parent.excluded;
            if let Err(at) = exclusions.binary_search(&original) {
                exclusions.insert(at, original);
            }
        }
        if let Some(mut row) = p.row {
            row.id = explicit.map_or_else(Uuid::new_v4, |e| e.id);
            if let (Some(original), Some(base)) = (original_date, master) {
                row.kind = Kind::Occurrence {
                    series: base.id,
                    original,
                };
            } else if let Some(base) = master {
                row.excluded.clone_from(&base.excluded);
            }
            row.invitation = Some(Origin {
                cancelled_intervals: master
                    .filter(|_| row.kind == Kind::Weekly)
                    .and_then(|m| m.invitation.as_ref())
                    .map_or_else(Vec::new, |o| o.cancelled_intervals.clone()),
                ranges: if row.kind == Kind::Weekly {
                    master.map_or_else(Vec::new, |m| m.ranges().to_vec())
                } else {
                    vec![]
                },
                range_from,
                range_sequence: if row.kind == Kind::Weekly {
                    master.and_then(|m| m.invitation.as_ref()?.range_sequence)
                } else {
                    None
                },
                key,
                limit: p.limit,
                last,
                included,
                skips: p.skips,
                owned_skips: vec![],
                sequence: p.sequence,
                accepted: row.fields_on(row.date),
                location: p.location,
                original: p.original,
            });
            if row.kind == Kind::Weekly {
                match super::invitation_skips::prepare(
                    &mut row,
                    master,
                    &mut next,
                    &p.exceptions,
                    replay,
                    &mut review.skips,
                ) {
                    Ok(conflicts) => local_moves = conflicts,
                    Err(reason) => return Ok(review.blocked(reason)),
                }
            }
            if let Some(index) = next.iter().position(|e| e.id == row.id) {
                next[index] = row;
            } else {
                next.push(row);
            }
        } else {
            let base = master.unwrap();
            if original_date.is_none() {
                next.retain(|e| e.id != base.id && e.series_id() != Some(base.id));
            } else if let Some(e) = explicit {
                next.retain(|row| row.id != e.id);
            }
            marks.retain(|c| {
                c.key != key || (original_date.is_some() && c.original != original_date)
            });
            marks.push(Cancellation {
                tail: None,
                this_and_future: false,
                key,
                original: original_date,
                sequence: p.sequence,
            });
        }
        review.can_save = !replay;
        if replay {
            review.notice = "Already saved in this calendar.".into();
        } else if !source.verified {
            review.notice = "Mail has not verified the sender.".into();
        }
        let token = Uuid::new_v4();
        *pending = Some(Pending {
            token,
            revision,
            created: Instant::now(),
            date,
            local_moves,
            changes: (!replay).then_some((next, marks)),
        });
        review.token = Some(token);
        Ok(review)
    }

    pub fn check_invitation(&self, revision: u64, token: Uuid) -> Result<(), String> {
        self.invite
            .as_ref()
            .filter(|p| {
                p.token == token
                    && p.revision == revision
                    && self.state.revision == revision
                    && p.created.elapsed().as_secs() < REVIEW_SECONDS
            })
            .map(|_| ())
            .ok_or_else(|| "This review expired. Reopen the invitation in Mail.".into())
    }

    pub fn apply_invitation(
        &mut self,
        revision: u64,
        token: Uuid,
    ) -> Result<(NaiveDate, UndoReceipt), String> {
        self.apply_reviewed_invitation(revision, token, &[])
    }

    pub fn apply_reviewed_invitation(
        &mut self,
        revision: u64,
        token: Uuid,
        choices: &[crate::invitation::SkipDecision],
    ) -> Result<(NaiveDate, UndoReceipt), String> {
        self.check_invitation(revision, token)?;
        let pending = self.invite.as_ref().unwrap();
        let date = pending.date;
        let (mut rows, marks) = pending
            .changes
            .clone()
            .ok_or("This revision is already saved")?;
        super::invitation_skips::resolve(&mut rows, &pending.local_moves, choices)?;
        self.commit_invitation(revision, rows, marks, date)
            .map(|receipt| (date, receipt))
    }
}
