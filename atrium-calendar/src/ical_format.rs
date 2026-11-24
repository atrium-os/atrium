//! Calendar and Mail use this decoder in their disposable parse workers.
use crate::{
    import::{Candidates, Issue, Outcome, FILE_LIMIT, ROW_LIMIT},
    Entry, Kind, Span, Zone,
};
use chrono::{NaiveDate, NaiveDateTime, Timelike};
use chrono_tz::Tz;
use std::collections::{HashMap, HashSet};
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
    limit: Option<crate::WeeklyLimit>,
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

pub fn parse_mode(bytes: &[u8], invitation: bool) -> Result<Outcome> {
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
                        event.limit = crate::WeeklyLimit::parse(v).ok_or(Issue::Recurrence)?;
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
        if children.len() > crate::invitation::MAX_MOVES
            || (group
                && (events.len() > crate::invitation::MAX_CANCELLED_DATES || e.original.is_none()))
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
            Ok(crate::invitation::OccurrenceId {
                date: stamp.date,
                minute: stamp.minute.ok_or(Issue::Recurrence)?,
                zone: stamp.zone,
            })
        };
        let original = e.original.map(occurrence).transpose()?;
        let mut cancelled_dates = Vec::new();
        if group {
            cancelled_dates.push(crate::invitation::CancelledDate {
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
                cancelled_dates.push(crate::invitation::CancelledDate {
                    original,
                    sequence: child.sequence.unwrap_or(0),
                });
                continue;
            }
            exceptions.push(crate::invitation::IncludedMove {
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
        if skips.len() + exceptions.len() > crate::invitation::MAX_MOVES {
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
        return Ok(Outcome::Invitation(crate::invitation::Proposal {
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
