use super::*;
use crate::invitation::{Cancellation, MeetingKey, Origin, Proposal, Review};
use portcullis_ipc::calendar::{Source, REVIEW_SECONDS};
use std::time::Instant;

pub(super) struct Pending {
    token: Uuid,
    revision: u64,
    created: Instant,
    date: NaiveDate,
    changes: Option<(Vec<Entry>, Vec<Cancellation>)>,
}

impl Store {
    pub fn review_invitation(
        &mut self,
        revision: u64,
        source: &Source,
        p: Proposal,
    ) -> Result<Review, String> {
        self.invite = None;
        if revision != self.state.revision {
            return Err("Calendar changed. Review this invitation again.".into());
        }
        p.validate(source)?;
        if !p.cancelled_dates.is_empty() {
            return self.review_cancelled_dates(revision, source, p);
        }
        let view = self.viewing_zone();
        let children = p
            .exceptions
            .iter()
            .map(|m| Proposal {
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
    fn prepare(
        rows: &[Entry],
        history: &[Cancellation],
        view: Tz,
        revision: u64,
        source: &Source,
        p: Proposal,
        prior_occurrence: Option<&Entry>,
        pending: &mut Option<Pending>,
    ) -> Result<Review, String> {
        let cancelled = p.row.is_none();
        let organizer = p.organizer.to_ascii_lowercase();
        let mut mine = source.identities.iter().filter(|id| {
            if cancelled && p.attendees.is_empty() {
                return rows.iter().filter_map(|e| e.invitation.as_ref()).any(|o| {
                    o.key.account == source.account
                        && o.key.organizer == organizer
                        && o.key.uid == p.uid
                        && o.key.attendee.eq_ignore_ascii_case(id)
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
        let original_date = p.original.map(|o| o.date);
        let mut review = Review {
            previous_dates: vec![],
            previous: master.cloned(),
            weekly: master.is_some_and(|e| e.kind == Kind::Weekly)
                || p.row.as_ref().is_some_and(|e| e.kind == Kind::Weekly),
            attendee,
            can_save: false,
            token: None,
            notice: String::new(),
        };
        // Only a saved series can give a RECURRENCE-ID its meaning.
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
        if !source.from.eq_ignore_ascii_case(&key.organizer) || !source.reply_destination_matches {
            return Ok(
                review.blocked("The Mail sender or reply address differs from the organizer.")
            );
        }

        let relevant = |c: &&Cancellation| {
            c.key == key
                && (c.original.is_none()
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
                    && !base.excluded.is_empty()
                    && row.date != base.date
                {
                    return Ok(
                        review.blocked("The new series start conflicts with existing exceptions.")
                    );
                }
            }
            if let Some(date) = original_date {
                if base.excluded.contains(&date) && explicit.is_none() {
                    if !history
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
                    let recorded = history
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
        if let Some(base) = master.filter(|_| p.original.is_none()) {
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
                        && o.location == p.location
                        && o.limit == p.limit
                        && o.included == included
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
        if let (Some(original), Some(base)) = (original_date, master) {
            let exclusions = &mut next.iter_mut().find(|e| e.id == base.id).unwrap().excluded;
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
                key,
                limit: p.limit,
                last,
                included,
                sequence: p.sequence,
                accepted: row.fields_on(row.date),
                location: p.location,
                original: p.original,
            });
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
        self.check_invitation(revision, token)?;
        let pending = self.invite.as_ref().unwrap();
        let date = pending.date;
        let (rows, marks) = pending
            .changes
            .clone()
            .ok_or("This revision is already saved")?;
        self.commit_invitation(revision, rows, marks, date)
            .map(|receipt| (date, receipt))
    }
}
