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
    this_and_future: bool,
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
                    "RECURRENCE-ID" => {
                        let mut clock_params = Vec::new();
                        for param in &params {
                            if param.eq_ignore_ascii_case("RANGE=THISANDFUTURE")
                                && !event.this_and_future
                            {
                                event.this_and_future = true;
                            } else {
                                clock_params.push(*param);
                            }
                        }
                        once(&mut event.original, stamp(&clock_params, v)?)?;
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
        if e.original.is_some() && e.weekly || e.this_and_future && !children.is_empty() {
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
            if child.this_and_future
                || child.weekly
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
            this_and_future: e.this_and_future,
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
            moved.replace("RECURRENCE-ID:", "RECURRENCE-ID;RANGE=THISANDPRIOR:"),
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

    const RANGE_MASTER: &str = include_str!("../../tests/fixtures/bounded-moves.ics");
    const RANGE_REQUEST: &str = include_str!("../../tests/fixtures/future.ics");

    fn range_proposal(text: &str) -> atrium_calendar::invitation::Proposal {
        match parse_mode(text.as_bytes(), true).unwrap() {
            Outcome::Invitation(p) => p,
            _ => panic!("expected an invitation"),
        }
    }

    fn range_source(p: &atrium_calendar::invitation::Proposal) -> portcullis_ipc::calendar::Source {
        use portcullis_ipc::calendar::{Delivery, Source};
        Source {
            account: 8,
            from: p.organizer.clone(),
            verified: true,
            identities: vec!["dev@fixture.invalid".into()],
            reply_destination_matches: true,
            delivery: Some(Delivery {
                uid: p.uid.clone(),
                sequence: p.sequence,
                organizer: p.organizer.clone(),
                cancelled: p.row.is_none(),
                scope: p.scope(),
            }),
        }
    }

    fn review_range(s: &mut Store, rev: u64, text: &str) -> atrium_calendar::invitation::Review {
        let p = range_proposal(text);
        s.review_invitation(rev, &range_source(&p), p).unwrap()
    }
    #[test]
    fn reviewed_future_range_respects_exceptions_revisions_and_undo() {
        use portcullis_ipc::calendar::Source;
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let t = review_range(&mut store, 0, RANGE_MASTER).token.unwrap();
        store.apply_invitation(0, t).unwrap();
        let snapshot = |s: &Store| {
            (1..=31)
                .map(|d| {
                    s.day(NaiveDate::from_ymd_opt(2026, 12, d).unwrap())
                        .entries
                        .into_iter()
                        .map(|e| (e.id, e.span))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let old = snapshot(&store);
        let p = range_proposal(RANGE_REQUEST);
        let mut forged = range_source(&p);
        forged.delivery.as_mut().unwrap().scope = Some(p.original.unwrap().property());
        assert!(store.review_invitation(1, &forged, p.clone()).is_err());
        let unverified = Source {
            verified: false,
            ..range_source(&p)
        };
        assert!(store
            .review_invitation(1, &unverified, p)
            .unwrap()
            .token
            .is_none());
        let r = review_range(&mut store, 1, RANGE_REQUEST);
        assert_eq!(
            r.range_dates
                .iter()
                .map(|r| (r.original, r.after.is_some()))
                .collect::<Vec<_>>(),
            [
                (date("2026-12-08"), true),
                (date("2026-12-15"), false),
                (date("2026-12-22"), true)
            ]
        );
        assert_eq!(snapshot(&store), old);
        let receipt = store.apply_invitation(1, r.token.unwrap()).unwrap().1;
        let saved = snapshot(&store);
        assert_eq!(old[0], saved[0]);
        assert_eq!(old[16], saved[16]);
        assert_eq!(
            saved[8][0].1,
            Span {
                start: 960,
                end: 1020
            }
        );
        assert!(saved[21].is_empty());
        assert_eq!(saved[22][0].1, saved[8][0].1);
        assert_eq!(saved.iter().map(Vec::len).sum::<usize>(), 4);
        assert!(!review_range(&mut store, 2, RANGE_REQUEST).can_save);
        let different = RANGE_REQUEST.replace("T170000Z", "T173000Z");
        assert!(review_range(&mut store, 2, &different).token.is_none());
        let single = RANGE_REQUEST.replace(";RANGE=THISANDFUTURE", "");
        assert!(review_range(&mut store, 2, &single).token.is_none());
        store.undo(receipt).unwrap();
        assert_eq!(snapshot(&store), old);
        let t = review_range(&mut store, 3, RANGE_REQUEST).token.unwrap();
        store.apply_invitation(3, t).unwrap();
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        let later = different.replace("SEQUENCE:2", "SEQUENCE:3");
        let t = review_range(&mut store, 4, &later).token.unwrap();
        store.apply_invitation(4, t).unwrap();
        assert_eq!(snapshot(&store)[22][0].1.end, 1050);
        assert!(review_range(&mut store, 5, RANGE_REQUEST).token.is_none());
        assert_eq!(snapshot(&store)[16], old[16]);

        // A separately excluded date stays absent, and a local edit at the
        // organizer's starting slot blocks the range as a whole.
        let dir = tempfile::tempdir().unwrap();
        let mut separate = Store::open(dir.path()).unwrap();
        let excluded = RANGE_MASTER.replace("COUNT=4", "COUNT=4\nEXDATE:20261222T140000Z");
        let t = review_range(&mut separate, 0, &excluded).token.unwrap();
        separate.apply_invitation(0, t).unwrap();
        let r = review_range(&mut separate, 1, RANGE_REQUEST);
        assert!(r.range_dates[2].before.is_none() && r.range_dates[2].after.is_none());
        let undo = separate.apply_invitation(1, r.token.unwrap()).unwrap().1;
        assert!(separate.day(date("2026-12-23")).entries.is_empty());
        separate.undo(undo).unwrap();
        let current = date("2026-12-09");
        let moved = separate.day(current).entries.pop().unwrap();
        let mut local = moved.fields_on(current);
        local.date = date("2026-12-10");
        separate
            .change(
                3,
                moved.id,
                current,
                atrium_calendar::EditScope::ThisEvent,
                Some(local),
            )
            .unwrap();
        let saved = snapshot(&separate);
        let blocked = review_range(&mut separate, 4, RANGE_REQUEST);
        assert!(blocked.token.is_none() && blocked.notice.contains("locally"));
        assert_eq!(snapshot(&separate), saved);
        // A spring clock change falls between the original slot and its replacement.
        let master = include_str!("../../tests/fixtures/weekly.ics")
            .replace("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=3")
            .replace(
                "DTSTART:20261201T140000Z",
                "DTSTART;TZID=America/New_York:20260307T090000",
            )
            .replace(
                "DTEND:20261201T144500Z",
                "DTEND;TZID=America/New_York:20260307T094500",
            );
        let range = RANGE_REQUEST
            .replace(
                "THISANDFUTURE:20261208T140000Z",
                "THISANDFUTURE;TZID=America/New_York:20260307T090000",
            )
            .replace(
                "DTSTART:20261209T160000Z",
                "DTSTART;TZID=America/New_York:20260308T090000",
            )
            .replace(
                "DTEND:20261209T170000Z",
                "DTEND;TZID=America/New_York:20260308T100000",
            );
        let dst = tempfile::tempdir().unwrap();
        let mut dst_store = Store::open(dst.path()).unwrap();
        let t = review_range(&mut dst_store, 0, &master).token.unwrap();
        dst_store.apply_invitation(0, t).unwrap();
        let r = review_range(&mut dst_store, 1, &range);
        let later = r.range_dates[1].after.as_ref().unwrap();
        assert_eq!(
            (later.date, later.span),
            (
                date("2026-03-15"),
                Span {
                    start: 480,
                    end: 540
                }
            )
        );
        let t = r.token.unwrap();
        dst_store.apply_invitation(1, t).unwrap();
        assert!(parse_mode(
            RANGE_REQUEST
                .replace("METHOD:REQUEST", "METHOD:CANCEL")
                .as_bytes(),
            true
        )
        .is_ok());
        assert!(parse_mode(
            RANGE_REQUEST
                .replace(
                    "RANGE=THISANDFUTURE",
                    "RANGE=THISANDFUTURE;RANGE=THISANDFUTURE"
                )
                .as_bytes(),
            true
        )
        .is_err());
    }

    const CANCEL_FUTURE: &str = include_str!("../../tests/fixtures/cancel-future.ics");

    const REINSTATE: &str = include_str!("../../tests/fixtures/reinstate-future.ics");

    const RESTART: &str = include_str!("../../tests/fixtures/restart-future.ics");

    const RESTORE_ONE: &str = include_str!("../../tests/fixtures/restore-one.ics");

    fn seed_cancelled_interval(s: &mut Store) {
        use atrium_calendar::{invitation::SkipDecision, EditScope};
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let t = review_range(s, 0, weekly).token.unwrap();
        s.apply_invitation(0, t).unwrap();
        for (rev, original) in [(1, "2026-12-15"), (2, "2026-12-22")] {
            let day = date(original);
            let row = s.day(day).entries.pop().unwrap();
            let mut edit = row.fields_on(day);
            edit.date = date("2026-12-24");
            edit.title = format!("Private copy of {original}");
            s.change(rev, row.id, day, EditScope::ThisEvent, Some(edit))
                .unwrap();
        }
        let cut = CANCEL_FUTURE.replace("20261215", "20261208");
        let t = review_range(s, 3, &cut).token.unwrap();
        let choices = ["2026-12-15", "2026-12-22"].map(|d| SkipDecision {
            original: date(d),
            keep: true,
        });
        s.apply_reviewed_invitation(3, t, &choices).unwrap();
        let restart = RESTART
            .replace("20261222", "20261229")
            .replace("20261223", "20261230");
        let t = review_range(s, 4, &restart).token.unwrap();
        s.apply_invitation(4, t).unwrap();
    }

    #[test]
    fn reissue_must_follow_the_revision_of_a_range_covering_the_cancelled_slot() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let cancel = include_str!("../../tests/fixtures/cancel-restored.ics");
        let range = RANGE_REQUEST.replace("SEQUENCE:2", "SEQUENCE:7");
        for (rev, text) in [
            (0, include_str!("../../tests/fixtures/weekly.ics")),
            (1, cancel),
            (2, range.as_str()),
        ] {
            let token = review_range(&mut store, rev, text).token.unwrap();
            store.apply_invitation(rev, token).unwrap();
        }
        let reissue = include_str!("../../tests/fixtures/reissue-one.ics");
        let blocked = review_range(&mut store, 3, reissue);
        assert!(blocked.token.is_none() && blocked.notice.contains("supersedes"));
        assert!(store.day(date("2026-12-16")).entries.is_empty());
        let newer = reissue.replace("SEQUENCE:6", "SEQUENCE:8");
        let review = review_range(&mut store, 3, &newer);
        assert!(review.reinstating);
        store.apply_invitation(3, review.token.unwrap()).unwrap();
        assert_eq!(store.day(date("2026-12-18")).entries[0].span.start, 690);
        assert_eq!(store.day(date("2026-12-23")).entries[0].span.start, 960);
    }

    #[test]
    fn cancellation_and_reissue_of_a_restored_slot_leave_private_copies_independent() {
        use atrium_calendar::{invitation::SkipDecision, EditScope};
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        seed_cancelled_interval(&mut s);
        let original = date("2026-12-15");
        let t = review_range(&mut s, 5, RESTORE_ONE).token.unwrap();
        s.apply_reviewed_invitation(
            5,
            t,
            &[SkipDecision {
                original,
                keep: true,
            }],
        )
        .unwrap();
        let old_day = date("2026-12-17");
        let old_id = s.day(old_day).entries[0].id;
        let private_day = date("2026-12-24");
        let private = s
            .day(private_day)
            .entries
            .into_iter()
            .find(|r| r.title.ends_with("12-15"))
            .unwrap();
        let mut moved = private.fields_on(private_day);
        // Even an exact collision with the new organizer time is an independent appointment.
        moved.date = date("2026-12-18");
        moved.span = Span {
            start: 690,
            end: 735,
        };
        moved.title = "Independent appointment".into();
        s.change(
            6,
            private.id,
            private_day,
            EditScope::ThisEvent,
            Some(moved.clone()),
        )
        .unwrap();
        let cancel = include_str!("../../tests/fixtures/cancel-restored.ics");
        let reissue = include_str!("../../tests/fixtures/reissue-one.ics");
        let snapshot = || {
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(dir.path().join("calendar.json")).unwrap(),
            )
            .unwrap()
        };
        let with_revision = |value: &serde_json::Value, rev: u64| {
            let mut v = value.clone();
            v["revision"] = rev.into();
            v
        };
        let before = snapshot();
        let p = range_proposal(cancel);
        let mut source = range_source(&p);
        source.verified = false;
        assert!(s.review_invitation(7, &source, p).unwrap().token.is_none());
        assert!(
            review_range(&mut s, 7, &cancel.replace("SEQUENCE:5", "SEQUENCE:4"))
                .token
                .is_none()
        );
        let r = review_range(&mut s, 7, cancel);
        assert_eq!(r.previous.as_ref().unwrap().id, old_id);
        assert_eq!(r.previous.as_ref().unwrap().date, old_day);
        assert_eq!(snapshot(), before);
        let (_, undo) = s.apply_invitation(7, r.token.unwrap()).unwrap();
        assert!(s.day(old_day).entries.is_empty());
        assert_eq!(s.day(moved.date).entries[0].fields_on(moved.date), moved);
        s.undo(undo).unwrap();
        assert_eq!(snapshot(), with_revision(&before, 9));
        let t = review_range(&mut s, 9, cancel).token.unwrap();
        s.apply_invitation(9, t).unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        let cancelled = snapshot();
        assert!(review_range(&mut s, 10, cancel).token.is_none());
        for stale in [
            RESTORE_ONE.to_owned(),
            reissue.replace("SEQUENCE:6", "SEQUENCE:5"),
        ] {
            let r = review_range(&mut s, 10, &stale);
            assert!(r.token.is_none() && r.previous.is_none());
        }
        let widened = reissue.replace("RECURRENCE-ID:", "RECURRENCE-ID;RANGE=THISANDFUTURE:");
        assert!(review_range(&mut s, 10, &widened).token.is_none());
        let master =
            include_str!("../../tests/fixtures/weekly.ics").replace("SEQUENCE:1", "SEQUENCE:6");
        let (_, event) = reissue.split_once("BEGIN:VEVENT").unwrap();
        let bundle = master.replace("END:VCALENDAR", &format!("BEGIN:VEVENT{event}"));
        assert!(review_range(&mut s, 10, &bundle).token.is_none());
        let proposal = range_proposal(reissue);
        let mut unverified = range_source(&proposal);
        unverified.verified = false;
        assert!(s
            .review_invitation(10, &unverified, proposal.clone())
            .unwrap()
            .token
            .is_none());
        let mut substituted = range_source(&proposal);
        substituted.delivery.as_mut().unwrap().scope = range_proposal(&widened).scope();
        assert!(s.review_invitation(10, &substituted, proposal).is_err());
        let equivalent = reissue.replace(
            "RECURRENCE-ID:20261215T140000Z",
            "RECURRENCE-ID;TZID=Pacific/Kiritimati:20261216T040000",
        );
        for (rev, request) in [(10, equivalent.as_str()), (12, reissue)] {
            let r = review_range(&mut s, rev, request);
            assert!(r.reinstating && r.can_save && r.previous.is_none());
            assert_eq!(r.range_dates.len(), 1);
            let d = &r.range_dates[0];
            assert_eq!(d.original, original);
            assert!(d.private_copy.is_none() && d.before.is_none());
            assert_eq!(d.after.as_ref().unwrap().date, moved.date);
            let t = r.token.unwrap();
            assert!(s
                .apply_reviewed_invitation(
                    rev,
                    t,
                    &[SkipDecision {
                        original,
                        keep: false
                    }]
                )
                .is_err());
            assert_eq!(snapshot(), with_revision(&cancelled, rev));
            let (_, undo) = s.apply_invitation(rev, t).unwrap();
            let day = s.day(moved.date).entries;
            assert_eq!(day.len(), 2);
            assert!(day
                .iter()
                .any(|e| e.id == private.id && e.fields_on(moved.date) == moved));
            let returned = day.iter().find(|e| e.invitation.is_some()).unwrap();
            assert_eq!(returned.invitation.as_ref().unwrap().sequence, 6);
            for day in ["2026-12-08", "2026-12-15", "2026-12-17", "2026-12-22"] {
                assert!(s.day(date(day)).entries.is_empty());
            }
            assert_eq!(s.day(date("2026-12-30")).entries[0].span.start, 960);
            assert!(!review_range(&mut s, rev + 1, request).can_save);
            assert!(review_range(&mut s, rev + 1, cancel).token.is_none());
            if rev == 10 {
                s.undo(undo).unwrap();
                assert_eq!(snapshot(), with_revision(&cancelled, 12));
            }
        }
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        let returned = s
            .day(moved.date)
            .entries
            .into_iter()
            .find(|r| r.invitation.is_some())
            .unwrap();
        s.change(13, returned.id, moved.date, EditScope::ThisEvent, None)
            .unwrap();
        let blocked = review_range(&mut s, 14, &reissue.replace("SEQUENCE:6", "SEQUENCE:7"));
        assert!(blocked.token.is_none() && blocked.notice.contains("removed locally"));
    }

    #[test]
    fn one_verified_request_splits_the_interval_and_reviews_only_its_copy() {
        use atrium_calendar::invitation::SkipDecision;
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        seed_cancelled_interval(&mut s);
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        let p = range_proposal(RESTORE_ONE);
        let mut source = range_source(&p);
        source.verified = false;
        assert!(s
            .review_invitation(5, &source, p.clone())
            .unwrap()
            .token
            .is_none());
        source = range_source(&p);
        source.delivery.as_mut().unwrap().scope = range_proposal(REINSTATE).scope();
        assert!(s.review_invitation(5, &source, p).is_err());
        for bad in [
            RESTORE_ONE.replace("SEQUENCE:4", "SEQUENCE:3"),
            RESTORE_ONE.replace("20261215T140000Z", "20261215T130000Z"),
            RESTORE_ONE.replace("RECURRENCE-ID:", "RECURRENCE-ID;RANGE=THISANDFUTURE:"),
        ] {
            assert!(review_range(&mut s, 5, &bad).token.is_none());
        }
        let bundle = RANGE_MASTER.replace("SEQUENCE:1", "SEQUENCE:4");
        let blocked = review_range(&mut s, 5, &bundle);
        assert!(blocked.token.is_none() && blocked.notice.contains("cancelled interval"));
        let snapshot = || std::fs::read(dir.path().join("calendar.json")).unwrap();
        let baseline = snapshot();
        let copies_on = date("2026-12-24");
        let first = s.day(date("2026-12-01")).entries[0].id;
        let resumed = s.day(date("2026-12-30")).entries[0].id;
        let original = date("2026-12-15");
        let sibling = date("2026-12-22");
        let zoned = RESTORE_ONE.replace(
            "RECURRENCE-ID:20261215T140000Z",
            "RECURRENCE-ID;TZID=Pacific/Kiritimati:20261216T040000",
        );
        for (rev, keep, request) in [(5, true, RESTORE_ONE), (7, false, zoned.as_str())] {
            let review = review_range(&mut s, rev, request);
            assert!(review.reinstating && review.can_save);
            assert_eq!(review.range_dates.len(), 1);
            let detail = &review.range_dates[0];
            assert_eq!(detail.original, original);
            assert_eq!(
                detail.private_copy.as_ref().unwrap().title,
                "Private copy of 2026-12-15"
            );
            assert_eq!(detail.after.as_ref().unwrap().date, date("2026-12-17"));
            let token = review.token.unwrap();
            assert!(s.apply_invitation(rev, token).is_err());
            assert!(s
                .apply_reviewed_invitation(
                    rev,
                    token,
                    &[SkipDecision {
                        original: sibling,
                        keep: false
                    }]
                )
                .is_err());
            assert_eq!(s.day(copies_on).entries.len(), 2);
            let (_, undo) = s
                .apply_reviewed_invitation(rev, token, &[SkipDecision { original, keep }])
                .unwrap();
            assert_eq!(s.day(copies_on).entries.len(), if keep { 2 } else { 1 });
            assert!(s
                .day(copies_on)
                .entries
                .iter()
                .any(|r| r.title == "Private copy of 2026-12-22"));
            assert_eq!(s.day(date("2026-12-17")).entries[0].span.start, 600);
            for absent in [date("2026-12-08"), original, sibling] {
                assert!(s.day(absent).entries.is_empty());
            }
            assert_eq!(s.day(date("2026-12-01")).entries[0].id, first);
            assert_eq!(s.day(date("2026-12-30")).entries[0].id, resumed);
            let replay = review_range(&mut s, rev + 1, request);
            assert!(!replay.can_save && replay.notice.contains("Already saved"));
            let next = RESTORE_ONE.replace("20261215T140000", "20261222T140000");
            let preview = review_range(&mut s, rev + 1, &next);
            assert_eq!(
                preview.range_dates[0].private_copy.as_ref().unwrap().title,
                "Private copy of 2026-12-22"
            );
            if keep {
                s.undo(undo).unwrap();
                let mut restored: serde_json::Value = serde_json::from_slice(&snapshot()).unwrap();
                restored["revision"] = 5.into();
                assert_eq!(
                    restored,
                    serde_json::from_slice::<serde_json::Value>(&baseline).unwrap()
                );
            }
        }
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.day(date("2026-12-17")).entries.len(), 1);
        assert!(s.day(date("2026-12-08")).entries.is_empty() && s.day(sibling).entries.is_empty());
    }

    #[test]
    fn restoring_a_date_leaves_unlinked_or_deleted_private_copies_alone() {
        use atrium_calendar::EditScope;
        for legacy in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut s = Store::open(dir.path()).unwrap();
            seed_cancelled_interval(&mut s);
            let private_day = date("2026-12-24");
            let mut replacement = s
                .day(private_day)
                .entries
                .into_iter()
                .find(|e| e.title.ends_with("12-15"))
                .unwrap();
            let rev = if legacy {
                drop(s);
                let path = dir.path().join("calendar.json");
                let mut json: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                json["version"] = 12.into();
                let master = json["entries"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|r| r["kind"] == "Weekly")
                    .unwrap();
                let gap = master["invitation"]["cancelled_intervals"][0]
                    .as_object_mut()
                    .unwrap();
                gap.remove("copies");
                gap.remove("sequence");
                std::fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
                s = Store::open(dir.path()).unwrap();
                5
            } else {
                s.change(5, replacement.id, private_day, EditScope::ThisEvent, None)
                    .unwrap();
                replacement.title = "Independent appointment".into();
                replacement.source = None;
                s.add(6, replacement.clone()).unwrap();
                7
            };
            let preview = review_range(&mut s, rev, RESTORE_ONE);
            assert!(preview.range_dates[0].private_copy.is_none());
            assert_eq!(preview.notice.contains("Older calendar"), legacy);
            s.apply_invitation(rev, preview.token.unwrap()).unwrap();
            assert!(s
                .day(private_day)
                .entries
                .iter()
                .any(|r| r.id == replacement.id && r.title == replacement.title));
        }
    }

    #[test]
    fn partial_restart_uses_original_dates_for_both_private_copies() {
        use atrium_calendar::{invitation::SkipDecision, EditScope};
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics").replace(
            "FREQ=WEEKLY",
            "FREQ=WEEKLY;COUNT=6\nEXDATE:20261229T140000Z",
        );
        let t = review_range(&mut s, 0, &weekly).token.unwrap();
        s.apply_invitation(0, t).unwrap();
        let cut = date("2026-12-15");
        let restart = date("2026-12-22");
        let stay = date("2026-12-24");
        let choice_day = date("2026-12-11");
        for (rev, from, to) in [(1, cut, stay), (2, restart, choice_day)] {
            let row = s.day(from).entries.pop().unwrap();
            let mut edit = row.fields_on(from);
            edit.date = to;
            s.change(rev, row.id, from, EditScope::ThisEvent, Some(edit))
                .unwrap();
        }
        let earlier = s.day(date("2026-12-08")).entries.pop().unwrap();
        let t = review_range(&mut s, 3, CANCEL_FUTURE).token.unwrap();
        let choices = [
            SkipDecision {
                original: cut,
                keep: true,
            },
            SkipDecision {
                original: restart,
                keep: true,
            },
        ];
        s.apply_reviewed_invitation(3, t, &choices).unwrap();
        let snapshot = || std::fs::read(dir.path().join("calendar.json")).unwrap();
        let before = snapshot();
        let p = range_proposal(RESTART);
        let mut wrong = range_source(&p);
        wrong.delivery.as_mut().unwrap().scope = range_proposal(REINSTATE).scope();
        assert!(s.review_invitation(4, &wrong, p.clone()).is_err());
        let mut unverified = range_source(&p);
        unverified.verified = false;
        assert!(s
            .review_invitation(4, &unverified, p)
            .unwrap()
            .token
            .is_none());
        assert!(
            review_range(&mut s, 4, &RESTART.replace("SEQUENCE:3", "SEQUENCE:2"))
                .token
                .is_none()
        );
        let after_end = RESTART.replace("20261222T140000", "20270112T140000");
        assert!(review_range(&mut s, 4, &after_end).token.is_none());
        let inspect = |s: &mut Store, rev| {
            let r = review_range(s, rev, RESTART);
            let gap = r.cancelled_before_restart.unwrap();
            assert_eq!((gap.first, gap.restart), (cut, restart));
            assert_eq!(
                r.range_dates
                    .iter()
                    .map(|d| (
                        d.original,
                        d.after.as_ref().map(|e| e.date),
                        d.private_copy.as_ref().map(|e| e.date)
                    ))
                    .collect::<Vec<_>>(),
                [
                    (cut, None, Some(stay)),
                    (restart, Some(date("2026-12-23")), Some(choice_day)),
                    (date("2026-12-29"), None, None),
                    (date("2027-01-05"), Some(date("2027-01-06")), None)
                ]
            );
            r.token.unwrap()
        };
        let t = inspect(&mut s, 4);
        assert!(s.apply_invitation(4, t).is_err());
        assert!(s.apply_reviewed_invitation(4, t, &choices).is_err());
        assert!(s.apply_reviewed_invitation(4, t, &choices[..1]).is_err());
        assert_eq!(before, snapshot());
        let (_, undo) = s.apply_reviewed_invitation(4, t, &choices[1..]).unwrap();
        assert!(s.day(cut).entries.is_empty());
        assert_eq!(s.day(choice_day).entries.len(), 1);
        s.undo(undo).unwrap();
        let mut undone: serde_json::Value = serde_json::from_slice(&snapshot()).unwrap();
        undone["revision"] = 4.into();
        assert_eq!(
            undone,
            serde_json::from_slice::<serde_json::Value>(&before).unwrap()
        );
        let t = inspect(&mut s, 6);
        s.apply_reviewed_invitation(
            6,
            t,
            &[SkipDecision {
                original: restart,
                keep: false,
            }],
        )
        .unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        assert!(s.day(cut).entries.is_empty() && s.day(choice_day).entries.is_empty());
        assert_eq!(s.day(stay).entries[0].kind, Kind::Appointment);
        assert_eq!(s.day(date("2026-12-08")).entries[0].id, earlier.id);
        assert_eq!(s.day(date("2026-12-23")).entries[0].span.start, 960);
        assert!(s.day(date("2026-12-30")).entries.is_empty());
        assert!(!review_range(&mut s, 7, RESTART).can_save);
        let gap_update = REINSTATE.replace("SEQUENCE:3", "SEQUENCE:5");
        let blocked = review_range(&mut s, 7, &gap_update);
        assert!(blocked.token.is_none() && blocked.notice.contains("cancelled interval"));
        let extension = weekly
            .replace("COUNT=6", "COUNT=8")
            .replace("SEQUENCE:1", "SEQUENCE:4");
        let t = review_range(&mut s, 7, &extension).token.unwrap();
        s.apply_invitation(7, t).unwrap();
        assert!(s.day(cut).entries.is_empty() && s.day(date("2026-12-30")).entries.is_empty());
        assert_eq!(s.day(date("2027-01-20")).entries[0].span.start, 960);
    }

    #[test]
    fn distant_restart_after_first_slot_cancellation_keeps_a_compact_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let t = review_range(&mut s, 0, weekly).token.unwrap();
        s.apply_invitation(0, t).unwrap();
        let t = review_range(&mut s, 1, &CANCEL_FUTURE.replace("20261215", "20261201"))
            .token
            .unwrap();
        s.apply_invitation(1, t).unwrap();
        let first = date("2026-12-01");
        let restart = first + chrono::Days::new(7 * 2500);
        let resumed = restart.succ_opt().unwrap();
        let req = RESTART
            .replace("20261222", &restart.format("%Y%m%d").to_string())
            .replace("20261223", &resumed.format("%Y%m%d").to_string());
        let r = review_range(&mut s, 2, &req);
        assert_eq!(r.range_dates.len(), 4);
        s.apply_invitation(2, r.token.unwrap()).unwrap();
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        for d in [
            first,
            first + chrono::Days::new(7 * 1200),
            restart - chrono::Days::new(7),
        ] {
            assert!(s.day(d).entries.is_empty());
        }
        assert_eq!(
            s.day(resumed + chrono::Days::new(7 * 25)).entries[0]
                .span
                .start,
            960
        );
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("calendar.json")).unwrap())
                .unwrap();
        assert_eq!(json["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            json["entries"][0]["invitation"]["cancelled_intervals"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn deleting_a_private_copy_severs_its_link_and_legacy_cuts_stay_closed() {
        use atrium_calendar::{invitation::SkipDecision, EditScope};
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let t = review_range(&mut s, 0, weekly).token.unwrap();
        s.apply_invitation(0, t).unwrap();
        let original = date("2026-12-22");
        let row = &s.day(original).entries[0];
        let mut fields = row.fields_on(original);
        fields.date = date("2026-12-11");
        s.change(1, row.id, original, EditScope::ThisEvent, Some(fields))
            .unwrap();
        let t = review_range(&mut s, 2, CANCEL_FUTURE).token.unwrap();
        s.apply_reviewed_invitation(
            2,
            t,
            &[SkipDecision {
                original,
                keep: true,
            }],
        )
        .unwrap();
        let day = date("2026-12-11");
        let mut unrelated = s.day(day).entries.pop().unwrap();
        s.change(3, unrelated.id, day, EditScope::ThisEvent, None)
            .unwrap();
        unrelated.source = None;
        s.add(4, unrelated.clone()).unwrap();
        let r = review_range(&mut s, 5, REINSTATE);
        assert!(r.reinstating && r.range_dates.iter().all(|d| d.private_copy.is_none()));
        let (_, undo) = s.apply_invitation(5, r.token.unwrap()).unwrap();
        assert_eq!(s.day(day).entries[0].id, unrelated.id);
        s.undo(undo).unwrap();
        drop(s);
        let path = dir.path().join("calendar.json");
        let mut snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot["version"] = 10.into();
        snapshot["cancelled"][0]
            .as_object_mut()
            .unwrap()
            .remove("tail");
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let denied = review_range(&mut s, 7, REINSTATE);
        assert!(denied.token.is_none() && denied.notice.contains("older cancellation"));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(s.day(day).entries[0].id, unrelated.id);
    }

    #[test]
    fn reinstatement_reviews_current_private_copy_and_keeps_the_live_prefix() {
        use atrium_calendar::{invitation::SkipDecision, EditScope};
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let save = |s: &mut Store, rev, ics| {
            let t = review_range(s, rev, ics).token.unwrap();
            s.apply_invitation(rev, t).unwrap();
        };
        save(&mut s, 0, weekly);
        save(&mut s, 1, RANGE_REQUEST);
        // A projected range occurrence has a deterministic ID. Move it onto the
        // time at which the organizer will later bring that same meeting back.
        let wed = date("2026-12-23");
        let row = s.day(wed).entries.pop().unwrap();
        let mut fields = row.fields_on(wed);
        fields.date = date("2026-12-11");
        s.change(2, row.id, wed, EditScope::ThisEvent, Some(fields))
            .unwrap();
        let cut = CANCEL_FUTURE.replace("SEQUENCE:2", "SEQUENCE:3");
        let proposal = REINSTATE.replace("SEQUENCE:3", "SEQUENCE:4");
        let token = review_range(&mut s, 3, &cut).token.unwrap();
        let mut choice = SkipDecision {
            original: date("2026-12-22"),
            keep: true,
        };
        s.apply_reviewed_invitation(3, token, &[choice]).unwrap();
        let mut private = row.fields_on(wed);
        private.title = "Private appointment".into();
        s.change(
            4,
            row.id,
            date("2026-12-11"),
            EditScope::ThisEvent,
            Some(private.clone()),
        )
        .unwrap();
        let earlier = date("2026-12-09");
        let old = s.day(earlier).entries.pop().unwrap();
        let mut edited = old.fields_on(earlier);
        edited.date = date("2026-12-18");
        s.change(
            5,
            old.id,
            earlier,
            EditScope::ThisEvent,
            Some(edited.clone()),
        )
        .unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        let p = range_proposal(&proposal);
        let mut source = range_source(&p);
        source.verified = false;
        let denied = s.review_invitation(6, &source, p.clone()).unwrap();
        assert!(denied.token.is_none() && denied.range_dates.is_empty());
        assert!(review_range(&mut s, 6, REINSTATE).token.is_none());
        let before = || std::fs::read(dir.path().join("calendar.json")).unwrap();
        let baseline = before();
        let far = wed + chrono::Days::new(7 * 2500);
        for rev in [6, 8] {
            choice.keep = rev == 6;
            let r = review_range(&mut s, rev, &proposal);
            assert!(r.reinstating && r.can_save);
            let copy = r
                .range_dates
                .iter()
                .find(|d| d.original == choice.original)
                .unwrap();
            assert_eq!(copy.private_copy.as_ref(), Some(&private));
            assert_eq!(copy.after.as_ref().unwrap().date, wed);
            assert!(r
                .range_dates
                .iter()
                .all(|d| d.original >= date("2026-12-15")));
            assert_eq!(s.day(wed).entries.len(), 1);
            assert!(s.day(far).entries.is_empty());
            let t = r.token.unwrap();
            assert!(s.apply_invitation(rev, t).is_err());
            if rev == 6 {
                assert_eq!(before(), baseline);
            }
            let undo = s.apply_reviewed_invitation(rev, t, &[choice]).unwrap().1;
            let day = s.day(wed).entries;
            assert_eq!(day.len(), if choice.keep { 2 } else { 1 });
            let returned = day.iter().find(|e| e.invitation.is_some()).unwrap();
            assert_ne!(returned.id, row.id);
            assert_eq!(returned.span.start, 960);
            assert_eq!(day.iter().any(|r| r.id == row.id), choice.keep);
            assert_eq!(s.day(far).entries.len(), 1);
            assert_eq!(
                s.day(date("2026-12-18")).entries[0].fields_on(edited.date),
                edited
            );
            assert_eq!(s.day(date("2026-12-01")).entries[0].span.start, 840);
            assert!(!review_range(&mut s, rev + 1, &proposal).can_save);
            assert!(review_range(&mut s, rev + 1, &cut).token.is_none());
            s.undo(undo).unwrap();
            let mut restored: serde_json::Value = serde_json::from_slice(&before()).unwrap();
            restored["revision"] = 6.into();
            assert_eq!(
                restored,
                serde_json::from_slice::<serde_json::Value>(&baseline).unwrap()
            );
        }
    }

    #[test]
    fn reinstating_from_first_slot_preserves_separate_moves_skips_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let weekly = RANGE_MASTER.replace("COUNT=4", "COUNT=4\nEXDATE:20261222T140000Z");
        let t = review_range(&mut s, 0, &weekly).token.unwrap();
        s.apply_invitation(0, t).unwrap();
        let cut = CANCEL_FUTURE.replace("20261215", "20261201");
        let t = review_range(&mut s, 1, &cut).token.unwrap();
        s.apply_invitation(1, t).unwrap();
        let req = REINSTATE
            .replace("20261215", "20261201")
            .replace("20261216", "20261202");
        let r = review_range(&mut s, 2, &req);
        let dates: Vec<_> = r
            .range_dates
            .iter()
            .map(|d| d.after.as_ref().map(|e| e.date))
            .collect();
        assert_eq!(
            dates,
            [
                Some(date("2026-12-02")),
                Some(date("2026-12-09")),
                Some(date("2026-12-17")),
                None
            ]
        );
        s.apply_invitation(2, r.token.unwrap()).unwrap();
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.day(date("2026-12-17")).entries[0].span.start, 660);
        assert!(s.day(date("2026-12-23")).entries.is_empty());
        assert!(s.day(date("2026-12-30")).entries.is_empty());
    }

    #[test]
    fn range_cancellation_uses_original_dates_and_undo_restores_the_open_tail() {
        use atrium_calendar::{
            invitation::{SkipDecision, SkipEffect},
            EditScope,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let t = review_range(&mut store, 0, weekly).token.unwrap();
        store.apply_invitation(0, t).unwrap();
        let t = review_range(&mut store, 1, RANGE_REQUEST).token.unwrap();
        store.apply_invitation(1, t).unwrap();
        let move_here = |s: &mut Store, rev, day: &str, to: &str| {
            let shown = date(day);
            let row = s.day(shown).entries.pop().unwrap();
            let mut edit = row.fields_on(shown);
            edit.date = date(to);
            edit.span = Span {
                start: 675,
                end: 720,
            };
            s.change(rev, row.id, shown, EditScope::ThisEvent, Some(edit))
                .unwrap();
            row.id
        };
        let before = move_here(&mut store, 2, "2026-12-09", "2026-12-18");
        let moved = move_here(&mut store, 3, "2026-12-23", "2026-12-11");
        let text = CANCEL_FUTURE.replace("SEQUENCE:2", "SEQUENCE:3");
        let r = review_range(&mut store, 4, &text);
        assert_eq!(
            r.skips.iter().map(|s| s.effect).collect::<Vec<_>>(),
            [
                SkipEffect::KeepEarlier,
                SkipEffect::Cancel,
                SkipEffect::ChooseLocalMove,
                SkipEffect::Cancel
            ]
        );
        assert_eq!(
            r.skips[1].current.as_ref().unwrap().date,
            date("2026-12-16")
        );
        assert_eq!(
            r.skips[2].current.as_ref().unwrap().date,
            date("2026-12-11")
        );
        let t = r.token.unwrap();
        assert!(store.apply_invitation(4, t).is_err());
        assert!(store
            .apply_reviewed_invitation(
                4,
                t,
                &[SkipDecision {
                    original: date("2026-12-11"),
                    keep: true
                }]
            )
            .is_err());
        let far = date("2026-12-09") + chrono::Days::new(7 * 2500);
        let distant_id = store.day(far).entries[0].id;
        let mut decision = SkipDecision {
            original: date("2026-12-22"),
            keep: true,
        };
        let undo = store
            .apply_reviewed_invitation(4, t, &[decision])
            .unwrap()
            .1;
        assert!(store.day(far).entries.is_empty());
        let copy = store.day(date("2026-12-11")).entries.pop().unwrap();
        assert_eq!((copy.id, copy.kind), (moved, Kind::Appointment));
        assert!(copy.invitation.is_none());
        assert_eq!(store.day(date("2026-12-18")).entries[0].id, before);
        assert_eq!(store.day(date("2026-12-01")).entries[0].span.start, 840);
        assert!(review_range(&mut store, 5, &text).token.is_none());
        assert!(
            review_range(&mut store, 5, &weekly.replace("SEQUENCE:1", "SEQUENCE:4"))
                .token
                .is_none()
        );
        store.undo(undo).unwrap();
        assert!(store.undo(undo).is_err());
        assert_eq!(store.day(far).entries[0].id, distant_id);
        assert!(store.day(date("2026-12-11")).entries[0]
            .invitation
            .is_some());
        decision.keep = false;
        let t = review_range(&mut store, 6, &text).token.unwrap();
        assert!(store.apply_reviewed_invitation(4, t, &[decision]).is_err());
        store.apply_reviewed_invitation(6, t, &[decision]).unwrap();
        drop(store);
        let mut store = Store::open(dir.path()).unwrap();
        assert!(
            store.day(far).entries.is_empty() && store.day(date("2026-12-11")).entries.is_empty()
        );
        assert_eq!(store.day(date("2026-12-18")).entries[0].id, before);
        assert!(review_range(&mut store, 7, &text).token.is_none());
    }

    #[test]
    fn private_copy_decisions_are_independent_when_two_moves_share_a_saved_day() {
        use atrium_calendar::{invitation::SkipDecision, EditScope};
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let t = review_range(&mut store, 0, weekly).token.unwrap();
        store.apply_invitation(0, t).unwrap();
        let destination = date("2026-12-11");
        let first = date("2026-12-15");
        let second = date("2026-12-22");
        for (index, day) in [first, second].into_iter().enumerate() {
            let source = store.day(day).entries.pop().unwrap();
            let mut fields = source.fields_on(day);
            fields.date = destination;
            fields.span = Span {
                start: 600 + index as u16 * 60,
                end: 645 + index as u16 * 60,
            };
            store
                .change(
                    1 + index as u64,
                    source.id,
                    day,
                    EditScope::ThisEvent,
                    Some(fields),
                )
                .unwrap();
        }
        let baseline = store.day(destination).entries;
        assert_eq!(baseline.len(), 2);
        let t = review_range(&mut store, 3, CANCEL_FUTURE).token.unwrap();
        let keep = SkipDecision {
            original: second,
            keep: true,
        };
        assert!(store
            .apply_reviewed_invitation(3, t, &[keep, keep])
            .is_err());
        assert_eq!(store.day(destination).entries.len(), 2);
        let undo = store
            .apply_reviewed_invitation(
                3,
                t,
                &[
                    keep,
                    SkipDecision {
                        original: first,
                        keep: false,
                    },
                ],
            )
            .unwrap()
            .1;
        let copies = store.day(destination).entries;
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].id, baseline[1].id);
        assert_eq!(copies[0].kind, Kind::Appointment);
        store.undo(undo).unwrap();
        assert_eq!(
            serde_json::to_value(store.day(destination).entries).unwrap(),
            serde_json::to_value(baseline).unwrap()
        );
    }

    #[test]
    fn a_large_cancellation_choice_list_round_trips_through_fasti() {
        use atrium_calendar::{invitation::SkipDecision, wire, CalendarPage, EditScope};
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let weekly = include_str!("../../tests/fixtures/weekly.ics");
        let t = review_range(&mut store, 0, weekly).token.unwrap();
        store.apply_invitation(0, t).unwrap();
        let start = date("2026-12-15");
        let mut choices = Vec::new();
        for week in 0..120 {
            let original = start + chrono::Days::new(week * 7);
            let row = store.day(original).entries.pop().unwrap();
            let mut edit = row.fields_on(original);
            edit.date = original.succ_opt().unwrap();
            store
                .change(week + 1, row.id, original, EditScope::ThisEvent, Some(edit))
                .unwrap();
            choices.push(SkipDecision {
                original,
                keep: week % 2 == 0,
            });
        }
        let p = range_proposal(CANCEL_FUTURE);
        let review = wire::Request::ReviewInvitation {
            revision: 121,
            source: range_source(&p),
            proposal: p,
        };
        let socket = dir.path().join("fasti.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut served = 0;
                while served < 2 {
                    match listener.accept() {
                        Ok((mut client, _)) => {
                            wire::serve(&mut client, &mut store).unwrap();
                            served += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "Missing Fasti test request"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Err(e) => panic!("Fasti test listener failed: {e}"),
                    }
                }
            });
            let r: atrium_calendar::invitation::Review = wire::exchange(&socket, &review).unwrap();
            let apply = wire::Request::ApplyInvitation {
                revision: 121,
                token: r.token.unwrap(),
                choices,
            };
            assert!(serde_json::to_vec(&apply).unwrap().len() > 4096);
            let page: CalendarPage = wire::exchange(&socket, &apply).unwrap();
            assert_eq!(page.revision, 122);
        });
        assert_eq!(
            store.day(start.succ_opt().unwrap()).entries[0].kind,
            Kind::Appointment
        );
        assert!(store.day(start + chrono::Days::new(8)).entries.is_empty());
    }

    #[test]
    fn range_cancellation_checks_delivery_and_newer_exceptions_before_removing_any_date() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let t = review_range(&mut store, 0, RANGE_MASTER).token.unwrap();
        store.apply_invitation(0, t).unwrap();
        let p = range_proposal(CANCEL_FUTURE);
        for fault in 0..4 {
            let mut mail = range_source(&p);
            match fault {
                0 => mail.verified = false,
                1 => mail.from = "other@fixture.invalid".into(),
                2 => mail.delivery.as_mut().unwrap().scope = Some(p.original.unwrap().property()),
                _ => mail.delivery.as_mut().unwrap().cancelled = false,
            }
            assert!(store
                .review_invitation(1, &mail, p.clone())
                .ok()
                .and_then(|r| r.token)
                .is_none());
        }
        let new = include_str!("../../tests/fixtures/move-one.ics")
            .replace("SEQUENCE:2", "SEQUENCE:5")
            .replace("20261208T", "20261222T")
            .replace("20261209T", "20261223T");
        let t = review_range(&mut store, 1, &new).token.unwrap();
        store.apply_invitation(1, t).unwrap();
        assert!(review_range(&mut store, 2, CANCEL_FUTURE).token.is_none());
        assert_eq!(store.day(date("2026-12-17")).entries.len(), 1);
        let fresh = CANCEL_FUTURE
            .replace("SEQUENCE:2", "SEQUENCE:6")
            .replace("20261215T", "20261201T");
        let r = review_range(&mut store, 2, &fresh);
        assert_eq!(r.skips.len(), 4);
        let undo = store.apply_invitation(2, r.token.unwrap()).unwrap().1;
        assert!(store.day(date("2026-12-01")).entries.is_empty());
        assert!(store.day(date("2026-12-23")).entries.is_empty());
        assert!(review_range(&mut store, 3, &fresh).token.is_none());
        store.undo(undo).unwrap();
        assert_eq!(store.day(date("2026-12-23")).entries.len(), 1);
        // A newer detached meeting in the prefix cannot invalidate a cancellation of the tail.
        let last = CANCEL_FUTURE
            .replace("20261215T", "20261222T")
            .replace("SEQUENCE:2", "SEQUENCE:6");
        let update_before =
            include_str!("../../tests/fixtures/move-one.ics").replace("SEQUENCE:2", "SEQUENCE:7");
        let t = review_range(&mut store, 4, &update_before).token.unwrap();
        store.apply_invitation(4, t).unwrap();
        let keep = store.day(date("2026-12-09")).entries[0].id;
        let t = review_range(&mut store, 5, &last).token.unwrap();
        let undo = store.apply_invitation(5, t).unwrap().1;
        assert_eq!(store.day(date("2026-12-09")).entries[0].id, keep);
        store.undo(undo).unwrap();

        // The starting meeting can itself be a private move, even at DTSTART.
        let day = date("2026-12-01");
        let row = store.day(day).entries.pop().unwrap();
        let mut edit = row.fields_on(day);
        edit.date = date("2026-11-30");
        store
            .change(
                7,
                row.id,
                day,
                atrium_calendar::EditScope::ThisEvent,
                Some(edit),
            )
            .unwrap();
        let t = review_range(&mut store, 8, &fresh.replace("SEQUENCE:6", "SEQUENCE:8"))
            .token
            .unwrap();
        assert!(store.apply_invitation(8, t).is_err());
        store
            .apply_reviewed_invitation(
                8,
                t,
                &[atrium_calendar::invitation::SkipDecision {
                    original: day,
                    keep: true,
                }],
            )
            .unwrap();
        assert_eq!(
            store.day(date("2026-11-30")).entries[0].kind,
            Kind::Appointment
        );
        assert!(store.day(day).entries.is_empty());
    }

    #[test]
    fn legacy_range_extends_after_its_anchor_is_deleted() {
        // A v8 calendar stored the reviewed range as detached meetings. Its
        // extension can recover the accepted shift even after deleting the anchor.
        let legacy = tempfile::tempdir().unwrap();
        let mut store = Store::open(legacy.path()).unwrap();
        let t = review_range(&mut store, 0, RANGE_MASTER).token.unwrap();
        store.apply_invitation(0, t).unwrap();
        let t = review_range(&mut store, 1, RANGE_REQUEST).token.unwrap();
        store.apply_invitation(1, t).unwrap();
        let anchor_day = date("2026-12-09");
        let id = store.day(anchor_day).entries[0].id;
        store
            .change(
                2,
                id,
                anchor_day,
                atrium_calendar::EditScope::ThisEvent,
                None,
            )
            .unwrap();
        drop(store);
        let path = legacy.path().join("calendar.json");
        let mut snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot["version"] = 8.into();
        for entry in snapshot["entries"].as_array_mut().unwrap() {
            entry["invitation"]
                .as_object_mut()
                .unwrap()
                .remove("ranges");
        }
        std::fs::write(path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let mut store = Store::open(legacy.path()).unwrap();
        let extension = include_str!("../../tests/fixtures/weekly.ics")
            .replace("SEQUENCE:1", "SEQUENCE:3")
            .replace("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=16");
        let r = review_range(&mut store, 3, &extension);
        assert_eq!(
            r.range_dates[0].after.as_ref().unwrap().date,
            date("2026-12-30")
        );
        store.apply_invitation(3, r.token.unwrap()).unwrap();
        assert_eq!(
            store.day(date("2027-03-17")).entries[0].span,
            Span {
                start: 960,
                end: 1020
            }
        );
        assert!(store.day(date("2027-03-24")).entries.is_empty());
        assert!(store.day(anchor_day).entries.is_empty());
    }

    #[test]
    fn long_and_open_ranges_keep_private_edits_and_extend_after_restart() {
        for rule in ["FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=11"] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(dir.path()).unwrap();
            let t = review_range(
                &mut store,
                0,
                &RANGE_MASTER.replace("FREQ=WEEKLY;COUNT=4", rule),
            )
            .token
            .unwrap();
            store.apply_invitation(0, t).unwrap();
            let open = !rule.contains("COUNT");
            let day = date("2027-01-12");
            let local = store.day(day).entries.pop().unwrap();
            let mut edit = local.fields_on(day);
            edit.date = date("2027-01-14");
            edit.span = Span {
                start: 690,
                end: 735,
            };
            store
                .change(
                    1,
                    local.id,
                    day,
                    atrium_calendar::EditScope::ThisEvent,
                    Some(edit),
                )
                .unwrap();
            let r = review_range(&mut store, 2, RANGE_REQUEST);
            assert!(r.range_summary.as_ref().unwrap().contains(if open {
                "no end date"
            } else {
                "2027-02-09"
            }));
            assert!(r.range_dates.iter().any(|d| d.original == day
                && d.after.is_none()
                && d.before.as_ref().unwrap().date == date("2027-01-14")));
            store.apply_invitation(2, r.token.unwrap()).unwrap();
            assert!(store.day(date("2027-01-13")).entries.is_empty());
            assert_eq!(store.day(date("2027-01-14")).entries[0].span.start, 690);
            let regular = date("2027-01-27");
            let row = store.day(regular).entries.pop().unwrap();
            assert_eq!(
                row.span,
                Span {
                    start: 960,
                    end: 1020
                }
            );
            assert!(store.day(date("2027-01-26")).entries.is_empty());
            let page = store.page(regular, Some(atrium_calendar::CalendarView::Month));
            assert_eq!(page.on(regular).next().unwrap().id, row.id);
            let hits = store
                .search(
                    &atrium_calendar::SearchQuery {
                        text: "studio".into(),
                        start: regular,
                        end: regular,
                    },
                    0,
                    None,
                )
                .unwrap();
            assert_eq!(hits.hits[0].id, row.id);
            assert!(store.open_search_hit(3, row.id, regular).is_ok());
            let mut later = row.fields_on(regular);
            later.date = date("2027-01-29");
            store
                .change(
                    3,
                    row.id,
                    regular,
                    atrium_calendar::EditScope::ThisEvent,
                    Some(later),
                )
                .unwrap();
            let extension =
                include_str!("../../tests/fixtures/weekly.ics").replace("SEQUENCE:1", "SEQUENCE:3");
            if !open {
                assert!(store.day(date("2027-02-17")).entries.is_empty());
                let extended = review_range(&mut store, 4, &extension);
                assert_eq!(
                    extended.range_dates[0].after.as_ref().unwrap().date,
                    date("2027-02-17")
                );
                assert!(store.day(date("2027-02-17")).entries.is_empty());
                let undo = store
                    .apply_invitation(4, extended.token.unwrap())
                    .unwrap()
                    .1;
                assert_eq!(store.day(date("2027-02-17")).entries[0].span.start, 960);
                store.undo(undo).unwrap();
                assert!(store.day(date("2027-02-17")).entries.is_empty());
                let t = review_range(&mut store, 6, &extension).token.unwrap();
                store.apply_invitation(6, t).unwrap();
            }
            drop(store);
            let store = Store::open(dir.path()).unwrap();
            let distant = date("2026-12-09") + chrono::Days::new(7 * 2500);
            assert_eq!(
                store.day(distant).entries[0].span,
                Span {
                    start: 960,
                    end: 1020
                }
            );
            assert!(store.day(distant.pred_opt().unwrap()).entries.is_empty());
            assert!(store.day(regular).entries.is_empty());
            assert_eq!(store.day(date("2027-01-29")).entries[0].id, row.id);
            assert_eq!(store.day(date("2026-12-01")).entries[0].span.start, 840);
            assert_eq!(store.day(date("2027-01-14")).entries[0].span.start, 690);
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
