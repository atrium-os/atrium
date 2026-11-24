//! Only Mail submits an RSVP. Recheck the selected message at explicit Send.
use crate::{Engine, Error};
use mail_schema::{InviteMethod, PartKind, SenderCheck};
use mail_store::MsgId;
use portcullis_ipc::calendar::{address, Delivery, Reply, Source, FILE_MAX};

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
        let m = self.store.message(msg)?;
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
        let mut jail = self.parser.start()?;
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
                Ok(Some(format!(
                    "{} {} · {zone}",
                    if range.is_some() {
                        "This and future occurrences ·"
                    } else {
                        "One occurrence · originally"
                    },
                    date.format("%Y-%m-%d %H:%M")
                )))
            }
        }
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
        self.queue_message(
            m.account,
            &draft,
            Default::default(),
            Default::default(),
            Some((&key, r.sequence)),
        )
    }
}
