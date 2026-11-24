//! File-format code runs only in the disposable Calendar worker.
use atrium_calendar::{
    import::{Candidates, Issue, Outcome, Parse, Parsed, FILE_LIMIT, ROW_LIMIT},
    Entry, Kind, Span, Zone,
};
use chrono::{NaiveDate, NaiveDateTime, Timelike};
use chrono_tz::Tz;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Issue>;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    date: NaiveDate,
    minute: Option<u16>,
    zone: Option<Tz>,
}

fn stamp(params: &[&str], value: &str) -> Result<Stamp> {
    let mut zone = None;
    let mut value_type = None;
    for p in params {
        let (k, v) = p.split_once('=').ok_or(Issue::Format)?;
        if k.eq_ignore_ascii_case("TZID") && zone.is_none() {
            zone = Some(v.trim_matches('"').parse::<Tz>().map_err(|_| Issue::Zone)?);
        } else if k.eq_ignore_ascii_case("VALUE") && value_type.is_none() {
            value_type = Some(v.to_ascii_uppercase());
        } else {
            return Err(Issue::Time);
        }
    }
    if value_type.as_deref() == Some("DATE") {
        if zone.is_some() || value.len() != 8 {
            return Err(Issue::Time);
        }
        return Ok(Stamp {
            date: NaiveDate::parse_from_str(value, "%Y%m%d").map_err(|_| Issue::Time)?,
            minute: None,
            zone,
        });
    }
    if value_type.as_deref().is_some_and(|v| v != "DATE-TIME") {
        return Err(Issue::Time);
    }
    let wall = if let Some(v) = value.strip_suffix('Z') {
        if zone.replace(chrono_tz::UTC).is_some() {
            return Err(Issue::Zone);
        }
        v
    } else {
        value
    };
    if wall.len() != 15 || !wall.ends_with("00") {
        return Err(Issue::Time);
    }
    let dt = NaiveDateTime::parse_from_str(wall, "%Y%m%dT%H%M%S").map_err(|_| Issue::Time)?;
    Ok(Stamp {
        date: dt.date(),
        minute: Some((dt.hour() * 60 + dt.minute()) as u16),
        zone,
    })
}

#[derive(Default)]
struct Event {
    uid: Option<String>,
    title: Option<String>,
    start: Option<Stamp>,
    end: Option<Stamp>,
    original: Option<Stamp>,
    weekly: bool,
    limit: Option<atrium_calendar::WeeklyLimit>,
    organizer: Option<String>,
    sequence: Option<u32>,
    attendees: Vec<String>,
    location: Option<String>,
    cancelled_status: Option<bool>,
    skipped: Vec<Stamp>,
}
fn once<T>(slot: &mut Option<T>, v: T) -> Result<()> {
    if slot.replace(v).is_some() {
        Err(Issue::Format)
    } else {
        Ok(())
    }
}
fn title(value: &str) -> Result<String> {
    let mut text = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        let c = if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => ' ',
                Some(c @ (';' | ',' | '\\')) => c,
                _ => return Err(Issue::Format),
            }
        } else {
            c
        };
        if !c.is_control()
            && !matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
        {
            text.push(c)
        }
    }
    if text.trim().is_empty() || text.len() > 160 {
        return Err(Issue::Title);
    }
    Ok(text.trim().into())
}
impl Event {
    fn property(&mut self, key: &str, p: &[&str], v: &str, omitted: &mut bool) -> Result<()> {
        match key {
            "UID" if p.is_empty() && !v.is_empty() && v.len() <= 255 => {
                once(&mut self.uid, v.into())
            }
            "SUMMARY"
                if p.iter()
                    .all(|x| x.to_ascii_uppercase().starts_with("LANGUAGE=")) =>
            {
                once(&mut self.title, title(v)?)
            }
            "DTSTART" => once(&mut self.start, stamp(p, v)?),
            "DTEND" => once(&mut self.end, stamp(p, v)?),
            "RECURRENCE-ID" => once(&mut self.original, stamp(p, v)?),
            "RRULE" if !self.weekly && p.is_empty() && v.eq_ignore_ascii_case("FREQ=WEEKLY") => {
                self.weekly = true;
                Ok(())
            }
            "EXDATE" => {
                for item in v.split(',') {
                    if self.skipped.len() >= 256 {
                        return Err(Issue::Limit);
                    }
                    self.skipped.push(stamp(p, item)?)
                }
                Ok(())
            }
            "RRULE" | "RDATE" | "EXRULE" => Err(Issue::Recurrence),
            "TRANSP" if v.eq_ignore_ascii_case("OPAQUE") => Ok(()),
            "STATUS" if v.eq_ignore_ascii_case("CONFIRMED") => Ok(()),
            "TRANSP" | "STATUS" | "METHOD" => Err(Issue::Scheduling),
            "DURATION" => Err(Issue::Time),
            "DTSTAMP" | "LAST-MODIFIED" | "CREATED" => Ok(()),
            "DESCRIPTION" | "LOCATION" | "ORGANIZER" | "ATTENDEE" | "URL" | "ATTACH"
            | "SEQUENCE" | "CLASS" | "CATEGORIES" | "COMMENT" | "PRIORITY" => {
                *omitted = true;
                Ok(())
            }
            x if x.starts_with("X-") => {
                *omitted = true;
                Ok(())
            }
            _ => Err(Issue::Component),
        }
    }
    fn row(&self, exclusions: &[Stamp]) -> Result<Entry> {
        let start = self.start.ok_or(Issue::Format)?;
        let (span, kind) = if let Some(minute) = start.minute {
            let end = self.end.ok_or(Issue::Time)?;
            if end.zone != start.zone {
                return Err(Issue::Zone);
            }
            let end_min = end.minute.ok_or(Issue::Time)?;
            let end = if end.date == start.date {
                end_min
            } else if Some(end.date) == start.date.succ_opt() && end_min == 0 {
                1440
            } else {
                return Err(Issue::Time);
            };
            (
                Span { start: minute, end },
                if self.weekly {
                    Kind::Weekly
                } else {
                    Kind::Appointment
                },
            )
        } else {
            if self.weekly || self.original.is_some() {
                return Err(Issue::Recurrence);
            }
            if self
                .end
                .is_some_and(|e| Some(e.date) != start.date.succ_opt() || e.minute.is_some())
            {
                return Err(Issue::Time);
            }
            (
                Span {
                    start: 0,
                    end: 1440,
                },
                Kind::AllDay,
            )
        };
        let mut entry = Entry {
            id: Uuid::new_v4(),
            date: start.date,
            span,
            kind,
            title: self.title.clone().ok_or(Issue::Title)?,
            zone: start.zone.map(|name| Zone {
                name,
                start_fold: None,
                end_fold: None,
            }),
            excluded: vec![],
            source: None,
            import_hash: None,
            invitation: None,
        };
        for d in exclusions {
            if !self.weekly
                || !entry.scheduled_on(d.date)
                || d.minute != start.minute
                || d.zone != start.zone
            {
                return Err(Issue::Recurrence);
            }
            entry.excluded.push(d.date);
        }
        entry.excluded.sort_unstable();
        entry.excluded.dedup();
        if !span.valid() {
            return Err(Issue::Time);
        }
        entry.validate().map_err(|_| Issue::Zone)?;
        Ok(entry)
    }
}

#[cfg(test)]
fn parse(bytes: &[u8]) -> Result<Candidates> {
    match parse_mode(bytes, false)? {
        Outcome::Ready(c) => Ok(c),
        _ => Err(Issue::Format),
    }
}
fn parse_mode(bytes: &[u8], invitation: bool) -> Result<Outcome> {
    use portcullis_ipc::calendar::address;
    let mut method = None;
    if bytes.len() > FILE_LIMIT {
        return Err(Issue::Limit);
    }
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for line in bytes.split(|&c| c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b" ").or_else(|| line.strip_prefix(b"\t")) {
            let prev = lines.last_mut().ok_or(Issue::Format)?;
            if prev.len() + rest.len() > 8192 {
                return Err(Issue::Limit);
            }
            prev.extend_from_slice(rest);
        } else {
            if line.len() > 8192 || lines.len() >= 32768 {
                return Err(Issue::Limit);
            }
            lines.push(line.to_vec());
        }
    }
    let mut stack = Vec::new();
    let mut events = Vec::new();
    let mut event = Event::default();
    let (mut version, mut ended, mut omitted) = (false, false, false);
    let mut components = 0;
    for raw in lines {
        if raw.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(&raw).map_err(|_| Issue::Format)?;
        if ended || line.contains(['\0', '\r']) {
            return Err(Issue::Format);
        }
        let (head, v) = line.split_once(':').ok_or(Issue::Format)?;
        let mut props = head.split(';');
        let key = props.next().unwrap().to_ascii_uppercase();
        let params: Vec<_> = props.collect();
        match key.as_str() {
            "BEGIN" if params.is_empty() => {
                components += 1;
                if components > 1000 {
                    return Err(Issue::Limit);
                }
                let name = v.to_ascii_uppercase();
                match (stack.last().map(String::as_str), name.as_str()) {
                    (None, "VCALENDAR") => {}
                    (Some("VCALENDAR"), "VEVENT") => {
                        if events.len() >= ROW_LIMIT {
                            return Err(Issue::Limit);
                        }
                    }
                    (Some("VEVENT"), "VALARM") => omitted = true,
                    (_, "VTIMEZONE") => return Err(Issue::Zone),
                    _ => return Err(Issue::Component),
                }
                stack.push(name);
            }
            "END" if params.is_empty() => {
                let top = stack.pop().ok_or(Issue::Format)?;
                if !top.eq_ignore_ascii_case(v) {
                    return Err(Issue::Format);
                }
                if top == "VEVENT" {
                    events.push(std::mem::take(&mut event))
                }
                if top == "VCALENDAR" {
                    ended = true
                }
            }
            _ => match stack.last().map(String::as_str) {
                Some("VCALENDAR") => match key.as_str() {
                    "VERSION" if !version && v == "2.0" => version = true,
                    "CALSCALE" if v.eq_ignore_ascii_case("GREGORIAN") => {}
                    "PRODID" => {}
                    "METHOD"
                        if invitation
                            && params.is_empty()
                            && (v.eq_ignore_ascii_case("REQUEST")
                                || v.eq_ignore_ascii_case("CANCEL")) =>
                    {
                        once(&mut method, v.eq_ignore_ascii_case("CANCEL"))?;
                    }
                    "METHOD" if !invitation && v.eq_ignore_ascii_case("PUBLISH") => {}
                    "METHOD" => return Err(Issue::Scheduling),
                    "X-WR-TIMEZONE" => return Err(Issue::Zone),
                    k if k.starts_with("X-") => omitted = true,
                    _ => return Err(Issue::Format),
                },
                Some("VEVENT") if invitation => match key.as_str() {
                    "ORGANIZER" | "ATTENDEE" => {
                        let a = v
                            .get(..7)
                            .filter(|s| s.eq_ignore_ascii_case("mailto:"))
                            .and_then(|_| v.get(7..))
                            .filter(|a| address(a))
                            .ok_or(Issue::Scheduling)?
                            .to_ascii_lowercase();
                        if params.iter().any(|p| {
                            let name = p.split_once('=').map(|(k, _)| k.to_ascii_uppercase());
                            !matches!(
                                name.as_deref(),
                                Some("CN")
                                    | Some("PARTSTAT")
                                    | Some("RSVP")
                                    | Some("ROLE")
                                    | Some("CUTYPE")
                            )
                        }) {
                            return Err(Issue::Scheduling);
                        }
                        if key == "ORGANIZER" {
                            once(&mut event.organizer, a)?;
                        } else if event.attendees.len() < 256 {
                            event.attendees.push(a);
                        } else {
                            return Err(Issue::Limit);
                        }
                    }
                    "SEQUENCE" if params.is_empty() => once(
                        &mut event.sequence,
                        v.parse::<u32>().map_err(|_| Issue::Scheduling)?,
                    )?,
                    "LOCATION" if params.is_empty() => once(
                        &mut event.location,
                        if v.is_empty() {
                            String::new()
                        } else {
                            title(v)?
                        },
                    )?,
                    "STATUS"
                        if params.is_empty()
                            && (v.eq_ignore_ascii_case("CANCELLED")
                                || v.eq_ignore_ascii_case("CONFIRMED")) =>
                    {
                        once(
                            &mut event.cancelled_status,
                            v.eq_ignore_ascii_case("CANCELLED"),
                        )?
                    }
                    "RRULE" if params.is_empty() && !event.weekly => {
                        event.limit =
                            atrium_calendar::WeeklyLimit::parse(v).ok_or(Issue::Recurrence)?;
                        event.weekly = true;
                    }
                    "RDATE" | "EXRULE" => return Err(Issue::Recurrence),
                    "URL" | "ATTACH" | "LOCATION" | "SEQUENCE" | "STATUS" => {
                        return Err(Issue::Component)
                    }
                    _ => event.property(&key, &params, v, &mut omitted)?,
                },
                Some("VEVENT") => event.property(&key, &params, v, &mut omitted)?,
                Some("VALARM") => {}
                _ => return Err(Issue::Format),
            },
        }
    }
    if !ended || !version || events.is_empty() {
        return Err(Issue::Format);
    }
    if invitation {
        let cancelled = method.ok_or(Issue::Scheduling)?;
        let (e, children) = events.split_first().ok_or(Issue::Format)?;
        let group = cancelled && !children.is_empty();
        if children.len() > atrium_calendar::invitation::MAX_MOVES
            || (group
                && (events.len() > atrium_calendar::invitation::MAX_CANCELLED_DATES
                    || e.original.is_none()))
            || (!children.is_empty() && !cancelled && !e.weekly)
            || e.cancelled_status.is_some_and(|s| s != cancelled)
            || (cancelled && (e.limit.is_some() || !e.skipped.is_empty()))
        {
            return Err(Issue::Scheduling);
        }
        if e.original.is_some() && e.weekly {
            return Err(Issue::Recurrence);
        }
        let occurrence = |stamp: Stamp| -> Result<_> {
            Ok(atrium_calendar::invitation::OccurrenceId {
                date: stamp.date,
                minute: stamp.minute.ok_or(Issue::Recurrence)?,
                zone: stamp.zone,
            })
        };
        let original = e.original.map(occurrence).transpose()?;
        let mut cancelled_dates = Vec::new();
        if group {
            cancelled_dates.push(atrium_calendar::invitation::CancelledDate {
                original: original.ok_or(Issue::Recurrence)?,
                sequence: e.sequence.unwrap_or(0),
            });
        }
        let mut exceptions = Vec::new();
        let identities = |event: &Event| {
            event
                .attendees
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        };
        let invited = identities(e);
        for child in children {
            if child.weekly
                || !child.skipped.is_empty()
                || child.cancelled_status.is_some_and(|s| s != cancelled)
                || child.uid != e.uid
                || child.organizer != e.organizer
                || (!group && child.sequence.unwrap_or(0) != e.sequence.unwrap_or(0))
                || identities(child) != invited
            {
                return Err(Issue::Scheduling);
            }
            let original = occurrence(child.original.ok_or(Issue::Recurrence)?)?;
            if group {
                if cancelled_dates
                    .iter()
                    .any(|o| o.original.date == original.date)
                {
                    return Err(Issue::Recurrence);
                }
                cancelled_dates.push(atrium_calendar::invitation::CancelledDate {
                    original,
                    sequence: child.sequence.unwrap_or(0),
                });
                continue;
            }
            exceptions.push(atrium_calendar::invitation::IncludedMove {
                original,
                row: child.row(&[])?,
                location: child.location.clone().unwrap_or_default(),
            });
        }
        let mut skips = e
            .skipped
            .iter()
            .copied()
            .map(occurrence)
            .collect::<Result<Vec<_>>>()?;
        skips.sort_by_cached_key(|o| o.property());
        skips.dedup();
        if skips.len() + exceptions.len() > atrium_calendar::invitation::MAX_MOVES {
            return Err(Issue::Limit);
        }
        let row = if cancelled {
            None
        } else {
            let row = e.row(&[])?;
            for skip in &skips {
                skip.in_series(&row).map_err(|_| Issue::Recurrence)?;
            }
            Some(row)
        };
        return Ok(Outcome::Invitation(atrium_calendar::invitation::Proposal {
            uid: e.uid.clone().ok_or(Issue::Format)?,
            exceptions,
            skips,
            limit: e.limit,
            sequence: e.sequence.unwrap_or(0),
            organizer: e.organizer.clone().ok_or(Issue::Scheduling)?,
            attendees: e.attendees.clone(),
            location: e.location.clone().unwrap_or_default(),
            original: if group { None } else { original },
            cancelled_dates,
            weekly_cancellation: cancelled && e.weekly,
            row,
        }));
    }
    let mut masters = HashMap::new();
    let mut rows = Vec::new();
    for (i, e) in events.iter().enumerate() {
        let uid = e.uid.as_deref().ok_or(Issue::Format)?;
        if e.original.is_none() && masters.insert(uid, i).is_some() {
            return Err(Issue::Recurrence);
        }
        rows.push(e.row(&e.skipped)?);
    }
    let mut exceptions = HashSet::new();
    for (i, e) in events.iter().enumerate() {
        if let Some(d) = e.original {
            let parent = *masters
                .get(e.uid.as_deref().ok_or(Issue::Format)?)
                .ok_or(Issue::Recurrence)?;
            let starts = events[parent].start.ok_or(Issue::Format)?;
            let base = &mut rows[parent];
            if e.weekly
                || base.kind != Kind::Weekly
                || d.minute != starts.minute
                || d.zone != starts.zone
                || !base.scheduled_on(d.date)
                || !exceptions.insert((parent, d.date))
            {
                return Err(Issue::Recurrence);
            }
            if let Err(pos) = base.excluded.binary_search(&d.date) {
                base.excluded.insert(pos, d.date)
            }
            rows[i].kind = Kind::Occurrence {
                series: base.id,
                original: d.date,
            };
        }
    }
    rows.sort_by_key(|e| (e.date, e.span.start));
    Ok(Outcome::Ready(Candidates {
        rows,
        omitted_details: omitted,
    }))
}

fn handle(ask: Parse, file: Option<File>) -> std::result::Result<Parsed, String> {
    let mut bytes = Vec::new();
    file.ok_or("missing file")?
        .take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "read failed")?;
    let outcome = match parse_mode(&bytes, ask.invitation) {
        Ok(outcome) => outcome,
        Err(issue) => Outcome::Blocked(issue),
    };
    let schedule = serde_json::to_vec(&outcome).map_err(|_| "encoding failed")?;
    if schedule.len() > atrium_calendar::import::FRAME_LIMIT {
        return Err("Preview exceeds the result limit".into());
    }
    Ok(Parsed { schedule })
}
fn main() {
    // Cap this disposable process before it receives its first descriptor.
    // Works on stock FreeBSD images where jail-wide rctl is unavailable.
    let limits = [(libc::RLIMIT_AS, 128 << 20), (libc::RLIMIT_CPU, 8)];
    for (resource, limit) in limits {
        let cap = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        // SAFETY: setrlimit reads one initialized rlimit for this process.
        if unsafe { libc::setrlimit(resource, &cap) } != 0 {
            std::process::exit(70);
        }
    }
    atrium_worker::serve(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atrium_calendar::{import::Prepared, store::Store};
    const STUDIO: &str = include_str!("../../tests/fixtures/studio.ics");
    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn reviewed_mail_invitation_changes_only_after_confirmation() {
        use portcullis_ipc::calendar::Source;
        let file = include_str!("../../tests/fixtures/invitation.ics");
        let read = |s: &str| match parse_mode(s.as_bytes(), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!(),
        };
        let week = date("2026-12-01");
        let src = Source {
            delivery: None,
            account: 7,
            from: "mina@fixture.invalid".into(),
            identities: vec!["dev@fixture.invalid".into()],
            verified: true,
            reply_destination_matches: true,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let proposal = read(file);
        let review = s.review_invitation(0, &src, proposal.clone()).unwrap();
        assert!(review.previous.is_none());
        assert!(s.day(week).entries.is_empty());
        s.apply_invitation(0, review.token.unwrap()).unwrap();
        let changed = file
            .replace("SEQUENCE:1", "SEQUENCE:2")
            .replace("T140000Z", "T150000Z")
            .replace("T144500Z", "T154500Z")
            .replace("Studio A", "Studio B");
        let r = s.review_invitation(1, &src, read(&changed)).unwrap();
        assert_eq!(r.previous.unwrap().span.start, 840);
        assert_eq!(s.day(week).entries[0].span.start, 840);
        let original_token = r.token.unwrap();
        for source in [
            Source {
                from: "other@fixture.invalid".into(),
                ..src.clone()
            },
            Source {
                verified: false,
                ..src.clone()
            },
            Source {
                reply_destination_matches: false,
                ..src.clone()
            },
        ] {
            assert!(s
                .review_invitation(1, &source, read(&changed))
                .unwrap()
                .token
                .is_none());
        }
        assert!(s.apply_invitation(1, original_token).is_err());
        let r = s.review_invitation(1, &src, read(&changed)).unwrap();
        s.apply_invitation(1, r.token.unwrap()).unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(s.day(week).entries[0].span.start, 900);
        assert!(s
            .review_invitation(2, &src, proposal)
            .unwrap()
            .token
            .is_none());
        let same = s.review_invitation(2, &src, read(&changed)).unwrap();
        assert!(!same.can_save);
        assert!(s.apply_invitation(2, same.token.unwrap()).is_err());
        assert!(s
            .review_invitation(2, &src, read(&changed.replace("Studio B", "Studio C")))
            .unwrap()
            .token
            .is_none());
        assert!(s
            .review_invitation(
                2,
                &Source {
                    identities: vec!["other@fixture.invalid".into()],
                    ..src
                },
                read(file)
            )
            .is_err());
        for bad in [
            file.replace("METHOD:REQUEST", "METHOD:COUNTER"),
            file.replace("SUMMARY:", "RRULE:FREQ=MONTHLY\nSUMMARY:"),
            file.replace("END:VCALENDAR", "BEGIN:VEVENT\nEND:VEVENT\nEND:VCALENDAR"),
            file.replace("END:VEVENT", "URL:https://fixture.invalid\nEND:VEVENT"),
        ] {
            assert!(parse_mode(bad.as_bytes(), true).is_err());
        }
        assert!(parse(file.as_bytes()).is_err());
    }

    #[test]
    fn mail_series_scopes_survive_moves_cancellation_undo_and_restart() {
        use portcullis_ipc::calendar::Source;
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let moved = include_str!("../../tests/fixtures/move-one.ics");
        let changed = include_str!("../../tests/fixtures/move-series.ics");
        let cancel_one = include_str!("../../tests/fixtures/cancel-one.ics");
        let cancel_all = include_str!("../../tests/fixtures/cancel-series.ics");
        let read = |file: &str| match parse_mode(file.as_bytes(), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!(),
        };
        let src = Source {
            delivery: None,
            account: 7,
            from: "mina@fixture.invalid".into(),
            identities: vec!["dev@fixture.invalid".into()],
            verified: true,
            reply_destination_matches: true,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let review =
            |s: &mut Store, rev, source: &Source, p: atrium_calendar::invitation::Proposal| {
                let mut source = source.clone();
                source.delivery = Some(portcullis_ipc::calendar::Delivery {
                    scope: p.scope(),
                    uid: p.uid.clone(),
                    sequence: p.sequence,
                    organizer: p.organizer.clone(),
                    cancelled: p.row.is_none(),
                });
                s.review_invitation(rev, &source, p)
            };
        assert!(store.review_invitation(0, &src, read(weekly)).is_err());
        let apply = |s: &mut Store, rev, p| {
            let r = review(s, rev, &src, p).unwrap();
            assert!(r.can_save, "{}", r.notice);
            s.apply_invitation(rev, r.token.unwrap()).unwrap().1
        };
        let starts = |s: &Store, day: &str| {
            s.day(date(day))
                .entries
                .iter()
                .map(|r| r.span.start)
                .collect::<Vec<_>>()
        };
        assert!(review(&mut store, 0, &src, read(moved))
            .unwrap()
            .token
            .is_none());
        apply(&mut store, 0, read(weekly));
        assert!(review(
            &mut store,
            1,
            &src,
            read(&moved.replace("20261208T140000Z", "20261208T120000Z"))
        )
        .unwrap()
        .token
        .is_none());
        let r = review(&mut store, 1, &src, read(moved)).unwrap();
        assert_eq!(r.previous.unwrap().date, date("2026-12-08"));
        assert_eq!(starts(&store, "2026-12-08"), [840]);
        apply(&mut store, 1, read(moved));
        assert!(starts(&store, "2026-12-08").is_empty());
        assert_eq!(starts(&store, "2026-12-09"), [900]);
        assert_eq!(starts(&store, "2026-12-15"), [840]);
        apply(&mut store, 2, read(changed));
        assert_eq!(starts(&store, "2026-12-09"), [900]);
        assert_eq!(starts(&store, "2026-12-15"), [960]);
        // The regular time is now 16:00, but this moved occurrence retains
        // its original 14:00 identity. Neither widening nor retiming is valid.
        let retimed = read(&cancel_one.replace("20261208T140000Z", "20261208T160000Z"));
        assert!(review(&mut store, 3, &src, retimed)
            .unwrap()
            .token
            .is_none());
        let proposed = read(cancel_one);
        let widened = Source {
            delivery: Some(portcullis_ipc::calendar::Delivery {
                scope: None,
                uid: proposed.uid.clone(),
                sequence: proposed.sequence,
                organizer: proposed.organizer.clone(),
                cancelled: true,
            }),
            ..src.clone()
        };
        assert!(store.review_invitation(3, &widened, proposed).is_err());
        for impostor in [
            Source {
                from: "other@fixture.invalid".into(),
                ..src.clone()
            },
            Source {
                verified: false,
                ..src.clone()
            },
            Source {
                reply_destination_matches: false,
                ..src.clone()
            },
        ] {
            assert!(review(&mut store, 3, &impostor, read(cancel_all))
                .unwrap()
                .token
                .is_none());
        }
        assert!(review(
            &mut store,
            3,
            &Source {
                account: 8,
                ..src.clone()
            },
            read(cancel_all)
        )
        .is_err());
        let r = review(&mut store, 3, &src, read(cancel_one)).unwrap();
        assert_eq!(r.previous.unwrap().date, date("2026-12-09"));
        let undo = apply(&mut store, 3, read(cancel_one));
        assert!(starts(&store, "2026-12-09").is_empty());
        assert_eq!(starts(&store, "2026-12-15"), [960]);
        store.undo(undo).unwrap();
        assert_eq!(starts(&store, "2026-12-09"), [900]);
        apply(&mut store, 5, read(cancel_one));
        let undo = apply(&mut store, 6, read(cancel_all));
        assert!(starts(&store, "2026-12-15").is_empty());
        store.undo(undo).unwrap();
        assert_eq!(starts(&store, "2026-12-15"), [960]);
        apply(&mut store, 8, read(cancel_all));
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        assert!(review(&mut store, 9, &src, read(weekly))
            .unwrap()
            .token
            .is_none());
        assert!(starts(&store, "2026-12-22").is_empty());
        for invalid in [
            weekly.replace("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=0"),
            moved.replace("RECURRENCE-ID:", "RECURRENCE-ID;RANGE=THISANDFUTURE:"),
        ] {
            assert!(parse_mode(invalid.as_bytes(), true).is_err());
        }
    }

    #[test]
    fn finite_series_and_included_moves_share_one_review_and_undo() {
        use portcullis_ipc::calendar::{Delivery, Source};
        let bundle = include_str!("../../tests/fixtures/bounded-moves.ics");
        let until = include_str!("../../tests/fixtures/weekly-until.ics");
        let proposal = |text: &str| match parse_mode(text.as_bytes(), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!("expected invitation"),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let review = |s: &mut Store, rev, text: &str| {
            let p = proposal(text);
            let src = Source {
                account: 7,
                from: p.organizer.clone(),
                verified: true,
                identities: vec!["dev@fixture.invalid".into()],
                reply_destination_matches: true,
                delivery: Some(Delivery {
                    uid: p.uid.clone(),
                    sequence: p.sequence,
                    organizer: p.organizer.clone(),
                    scope: p.scope(),
                    cancelled: false,
                }),
            };
            s.review_invitation(rev, &src, p)
        };
        let starts = |s: &Store, day: &str| {
            s.day(date(day))
                .entries
                .iter()
                .map(|e| e.span.start)
                .collect::<Vec<_>>()
        };
        let first = review(&mut store, 0, bundle).unwrap();
        assert!(first.can_save, "{}", first.notice);
        assert!(starts(&store, "2026-12-01").is_empty());
        let receipt = store.apply_invitation(0, first.token.unwrap()).unwrap().1;
        for (day, want) in [
            ("2026-12-01", vec![840]),
            ("2026-12-08", vec![]),
            ("2026-12-09", vec![900]),
            ("2026-12-15", vec![]),
            ("2026-12-17", vec![660]),
            ("2026-12-22", vec![840]),
            ("2026-12-29", vec![]),
        ] {
            assert_eq!(starts(&store, day), want, "{day}");
        }
        assert!(!review(&mut store, 1, bundle).unwrap().can_save);
        let conflict = bundle.replace("T110000Z", "T111500Z");
        assert!(review(&mut store, 1, &conflict).unwrap().token.is_none());
        store.undo(receipt).unwrap();
        assert!(starts(&store, "2026-12-09").is_empty());
        assert!(starts(&store, "2026-12-01").is_empty());
        let first = review(&mut store, 2, bundle).unwrap();
        store.apply_invitation(2, first.token.unwrap()).unwrap();
        let shortened = until.replace("20261229T140000Z", "20261208T135959Z");
        assert!(review(&mut store, 3, &shortened).unwrap().token.is_none());
        let next = review(&mut store, 3, until).unwrap();
        store.apply_invitation(3, next.token.unwrap()).unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(starts(&store, "2026-12-29"), [840]);
        assert!(starts(&store, "2027-01-05").is_empty());
        assert_eq!(starts(&store, "2026-12-17"), [660]);
        let incompatible = bundle.replacen("UID:studio-planning", "UID:other", 1);
        assert!(parse_mode(incompatible.as_bytes(), true).is_err());
        for bad in [
            bundle.replace("COUNT=4", "COUNT=2"),
            bundle.replace("20261215T140000Z", "20261208T140000Z"),
        ] {
            // Invalid input must be rejected in the worker or by Fasti even on an empty calendar.
            let tmp = tempfile::tempdir().unwrap();
            let mut clean = Store::open(tmp.path()).unwrap();
            assert!(review(&mut clean, 0, &bad).is_err());
            assert!(starts(&clean, "2026-12-01").is_empty());
        }
    }

    #[test]
    fn grouped_cancellation_is_atomic_scoped_and_reversible() {
        use atrium_calendar::invitation::Proposal;
        use portcullis_ipc::calendar::{Delivery, Source};
        const BATCH: &str = include_str!("../../tests/fixtures/cancel-dates.ics");
        let parse_invite = |text: &str| match parse_mode(text.as_bytes(), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!("expected scheduling proposal"),
        };
        let source = |p: &Proposal| Source {
            account: 7,
            from: p.organizer.clone(),
            verified: true,
            identities: vec!["dev@fixture.invalid".into()],
            reply_destination_matches: true,
            delivery: Some(Delivery {
                uid: p.uid.clone(),
                sequence: p.sequence,
                organizer: p.organizer.clone(),
                scope: p.scope(),
                cancelled: p.row.is_none(),
            }),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let master = parse_invite(include_str!("../../tests/fixtures/bounded-moves.ics"));
        let seed = store
            .review_invitation(0, &source(&master), master)
            .unwrap();
        store.apply_invitation(0, seed.token.unwrap()).unwrap();
        let sample = |s: &Store| {
            [
                "2026-12-01",
                "2026-12-08",
                "2026-12-09",
                "2026-12-15",
                "2026-12-17",
                "2026-12-22",
                "2026-12-29",
            ]
            .map(|day| {
                s.day(date(day))
                    .entries
                    .into_iter()
                    .map(|e| (e.id, e.span.start, e.span.end))
                    .collect::<Vec<_>>()
            })
        };
        let baseline = sample(&store);
        assert_eq!(
            baseline
                .clone()
                .map(|v| v.iter().map(|t| t.1).collect::<Vec<_>>()),
            [
                vec![840],
                vec![],
                vec![900],
                vec![],
                vec![660],
                vec![840],
                vec![]
            ]
        );
        let p = parse_invite(BATCH);
        let src = source(&p);
        let mut wrong = p.clone();
        wrong.cancelled_dates[1].sequence += 1;
        assert!(store.review_invitation(1, &src, wrong).is_err());
        for bad in [
            BATCH.replace("SEQUENCE:4", "SEQUENCE:1"),
            BATCH.replace("20261222T140000Z", "20261229T140000Z"),
            BATCH.replace("20261222T140000Z", "20261222T160000Z"),
        ] {
            let bad = parse_invite(&bad);
            let review = store.review_invitation(1, &source(&bad), bad).unwrap();
            assert!(review.token.is_none());
            assert_eq!(sample(&store), baseline);
        }
        for bad in [
            BATCH.replace("20261222T140000Z", "20261208T140000Z"),
            BATCH.replace("RECURRENCE-ID:20261222T140000Z\n", ""),
            BATCH.replacen("mailto:mina", "mailto:other", 1),
            BATCH.replace("STATUS:CANCELLED", "STATUS:CONFIRMED"),
            BATCH.replace("METHOD:CANCEL", "METHOD:REQUEST"),
            BATCH.replace("RECURRENCE-ID:", "RECURRENCE-ID;RANGE=THISANDFUTURE:"),
        ] {
            assert!(parse_mode(bad.as_bytes(), true).is_err());
        }
        let unverified = Source {
            verified: false,
            ..src.clone()
        };
        assert!(store
            .review_invitation(1, &unverified, p.clone())
            .unwrap()
            .token
            .is_none());
        let preview = store.review_invitation(1, &src, p.clone()).unwrap();
        assert_eq!(
            preview
                .previous_dates
                .iter()
                .map(|r| r.as_ref().unwrap().date)
                .collect::<Vec<_>>(),
            [date("2026-12-09"), date("2026-12-22")]
        );
        assert_eq!(sample(&store), baseline);
        let undo = store.apply_invitation(1, preview.token.unwrap()).unwrap().1;
        let mut remaining = baseline.clone();
        remaining[2].clear();
        remaining[5].clear();
        assert_eq!(sample(&store), remaining);
        assert!(store
            .review_invitation(2, &src, p.clone())
            .unwrap()
            .token
            .is_none());
        store.undo(undo).unwrap();
        assert_eq!(sample(&store), baseline);
        assert!(store.undo(undo).is_err());
        let again = store.review_invitation(3, &src, p.clone()).unwrap();
        store.apply_invitation(3, again.token.unwrap()).unwrap();
        drop(store);
        let mut reopened = Store::open(dir.path()).unwrap();
        assert_eq!(sample(&reopened), remaining);
        assert!(reopened
            .review_invitation(4, &src, p)
            .unwrap()
            .token
            .is_none());
    }

    #[test]
    fn exdate_review_preserves_local_moves_until_explicit_save() {
        use atrium_calendar::{
            invitation::{Proposal, SkipDecision, SkipEffect},
            EditScope,
        };
        use portcullis_ipc::calendar::{Delivery, Source};
        const SKIPS: &str = include_str!("../../tests/fixtures/weekly-exdates.ics");
        const MASTER: &str = include_str!("../../tests/fixtures/bounded-moves.ics");
        let proposal = |text: &str| match parse_mode(text.as_bytes(), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!("expected scheduling proposal"),
        };
        let source = |p: &Proposal| Source {
            account: 7,
            from: p.organizer.clone(),
            verified: true,
            identities: vec!["dev@fixture.invalid".into()],
            reply_destination_matches: true,
            delivery: Some(Delivery {
                uid: p.uid.clone(),
                sequence: p.sequence,
                organizer: p.organizer.clone(),
                scope: p.scope(),
                cancelled: false,
            }),
        };
        let review = |s: &mut Store, rev, text: &str| {
            let p = proposal(text);
            s.review_invitation(rev, &source(&p), p)
        };
        let sample = |s: &Store| {
            [
                "2026-12-01",
                "2026-12-08",
                "2026-12-09",
                "2026-12-10",
                "2026-12-15",
                "2026-12-17",
                "2026-12-22",
                "2026-12-29",
            ]
            .map(|day| {
                s.day(date(day))
                    .entries
                    .into_iter()
                    .map(|e| (e.id, e.span.start, e.span.end))
                    .collect::<Vec<_>>()
            })
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let r = review(&mut store, 0, MASTER).unwrap();
        store.apply_invitation(0, r.token.unwrap()).unwrap();
        let day = date("2026-12-09");
        let moved = store.day(day).entries.pop().unwrap();
        let mut edit = moved.fields_on(day);
        edit.date = date("2026-12-10");
        edit.span = Span {
            start: 990,
            end: 1035,
        };
        store
            .change(1, moved.id, day, EditScope::ThisEvent, Some(edit))
            .unwrap();
        let baseline = sample(&store);
        let mut substituted = proposal(SKIPS);
        let mail = source(&substituted);
        substituted.skips.pop();
        assert!(store.review_invitation(2, &mail, substituted).is_err());
        let r = review(&mut store, 2, SKIPS).unwrap();
        assert_eq!(
            r.skips.iter().map(|s| s.effect).collect::<Vec<_>>(),
            [SkipEffect::ChooseLocalMove, SkipEffect::Skip]
        );
        assert_eq!(
            r.skips[0].current.as_ref().unwrap().date,
            date("2026-12-10")
        );
        let t = r.token.unwrap();
        assert!(store.apply_invitation(2, t).is_err());
        assert!(store
            .apply_reviewed_invitation(
                2,
                t,
                &[SkipDecision {
                    original: date("2026-12-17"),
                    keep: false
                }]
            )
            .is_err());
        assert_eq!(sample(&store), baseline);
        let original = date("2026-12-08");
        let (_, undo) = store
            .apply_reviewed_invitation(
                2,
                t,
                &[SkipDecision {
                    original,
                    keep: false,
                }],
            )
            .unwrap();
        let mut expected = baseline.clone();
        expected[3].clear();
        expected[6].clear();
        assert_eq!(sample(&store), expected);
        store.undo(undo).unwrap();
        assert_eq!(sample(&store), baseline);
        let t = review(&mut store, 4, SKIPS).unwrap().token.unwrap();
        store
            .apply_reviewed_invitation(
                4,
                t,
                &[SkipDecision {
                    original,
                    keep: true,
                }],
            )
            .unwrap();
        let mut kept = baseline.clone();
        kept[6].clear();
        assert_eq!(sample(&store), kept);
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        let repeat = review(&mut store, 5, SKIPS).unwrap();
        assert!(!repeat.can_save);
        assert_eq!(repeat.skips[0].effect, SkipEffect::KeepLocalMove);
        let restored = SKIPS
            .replace("SEQUENCE:2", "SEQUENCE:3")
            .replace("EXDATE:20261208T140000Z,20261222T140000Z\n", "");
        let r = review(&mut store, 5, &restored).unwrap();
        assert_eq!(r.skips.len(), 1);
        assert_eq!(r.skips[0].effect, SkipEffect::Restore);
        store.apply_invitation(5, r.token.unwrap()).unwrap();
        assert_eq!(sample(&store), baseline);
        // A local deletion belongs to the user even when a later EXDATE also names it.
        let day = date("2026-12-22");
        let master = store.day(day).entries[0].id;
        store
            .change(6, master, day, EditScope::ThisEvent, None)
            .unwrap();
        let r = review(&mut store, 7, &SKIPS.replace("SEQUENCE:2", "SEQUENCE:4")).unwrap();
        assert_eq!(r.skips[1].effect, SkipEffect::AlreadyAbsent);
        store
            .apply_reviewed_invitation(
                7,
                r.token.unwrap(),
                &[SkipDecision {
                    original,
                    keep: true,
                }],
            )
            .unwrap();
        let r = review(&mut store, 8, &restored.replace("SEQUENCE:3", "SEQUENCE:5")).unwrap();
        assert!(r.skips.is_empty());
        store.apply_invitation(8, r.token.unwrap()).unwrap();
        assert_eq!(sample(&store), kept);
        let clean_dir = tempfile::tempdir().unwrap();
        let mut clean = Store::open(clean_dir.path()).unwrap();
        let fresh = SKIPS.replace(
            "20261208T140000Z,20261222T140000Z",
            "20261201T140000Z\nEXDATE:20261201T140000Z,20261222T140000Z",
        );
        let r = review(&mut clean, 0, &fresh).unwrap();
        assert_eq!(r.skips.len(), 2);
        clean.apply_invitation(0, r.token.unwrap()).unwrap();
        assert!(clean.day(date("2026-12-01")).entries.is_empty());
        assert_eq!(clean.day(date("2026-12-08")).entries.len(), 1);
        assert!(clean.day(date("2026-12-22")).entries.is_empty());
        // A locally created move on a regular slot is protected even without a prior Mail override.
        let day = date("2026-12-08");
        let ordinary = clean.day(day).entries.pop().unwrap();
        let mut edit = ordinary.fields_on(day);
        edit.date = date("2026-12-10");
        clean
            .change(1, ordinary.id, day, EditScope::ThisEvent, Some(edit))
            .unwrap();
        let r = review(&mut clean, 2, &SKIPS.replace("SEQUENCE:2", "SEQUENCE:3")).unwrap();
        let conflict = r.skips.iter().find(|s| s.original.date == day).unwrap();
        assert_eq!(conflict.effect, SkipEffect::ChooseLocalMove);
        assert_eq!(conflict.current.as_ref().unwrap().date, date("2026-12-10"));
        clean
            .apply_reviewed_invitation(
                2,
                r.token.unwrap(),
                &[SkipDecision {
                    original: day,
                    keep: true,
                }],
            )
            .unwrap();
        assert_eq!(clean.day(date("2026-12-10")).entries.len(), 1);
        assert_eq!(clean.day(date("2026-12-15")).entries.len(), 1);
        let overlap = MASTER.replace(
            "RRULE:FREQ=WEEKLY;COUNT=4",
            "RRULE:FREQ=WEEKLY;COUNT=4\nEXDATE:20261208T140000Z",
        );
        assert!(review(&mut clean, 3, &overlap).is_err());
        let beyond = SKIPS.replace("20261222T140000Z", "20261229T140000Z");
        assert!(review(&mut clean, 3, &beyond).is_err());
        for bad in [
            SKIPS.replace("20261222T140000Z", "20261222T153000Z"),
            SKIPS.replace("METHOD:REQUEST", "METHOD:CANCEL"),
            SKIPS.replace("EXDATE:", "EXDATE;VALUE=DATE:"),
        ] {
            assert!(parse_mode(bad.as_bytes(), true).is_err());
        }
    }

    #[test]
    fn exdates_match_the_original_slot_across_midnight_and_daylight_saving() {
        use portcullis_ipc::calendar::{Delivery, Source};
        let invite = include_str!("../../tests/fixtures/weekly-exdates.ics")
            .replace("DTSTART:20261201T140000Z", "DTSTART;TZID=America/Los_Angeles:20261027T233000")
            .replace("DTEND:20261201T144500Z", "DTEND;TZID=America/Los_Angeles:20261027T235500")
            .replace("EXDATE:20261208T140000Z,20261222T140000Z",
                     "EXDATE:20261028T063000Z,20261104T073000Z\nEXDATE;TZID=America/Los_Angeles:20261103T233000");
        let Outcome::Invitation(p) = parse_mode(invite.as_bytes(), true).unwrap() else {
            panic!("expected a weekly invitation");
        };
        let scope = "weekly;COUNT=4|EXDATE:20261028T063000Z|EXDATE:20261104T073000Z|EXDATE;TZID=America/Los_Angeles:20261103T233000";
        let source = Source {
            account: 7,
            verified: true,
            identities: vec!["dev@fixture.invalid".into()],
            from: p.organizer.clone(),
            reply_destination_matches: true,
            delivery: Some(Delivery {
                uid: p.uid.clone(),
                organizer: p.organizer.clone(),
                sequence: p.sequence,
                cancelled: false,
                scope: Some(scope.into()),
            }),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let review = store.review_invitation(0, &source, p).unwrap();
        assert_eq!(
            review
                .skips
                .iter()
                .map(|s| s.original.date)
                .collect::<Vec<_>>(),
            [date("2026-10-27"), date("2026-11-03")]
        );
        store.apply_invitation(0, review.token.unwrap()).unwrap();
        store
            .set_viewing_zone(1, Some(chrono_tz::America::Los_Angeles))
            .unwrap();
        for (day, expected) in [
            ("2026-10-27", false),
            ("2026-11-03", false),
            ("2026-11-10", true),
            ("2026-11-17", true),
        ] {
            assert_eq!(!store.day(date(day)).entries.is_empty(), expected);
        }
        for wrong in ["20261104T063000Z", "20261104T073000"] {
            let bad = invite.replace("20261104T073000Z", wrong);
            assert!(parse_mode(bad.as_bytes(), true).is_err());
        }
    }

    #[test]
    fn weekly_limits_follow_the_source_clock_and_skip_missing_times() {
        use atrium_calendar::WeeklyLimit;
        let p = match parse_mode(include_bytes!("../../tests/fixtures/weekly.ics"), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!(),
        };
        let mut row = p.row.unwrap();
        row.date = date("2026-03-01");
        row.span = Span {
            start: 150,
            end: 165,
        };
        row.zone = Some(Zone {
            name: chrono_tz::America::New_York,
            start_fold: None,
            end_fold: None,
        });
        let last = |rule: &str| WeeklyLimit::parse(rule).unwrap().unwrap().last(&row);
        assert_eq!(last("COUNT=2;FREQ=WEEKLY").unwrap(), date("2026-03-15"));
        assert_eq!(
            last("FREQ=WEEKLY;UNTIL=20260315T062959Z").unwrap(),
            date("2026-03-01")
        );
        assert_eq!(
            last("FREQ=WEEKLY;UNTIL=20260315T063000Z").unwrap(),
            date("2026-03-15")
        );
        assert!(last("FREQ=WEEKLY;UNTIL=20260315T063000").is_err());
        assert!(WeeklyLimit::parse("FREQ=WEEKLY;COUNT=2;UNTIL=20260315T063000Z").is_none());
        assert!(WeeklyLimit::parse("FREQ=WEEKLY;COUNT=521").is_none());
    }

    #[test]
    fn synthetic_file_preview_cancel_confirm_undo_and_restart() {
        let candidates = parse(STUDIO.replace('\n', "\r\n").as_bytes()).unwrap();
        assert!(candidates.omitted_details);
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let hash = [87; 32];
        let prepare = |store: &mut Store, rev| match store
            .prepare_import(rev, hash, candidates.rows.clone())
            .unwrap()
        {
            Prepared::New(token) => token,
            _ => panic!("expected new preview"),
        };
        let token = prepare(&mut store, 0);
        assert!(store.day(date("2026-11-02")).entries.is_empty());
        assert!(!dir.path().join("calendar.json").exists());
        store.discard_import(token);
        assert!(store.save_import(0, token).is_err());
        let token = prepare(&mut store, 0);
        let (_, receipt) = store.save_import(0, token).unwrap();
        assert!(store.save_import(0, token).is_err());
        assert_eq!(store.day(date("2026-11-03")).entries[0].kind, Kind::AllDay);
        for d in ["2026-11-09", "2026-11-16"] {
            assert!(store.day(date(d)).entries.is_empty());
        }
        let moved = &store.day(date("2026-11-10")).entries[0];
        assert_eq!(
            (moved.title.as_str(), moved.span.start),
            ("Moved studio review", 16 * 60)
        );
        assert_eq!(store.day(date("2026-11-23")).entries[0].span.start, 14 * 60);
        assert!(matches!(
            store.prepare_import(1, hash, candidates.rows.clone()),
            Ok(Prepared::Existing(_))
        ));
        store.undo(receipt).unwrap();
        assert!(store.day(date("2026-11-10")).entries.is_empty());
        let stale = prepare(&mut store, 2);
        store
            .set_opening_view(2, atrium_calendar::CalendarView::Week)
            .unwrap();
        assert!(store.save_import(2, stale).is_err());
        let token = prepare(&mut store, 3);
        store.save_import(3, token).unwrap();
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        assert!(matches!(
            store.prepare_import(4, hash, candidates.rows),
            Ok(Prepared::Existing(_))
        ));
        assert_eq!(store.day(date("2026-11-10")).entries.len(), 1);
    }

    #[test]
    fn unsupported_or_incomplete_files_never_produce_a_partial_batch() {
        assert_eq!(
            parse(include_bytes!("../../tests/fixtures/unsupported.ics")).unwrap_err(),
            Issue::Recurrence
        );
        for (from, to, issue) in [
            ("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=3", Issue::Recurrence),
            ("America/New_York", "Custom/Office", Issue::Zone),
            (
                "VERSION:2.0",
                "VERSION:2.0\nMETHOD:CANCEL",
                Issue::Scheduling,
            ),
            ("END:VCALENDAR", "", Issue::Format),
            (
                "DTEND;VALUE=DATE:20261104",
                "DTEND;VALUE=DATE:20261105",
                Issue::Time,
            ),
            ("20261109T090000", "20261109T080000", Issue::Recurrence),
            ("20261102T090000", "20261101T013000", Issue::Time),
        ] {
            assert_eq!(
                parse(STUDIO.replace(from, to).as_bytes()).unwrap_err(),
                issue
            );
        }
        assert_eq!(
            parse(&vec![b'x'; FILE_LIMIT + 1]).unwrap_err(),
            Issue::Limit
        );
        assert_eq!(
            parse(
                STUDIO
                    .replace("Studio review", &"a".repeat(8193))
                    .as_bytes()
            )
            .unwrap_err(),
            Issue::Limit
        );
    }

    #[test]
    fn floating_utc_folded_text_and_date_only_duration() {
        let make = |start, end, title| {
            format!("BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:a\nDTSTART{start}\n{end}SUMMARY:{title}\nEND:VEVENT\nEND:VCALENDAR")
        };
        for (start, end, zone) in [
            (":20261102T140000", "DTEND:20261102T144500\n", None),
            (
                ":20261102T140000Z",
                "DTEND:20261102T144500Z\n",
                Some(chrono_tz::UTC),
            ),
            (";VALUE=DATE:20261102", "", None),
        ] {
            let file = make(start, end, "Board\\, re\n view\u{202e}");
            let row = &parse(file.as_bytes()).unwrap().rows[0];
            assert_eq!(row.title, "Board, review");
            assert_eq!(row.zone.map(|z| z.name), zone);
        }
        let ambiguous = make(
            ";TZID=America/New_York:20261101T013000",
            "DTEND;TZID=America/New_York:20261101T014500\n",
            "Fold",
        );
        assert_eq!(parse(ambiguous.as_bytes()).unwrap_err(), Issue::Zone);
    }
}
