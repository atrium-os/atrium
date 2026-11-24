//! Only Mail submits an RSVP. Recheck the selected message at explicit Send.
use crate::{Engine, Error};
use mail_schema::{InviteMethod, PartKind, SenderCheck};
use mail_store::{MsgId, OccurrenceSlot};
use portcullis_ipc::calendar::{address, Delivery, Reply, Source, FILE_MAX};

#[derive(serde::Deserialize)]
struct IndividualIdentity {
    uid: String,
    sequence: u32,
    organizer: String,
    attendees: Vec<String>,
    row: Option<serde::de::IgnoredAny>,
    weekly_skips: Option<Vec<String>>,
    /// Each included move: original slot, new time, place.
    #[serde(default)]
    weekly_moves: Vec<(String, String, String)>,
}

#[derive(Debug, Clone)]
pub enum LatestInvitation {
    Update {
        msg: MsgId,
        sequence: u32,
        when: String,
        location: String,
    },
    Conflict {
        sequence: u32,
    },
}

fn occurrence_label(property: &str) -> Result<(bool, String), Error> {
    let (head, value) = property
        .split_once(':')
        .ok_or(Error::Refused("Invalid occurrence"))?;
    let local = value.strip_suffix('Z').unwrap_or(value);
    let date = chrono::NaiveDateTime::parse_from_str(local, "%Y%m%dT%H%M%S")
        .map_err(|_| Error::Refused("Invalid occurrence time"))?;
    if local.len() != 15
        || local.as_bytes()[8] != b'T'
        || !local
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit())
    {
        return Err(Error::Refused("Invalid occurrence time"));
    }
    let range = head.strip_prefix("RECURRENCE-ID;RANGE=THISANDFUTURE");
    let params = range
        .or_else(|| head.strip_prefix("RECURRENCE-ID"))
        .ok_or(Error::Refused("Invalid occurrence identifier"))?;
    let zone = if params.is_empty() {
        if value.ends_with('Z') {
            "UTC"
        } else {
            "Floating"
        }
    } else {
        params
            .strip_prefix(";TZID=")
            .filter(|z| {
                !z.is_empty()
                    && z.len() <= 64
                    && !value.ends_with('Z')
                    && z.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_/+-".contains(&b))
            })
            .ok_or(Error::Refused("Invalid occurrence zone"))?
    };
    Ok((
        range.is_some(),
        format!("{} · {zone}", date.format("%Y-%m-%d %H:%M")),
    ))
}

enum ResponseScope {
    Individual,
    Range(OccurrenceSlot, String),
    Group(usize),
    WeeklySkipped,
    /// The weekly invitation moves this occurrence to the time shown.
    WeeklyMoved(String),
    WeeklyRevised,
}

impl ResponseScope {
    fn same_effect(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Range(a, _), Self::Range(b, _)) => a == b,
            (Self::WeeklyMoved(a), Self::WeeklyMoved(b)) => a == b,
            (Self::WeeklySkipped, Self::WeeklySkipped)
            | (Self::WeeklyRevised, Self::WeeklyRevised)
            | (Self::Group(_), Self::Group(_))
            | (Self::Individual, Self::Individual) => true,
            _ => false,
        }
    }
}

// The two parser workers have agreed on every component of this scope. A
// sibling's SEQUENCE has no bearing on whether this reply is still current.
fn grouped_revision(scope: &str, wanted: OccurrenceSlot) -> Option<(u32, ResponseScope)> {
    let listed = scope.strip_prefix("cancel-dates|")?.split('|');
    let count = listed.clone().count();
    if !(2..=8).contains(&count) {
        return None;
    }
    let mut matched = None;
    for component in listed {
        let (seq, property) = component.split_once('/')?;
        if wanted.matches(OccurrenceSlot::parse(property)?) {
            if matched.is_some() {
                return None;
            }
            matched = Some(seq.parse().ok()?);
        }
    }
    Some((matched?, ResponseScope::Group(count)))
}

/// What a verified weekly revision does to the occurrence being answered,
/// with the place it moves to. Mail's wire scope and the parser's checked
/// proof must agree on whether it skips or moves that original slot. A
/// bundled move is found by its original slot: another occurrence moved to
/// the same new time is a different slot and stays answerable.
fn weekly_effect(
    wanted: OccurrenceSlot,
    raw: &str,
    skips: &[String],
    moves: &[(String, String, String)],
    original: &mail_schema::Invite,
) -> Option<(ResponseScope, String)> {
    let matches = |slot: Option<OccurrenceSlot>| slot.is_some_and(|found| wanted.matches(found));
    let wire_skip = raw.split('|').any(|part| matches(OccurrenceSlot::exdate(part)));
    let checked_skip = skips.iter().any(|s| matches(OccurrenceSlot::parse(s)));
    let wire_move = raw.split('|').skip(1).any(|part| matches(OccurrenceSlot::parse(part)));
    let checked_move = moves.iter().find(|(slot, ..)| matches(OccurrenceSlot::parse(slot)));
    if wire_skip != checked_skip
        || wire_move != checked_move.is_some()
        || (checked_skip && checked_move.is_some())
    {
        return None;
    }
    Some(match checked_move {
        // The weekly invitation restating the reviewed move changes nothing.
        Some((_, when, place))
            if proposed_time(original).as_ref() == Some(when) && original.location == *place =>
        {
            (ResponseScope::WeeklyRevised, String::new())
        }
        Some((_, when, place)) => (ResponseScope::WeeklyMoved(when.clone()), place.clone()),
        None if checked_skip => (ResponseScope::WeeklySkipped, String::new()),
        None => (ResponseScope::WeeklyRevised, String::new()),
    })
}

struct ResponseRevision {
    msg: MsgId,
    sequence: u32,
    invite: mail_schema::Invite,
    scope: ResponseScope,
    conflict: bool,
}

impl ResponseRevision {
    fn consider(mut self, latest: &mut Option<Self>) {
        if let Some(previous) = latest {
            if self.sequence < previous.sequence {
                return;
            }
            if self.sequence == previous.sequence {
                self.conflict |= previous.conflict
                    || !same_schedule(&previous.invite, &self.invite)
                    || !previous.scope.same_effect(&self.scope);
                if self.msg < previous.msg {
                    previous.conflict = self.conflict;
                    return;
                }
            }
        }
        *latest = Some(self);
    }
}

fn same_schedule(a: &mail_schema::Invite, b: &mail_schema::Invite) -> bool {
    a.cancelled == b.cancelled && a.start == b.start && a.end == b.end && a.location == b.location
}

pub struct Selected {
    pub source: Source,
    pub bytes: Vec<u8>,
    pub blob: mail_store::Hash,
}
/// Use the original invitation in Mail for both confirmation and the outgoing text.
pub fn proposed_time(invite: &mail_schema::Invite) -> Option<String> {
    use chrono::{NaiveDate, NaiveDateTime};
    use mail_schema::When;
    let clock = |value: &When| -> Option<(NaiveDateTime, String)> {
        match value {
            When::Utc(t) => Some((
                chrono::DateTime::from_timestamp(*t, 0)?.naive_utc(),
                "UTC".into(),
            )),
            When::Zoned { local, tzid } => Some((
                NaiveDateTime::parse_from_str(local, "%Y%m%dT%H%M%S").ok()?,
                tzid.clone(),
            )),
            When::Floating(local) => Some((
                NaiveDateTime::parse_from_str(local, "%Y%m%dT%H%M%S").ok()?,
                "Floating".into(),
            )),
            When::Date(_) => None,
        }
    };
    let start = invite.start.as_ref()?;
    if let When::Date(date) = start {
        let date = NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
        return Some(format!("{date} · All day"));
    }
    let (start, zone) = clock(start)?;
    let (end, end_zone) = clock(invite.end.as_ref()?)?;
    if zone != end_zone {
        return None;
    }
    let end_format = if start.date() == end.date() {
        "%H:%M"
    } else {
        "%Y-%m-%d %H:%M"
    };
    Some(format!(
        "{}–{} · {zone}",
        start.format("%Y-%m-%d · %H:%M"),
        end.format(end_format)
    ))
}

impl Engine {
    pub fn selected_invitation(&self, msg: MsgId) -> Result<Selected, Error> {
        self.extract_invitation(&self.store.message(msg)?, || self.parser.start())
    }

    fn extract_invitation(
        &self,
        m: &mail_store::Message,
        worker: impl FnOnce() -> Result<crate::Jail, Error>,
    ) -> Result<Selected, Error> {
        let blob = m
            .full
            .ok_or(Error::Refused("Download the full invitation first"))?;
        let summary = &m.summary;
        let invite = summary
            .invite
            .as_ref()
            .filter(|i| {
                (i.method == InviteMethod::Request && !i.cancelled)
                    || i.method == InviteMethod::Cancel
            })
            .ok_or(Error::Refused(
                "This message does not contain a supported scheduling request",
            ))?;
        let mut parts = summary
            .parts
            .iter()
            .filter(|p| p.kind == PartKind::Calendar);
        let p = parts
            .next()
            .ok_or(Error::Refused("No scheduling part in this message"))?;
        if parts.next().is_some()
            || summary.parts.iter().any(|p| p.kind == PartKind::Message)
            || m.summary.encrypted
        {
            return Err(Error::Refused(
                "Review a single directly delivered calendar part",
            ));
        }
        if p.size > (FILE_MAX as u64) * 2 {
            return Err(Error::Refused("Calendar part too large"));
        }
        let from = match summary.from.as_slice() {
            [a] => a.addr.to_ascii_lowercase(),
            _ => String::new(),
        };
        let matches = summary
            .reply_to
            .iter()
            .all(|a| a.addr.eq_ignore_ascii_case(&from))
            && summary
                .sender
                .as_ref()
                .is_none_or(|a| a.addr.eq_ignore_ascii_case(&from));
        let raw = self.store.blob(&blob)?;
        let len = raw.len() as u64;
        let mut jail = worker()?;
        mail_schema::write_frame(
            &mut jail.w,
            &mail_schema::ToJail::SchedulingPart {
                message: mail_schema::Raw { id: 1, bytes: raw },
                path: p.path.clone(),
            },
        )
        .map_err(|e| Error::Jail(e.to_string()))?;
        let mut frames = crate::Parser::exchange(
            &mut jail.r,
            &mut jail.w,
            &|id| (id == 1).then_some(len),
            &[],
            &|_| None,
        )?;
        let Some(mail_schema::FromJail::SchedulingPart {
            id: 1,
            result: Ok((bytes, scope)),
        }) = frames.pop()
        else {
            return Err(Error::Refused(
                "This invitation has an unsupported recurrence pattern or scope",
            ));
        };
        if !frames.is_empty() {
            return Err(Error::Jail("Unexpected scheduling response".into()));
        }
        let source = Source {
            delivery: (scope.is_some() || invite.cancelled).then(|| Delivery {
                uid: invite.uid.clone(),
                sequence: invite.sequence,
                organizer: invite.organizer.clone(),
                cancelled: invite.cancelled,
                scope,
            }),

            account: m.account,
            from,
            reply_destination_matches: matches,
            identities: self
                .identities(m.account)?
                .into_iter()
                .map(|i| i.addr)
                .collect(),
            verified: matches!(summary.sender_check, SenderCheck::Verified { .. }),
        };
        if !source.valid() {
            return Err(Error::Refused("Invalid Mail identity for this invitation"));
        }
        Ok(Selected {
            source,
            bytes,
            blob,
        })
    }

    /// Prepare an unopened inbox delivery. A permanent content refusal consumes
    /// its queue record. Transport, store and worker failures remain retryable.
    pub fn unopened_update(
        &self,
        msg: MsgId,
    ) -> Result<Option<portcullis_ipc::calendar::BackgroundUpdate>, Error> {
        let m = match self.store.message(msg) {
            Ok(m) => m,
            Err(mail_store::Error::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut inbox = false;
        for (folder, _) in &m.locations {
            inbox |= self.store.folder(*folder)?.role == mail_store::Role::Inbox;
        }
        if !inbox
            || !m.held.is_empty()
            || !matches!(m.summary.sender_check, SenderCheck::Verified { .. })
        {
            return Ok(None);
        }
        self.individual_update(&m, None, None)
    }

    fn individual_update(
        &self,
        m: &mail_store::Message,
        attendee: Option<&str>,
        slot: Option<mail_store::OccurrenceSlot>,
    ) -> Result<Option<portcullis_ipc::calendar::BackgroundUpdate>, Error> {
        use mail_schema::{FromJail, Raw, ToJail};
        let Some(i) = m.summary.invite.as_ref().filter(|i| {
            (!i.repeats || slot.is_some() && i.method == InviteMethod::Request)
                && (i.sequence > 0 || slot.is_some() && i.method == InviteMethod::Cancel)
        }) else {
            return Ok(None);
        };
        let selected = match self.extract_invitation(m, crate::Parser::installed_worker) {
            Ok(s) => s,
            Err(Error::Refused(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut source = selected.source;
        let scope = source.delivery.as_ref().and_then(|d| d.scope.clone());
        match (slot, scope.as_deref()) {
            (None, None) => (),
            (Some(wanted), Some(raw))
                if (i.repeats
                    && i.method == InviteMethod::Request
                    && raw.split([';', '|']).next() == Some("weekly"))
                    || mail_store::OccurrenceSlot::parse(raw)
                        .is_some_and(|found| wanted.matches(found))
                    || (i.method == InviteMethod::Cancel
                        && (raw.starts_with("cancel-dates|")
                            || OccurrenceSlot::range_cutoff(raw)
                                .is_some_and(|cutoff| wanted.on_or_after(cutoff)))) =>
            {
                ()
            }
            _ => return Ok(None),
        }
        if !source.verified
            || !source.reply_destination_matches
            || !i.organizer.eq_ignore_ascii_case(&source.from)
        {
            return Ok(None);
        }
        source.delivery = Some(Delivery {
            uid: i.uid.clone(),
            sequence: i.sequence,
            organizer: i.organizer.clone(),
            cancelled: i.cancelled,
            scope: scope.clone(),
        });
        if let Some(attendee) = attendee {
            source
                .identities
                .retain(|a| a.eq_ignore_ascii_case(attendee));
            if source.identities.is_empty() {
                return Ok(None);
            }
        }
        let mut jail = crate::Parser::installed_worker()?;
        let len = selected.bytes.len() as u64;
        let part = Raw {
            id: 1,
            bytes: selected.bytes,
        };
        let identities = source.identities.clone();
        let check = match scope {
            Some(scope) => ToJail::OccurrenceUpdate {
                part,
                identities,
                scope,
            },
            None => ToJail::OneOffUpdate { part, identities },
        };
        mail_schema::write_frame(&mut jail.w, &check).map_err(|e| Error::Jail(e.to_string()))?;
        let frames = crate::Parser::exchange(
            &mut jail.r,
            &mut jail.w,
            &|id| (id == 1).then_some(len),
            &[],
            &|_| None,
        )?;
        let [FromJail::IndividualUpdate { id: 1, proposal }] = frames.as_slice() else {
            return Err(Error::Jail("Invalid meeting update response".into()));
        };
        Ok(proposal
            .as_ref()
            .map(|proposal| portcullis_ipc::calendar::BackgroundUpdate {
                source,
                proposal: proposal.clone(),
            }))
    }

    /// Bind the Calendar choice to the scope in the immutable selected Mail part.
    pub fn invitation_scope(&self, msg: MsgId, reply: &Reply) -> Result<Option<String>, Error> {
        let selected = self.selected_invitation(msg)?;
        let scope = selected.source.delivery.and_then(|d| d.scope);
        if scope != reply.scope {
            return Err(Error::Refused("The invitation scope has changed"));
        }
        match scope.as_deref() {
            None => Ok(None),
            Some(s) if s.split([';', '|']).next() == Some("weekly") => {
                let mut parts = s.split('|');
                let rule = parts.next().unwrap();
                let mut label = "Whole weekly series".to_string();
                if let Some(limit) = rule.strip_prefix("weekly;") {
                    if let Some(n) = limit
                        .strip_prefix("COUNT=")
                        .and_then(|v| v.parse::<u16>().ok())
                        .filter(|n| (1..=520).contains(n))
                    {
                        label.push_str(&format!(" · {n} meetings"));
                    } else if let Some(until) = limit.strip_prefix("UNTIL=") {
                        let wall = chrono::NaiveDateTime::parse_from_str(
                            until.trim_end_matches('Z'),
                            "%Y%m%dT%H%M%S",
                        )
                        .map_err(|_| Error::Refused("Invalid end time"))?;
                        label.push_str(&format!(
                            " · until {} {}",
                            wall,
                            if until.ends_with('Z') {
                                "UTC"
                            } else {
                                "floating"
                            }
                        ));
                    } else {
                        return Err(Error::Refused("Invalid weekly limit"));
                    }
                }
                let (mut skips, mut moves) = (0, 0);
                for part in parts {
                    if part.starts_with("EXDATE:") || part.starts_with("EXDATE;TZID=") {
                        skips += 1;
                    } else {
                        moves += 1;
                    }
                }
                if skips > 0 {
                    label.push_str(&format!(" · {skips} skipped dates"));
                }
                if moves > 0 {
                    label.push_str(&format!(" · {moves} included moves"));
                }
                Ok(Some(label))
            }
            Some(property) => {
                let (range, stamp) = occurrence_label(property)?;
                let prefix = if range {
                    "This and future occurrences ·"
                } else {
                    "One occurrence · originally"
                };
                Ok(Some(format!("{prefix} {stamp}")))
            }
        }
    }

    /// Compare the revision for the reviewed occurrence. A conflict at the highest
    /// verified sequence needs a later organizer revision to disambiguate it.
    fn check_individual_response(
        &self,
        msg: MsgId,
        account: u32,
        slot: Option<mail_store::OccurrenceSlot>,
        original: &mail_schema::Invite,
        r: &Reply,
    ) -> Result<mail_store::CalendarRevisions, Error> {
        // Old stores may still be backfilling when the first RSVP is sent.
        self.store.index_calendar_before_send()?;
        // The index stores the first component's sequence. A later component
        // in the same cancellation can have a higher revision for this date.
        let floor = if slot.is_some() { 0 } else { r.sequence };
        let revisions = self
            .store
            .write(|w| w.calendar_revisions(account, &r.uid, floor, &r.organizer))?;
        let mut latest: Option<ResponseRevision> = None;
        let mut latest_weekly = None;
        // Set once a verified weekly revision skips or moves this occurrence.
        let mut saw_change = false;
        for &(seq, id, hash) in &revisions.messages {
            if id == msg {
                continue;
            }
            let m = match self.store.message(id) {
                Ok(m)
                    if m.full == Some(hash)
                        && matches!(m.summary.sender_check, SenderCheck::Verified { .. }) =>
                {
                    m
                }
                Ok(_) | Err(mail_store::Error::NotFound) => {
                    return Err(mail_store::Error::CalendarChanged.into())
                }
                Err(e) => return Err(e.into()),
            };
            if seq < latest.as_ref().map_or(r.sequence, |v| v.sequence)
                && !(slot.is_some()
                    && m.summary.invite.as_ref().is_some_and(|i| {
                        i.method == InviteMethod::Cancel || i.repeats && seq >= r.sequence
                    }))
            {
                continue;
            }
            let Some(update) = self.individual_update(&m, Some(&r.attendee), slot)? else {
                continue;
            };
            let Some(delivery) = update.source.delivery else {
                continue;
            };
            let proof: IndividualIdentity = serde_json::from_str(&update.proposal)
                .map_err(|_| Error::Jail("Invalid individual scheduling proof".into()))?;
            if delivery.uid != r.uid
                || delivery.sequence != seq
                || !delivery.organizer.eq_ignore_ascii_case(&r.organizer)
                || proof.uid != r.uid
                || proof.sequence != seq
                || !proof.organizer.eq_ignore_ascii_case(&r.organizer)
                || delivery.cancelled != proof.row.is_none()
                || !(proof.attendees.len() == 1
                    && proof.attendees[0].eq_ignore_ascii_case(&r.attendee)
                    || delivery.cancelled && proof.attendees.is_empty())
            {
                continue;
            }
            let Some(mut i) = m.summary.invite else {
                return Err(mail_store::Error::CalendarChanged.into());
            };
            if i.repeats {
                let (Some(wanted), Some(raw), Some(skips)) =
                    (slot, delivery.scope.as_deref(), proof.weekly_skips)
                else {
                    continue;
                };
                if seq < r.sequence {
                    continue;
                }
                let Some((scope, place)) =
                    weekly_effect(wanted, raw, &skips, &proof.weekly_moves, original)
                else {
                    continue;
                };
                saw_change |= !matches!(scope, ResponseScope::WeeklyRevised);
                i.start = None;
                i.end = None;
                i.location = place;
                ResponseRevision {
                    msg: id,
                    sequence: seq,
                    invite: i,
                    conflict: false,
                    scope,
                }
                .consider(&mut latest_weekly);
                continue;
            }
            let (sequence, scope) = match delivery.scope.as_deref() {
                Some(raw) if raw.starts_with("cancel-dates|") => {
                    let Some(found) = slot.and_then(|wanted| grouped_revision(raw, wanted)) else {
                        continue;
                    };
                    // The summary fields describe the first component. Only the
                    // original slot and revision identify the date being answered.
                    i.start = None;
                    i.end = None;
                    i.location.clear();
                    found
                }
                Some(raw) => (
                    seq,
                    OccurrenceSlot::range_cutoff(raw).map_or(ResponseScope::Individual, |start| {
                        ResponseScope::Range(start, raw.to_owned())
                    }),
                ),
                None => (seq, ResponseScope::Individual),
            };
            if sequence == 0 || sequence < latest.as_ref().map_or(r.sequence, |v| v.sequence) {
                continue;
            }
            let clashes_with_review =
                sequence == r.sequence && matches!(scope, ResponseScope::Individual);
            if clashes_with_review && same_schedule(&i, original) {
                continue;
            }
            ResponseRevision {
                msg: id,
                sequence,
                invite: i,
                scope,
                conflict: clashes_with_review,
            }
            .consider(&mut latest);
        }
        // An unrelated weekly refresh leaves this occurrence answerable. Once
        // a verified edit skips or moves it, review the newest weekly revision,
        // even when that revision withdraws the change.
        if saw_change {
            if let Some(weekly) = latest_weekly {
                weekly.consider(&mut latest);
            }
        }
        if let Some(ResponseRevision {
            msg,
            sequence,
            invite,
            scope,
            conflict,
        }) = latest
        {
            if conflict {
                return Err(Error::InvitationChanged(LatestInvitation::Conflict {
                    sequence,
                }));
            }
            let when = match scope {
                ResponseScope::Range(_, property) => {
                    let (_, stamp) = occurrence_label(&property)?;
                    format!("Cancelled from {stamp} onward")
                }
                ResponseScope::WeeklySkipped => "Weekly invitation skips this occurrence".into(),
                ResponseScope::WeeklyMoved(to) => {
                    format!("Weekly invitation moves this occurrence · {to}")
                }
                ResponseScope::WeeklyRevised => "Weekly invitation revised after the change".into(),
                ResponseScope::Group(count) => {
                    format!("This meeting is one of {count} cancelled dates")
                }
                ResponseScope::Individual if invite.cancelled => {
                    "Cancelled by the organizer".into()
                }
                ResponseScope::Individual => proposed_time(&invite).ok_or(Error::Refused(
                    "Open the latest invitation to review its time.",
                ))?,
            };
            return Err(Error::InvitationChanged(LatestInvitation::Update {
                msg,
                sequence,
                when,
                location: invite.location,
            }));
        }
        Ok(revisions)
    }

    pub fn send_invitation_reply(
        &self,
        msg: MsgId,
        blob: mail_store::Hash,
        r: &Reply,
    ) -> Result<u64, Error> {
        let m = self.store.message(msg)?;
        let i = m.summary.invite.as_ref().ok_or(Error::NotFound)?;
        if m.full != Some(blob)
            || i.method != InviteMethod::Request
            || i.cancelled
            || i.uid != r.uid
            || i.sequence != r.sequence
            || !i.organizer.eq_ignore_ascii_case(&r.organizer)
            || !address(&r.organizer)
            || !address(&r.attendee)
            || r.uid.is_empty()
            || r.uid.len() > 200
            || !r
                .uid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
            || !matches!(&m.summary.from[..], [a] if a.addr.eq_ignore_ascii_case(&r.organizer))
            || m.summary
                .reply_to
                .iter()
                .any(|a| !a.addr.eq_ignore_ascii_case(&r.organizer))
            || m.summary
                .sender
                .as_ref()
                .is_some_and(|a| !a.addr.eq_ignore_ascii_case(&r.organizer))
        {
            return Err(Error::Refused(
                "Invitation identity or revision changed. Review it again.",
            ));
        }
        let scope = self.invitation_scope(msg, r)?;
        let raw = self.store.blob(&blob)?;
        let received = self
            .parser
            .summarize_for(
                &[(1, &raw)],
                false,
                &[],
                &|_| None,
                vec![r.attendee.to_ascii_lowercase()],
            )?
            .0
            .pop()
            .flatten()
            .and_then(|s| s.invite)
            .ok_or(Error::Refused(
                "The original invitation could not be verified",
            ))?;
        if received.uid != r.uid
            || received.sequence != r.sequence
            || received.method != InviteMethod::Request
            || received.my_status.is_none()
            || !received.organizer.eq_ignore_ascii_case(&r.organizer)
        {
            return Err(Error::Refused(
                "The original request does not name this attendee and organizer",
            ));
        }
        let ids = self.identities(m.account)?;
        let index = ids
            .iter()
            .position(|id| id.addr.eq_ignore_ascii_case(&r.attendee))
            .ok_or(Error::Refused("The invited identity is unavailable"))?;
        let mut draft = self.compose_new(m.account, index)?.draft;
        draft.to.push(mail_compose::Mailbox::new("", &r.organizer));
        draft.subject = format!("{} · {}", r.answer.partstat(), i.title);
        let schedule = proposed_time(&received)
            .ok_or(Error::Refused("The proposed time could not be verified"))?;
        draft.text = format!("{}\n{}\n{}", r.answer.partstat(), received.title, schedule);
        if let Some(scope) = &scope {
            draft.text.push_str(&format!("\n{scope}"));
        }
        draft.in_reply_to = m.summary.message_id.clone();
        let stamp = chrono::DateTime::from_timestamp(crate::now(), 0)
            .ok_or(Error::Refused("Invalid device time"))?
            .format("%Y%m%dT%H%M%SZ");
        // Fold content lines at 75 octets. Every inserted value is bounded ASCII.
        let mut data = String::new();
        let mut lines = vec![
            "BEGIN:VCALENDAR".into(),
            "PRODID:-//Atrium//Mail//EN".into(),
            "VERSION:2.0".into(),
            "METHOD:REPLY".into(),
            "BEGIN:VEVENT".into(),
            format!("DTSTAMP:{stamp}"),
            format!("UID:{}", r.uid),
            format!("SEQUENCE:{}", r.sequence),
            format!("ORGANIZER:mailto:{}", r.organizer),
            format!(
                "ATTENDEE;PARTSTAT={}:mailto:{}",
                r.answer.partstat(),
                r.attendee
            ),
            "END:VEVENT".into(),
            "END:VCALENDAR".into(),
        ];
        if let Some(id) = r.scope.as_ref().filter(|v| v.starts_with("RECURRENCE-ID")) {
            lines.insert(7, id.clone());
        }
        for line in lines {
            let mut rest = line.as_str();
            while rest.len() > 74 {
                data.push_str(&rest[..74]);
                data.push_str("\r\n ");
                rest = &rest[74..];
            }
            data.push_str(rest);
            data.push_str("\r\n");
        }
        draft.attachments.push(mail_compose::write::Attachment {
            name: "response.ics".into(),
            mime: "text/calendar; method=REPLY".into(),
            bytes: data.into_bytes(),
        });
        let mut key = format!(
            "{}/{}/{}/{}",
            m.account,
            r.organizer.to_ascii_lowercase(),
            r.attendee.to_ascii_lowercase(),
            r.uid
        );
        if let Some(id) = r.scope.as_ref().filter(|v| v.starts_with("RECURRENCE-ID")) {
            key.push('/');
            key.push_str(id);
        }
        // Recheck inside the outbox transaction. If sync committed between
        // parsing these parts and enqueueing, review the new set before sending.
        let slot = r
            .scope
            .as_deref()
            .and_then(mail_store::OccurrenceSlot::parse);
        if slot.is_none()
            && r.scope.as_deref().is_some_and(|s| {
                s.starts_with("RECURRENCE-ID:") || s.starts_with("RECURRENCE-ID;TZID=")
            })
        {
            return Err(Error::Refused(
                "The original occurrence time could not be verified.",
            ));
        }
        for _ in 0..3 {
            let checked = if r.scope.is_none() || slot.is_some() {
                match self.check_individual_response(msg, m.account, slot, i, r) {
                    Ok(checked) => Some(checked),
                    Err(Error::Store(mail_store::Error::CalendarChanged)) => continue,
                    Err(e) => return Err(e),
                }
            } else {
                None
            };
            let result = self.queue_message(
                m.account,
                &draft,
                Default::default(),
                Default::default(),
                Some((&key, r.sequence, checked.as_ref())),
            );
            if !matches!(
                result,
                Err(Error::Store(mail_store::Error::CalendarChanged))
            ) {
                return result;
            }
        }
        Err(mail_store::Error::CalendarChanged.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(day: u32, hour: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(2026, 12, day).unwrap().and_hms_opt(hour, 0, 0).unwrap().and_utc().timestamp()
    }

    /// The occurrence being answered: originally `slot`, moved by its own
    /// invitation to December 9, 15:00–15:45 UTC in Studio B.
    fn reviewed() -> mail_schema::Invite {
        mail_schema::Invite {
            method: InviteMethod::Request,
            uid: "studio-planning@fixture.invalid".into(),
            sequence: 3,
            title: "Studio planning".into(),
            start: Some(mail_schema::When::Utc(dec(9, 15))),
            end: Some(mail_schema::When::Utc(dec(9, 15) + 45 * 60)),
            location: "Studio B".into(),
            organizer: "mina@fixture.invalid".into(),
            attendees: 2,
            my_status: None,
            cancelled: false,
            repeats: false,
        }
    }

    #[test]
    fn a_bundled_weekly_move_pauses_only_its_original_slot() {
        const DEC_15: &str = "RECURRENCE-ID:20261215T140000Z";
        const DEC_22: &str = "RECURRENCE-ID:20261222T140000Z";
        let slot = |s| OccurrenceSlot::parse(s).unwrap();
        let original = reviewed();
        assert_eq!(proposed_time(&original).as_deref(), Some("2026-12-09 · 15:00–15:45 · UTC"));
        // December 22, given in New York time, moved on to December 23.
        let raw = "weekly;COUNT=4|RECURRENCE-ID;TZID=America/New_York:20261222T090000";
        let moves = vec![(
            "RECURRENCE-ID;TZID=America/New_York:20261222T090000".to_string(),
            "2026-12-23 · 11:00–11:45 · UTC".to_string(),
            "Studio C".to_string(),
        )];
        // Both occurrences had been moved to the same December 9 time.
        let (scope, place) = weekly_effect(slot(DEC_22), raw, &[], &moves, &original).unwrap();
        assert!(matches!(&scope, ResponseScope::WeeklyMoved(to) if to == "2026-12-23 · 11:00–11:45 · UTC"));
        assert_eq!(place, "Studio C");
        let (scope, place) = weekly_effect(slot(DEC_15), raw, &[], &moves, &original).unwrap();
        assert!(matches!(scope, ResponseScope::WeeklyRevised) && place.is_empty());
        // Restating the reviewed move changes nothing.
        let same = vec![(moves[0].0.clone(), "2026-12-09 · 15:00–15:45 · UTC".to_string(), "Studio B".to_string())];
        assert!(matches!(weekly_effect(slot(DEC_22), raw, &[], &same, &original), Some((ResponseScope::WeeklyRevised, _))));
        // Two moves to one new time: each pauses only its own slot's reply.
        let raw2 = format!("{raw}|{DEC_15}");
        let both = vec![moves[0].clone(), (DEC_15.into(), moves[0].1.clone(), "Studio C".into())];
        for s in [DEC_15, DEC_22] {
            assert!(matches!(weekly_effect(slot(s), &raw2, &[], &both, &original), Some((ResponseScope::WeeklyMoved(_), _))), "{s}");
        }
        assert!(matches!(
            weekly_effect(slot("RECURRENCE-ID:20261208T140000Z"), &raw2, &[], &both, &original),
            Some((ResponseScope::WeeklyRevised, _))
        ));
        // The wire scope and the checked proof must agree.
        assert!(weekly_effect(slot(DEC_22), "weekly;COUNT=4", &[], &moves, &original).is_none());
        assert!(weekly_effect(slot(DEC_22), raw, &[], &[], &original).is_none());
        // A skip and a move of one slot contradict each other.
        let skipped = "weekly;COUNT=4|EXDATE:20261222T140000Z|RECURRENCE-ID:20261222T140000Z";
        assert!(weekly_effect(slot(DEC_22), skipped, &[DEC_22.into()], &moves, &original).is_none());
        // A skip alone still pauses as before.
        assert!(matches!(
            weekly_effect(slot(DEC_22), "weekly;COUNT=4|EXDATE:20261222T140000Z", &[DEC_22.into()], &[], &original),
            Some((ResponseScope::WeeklySkipped, _))
        ));
    }
}
