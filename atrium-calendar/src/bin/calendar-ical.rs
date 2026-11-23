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
    fn row(&self) -> Result<Entry> {
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
        };
        for d in &self.skipped {
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

fn parse(bytes: &[u8]) -> Result<Candidates> {
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
                    "METHOD" if v.eq_ignore_ascii_case("PUBLISH") => {}
                    "METHOD" => return Err(Issue::Scheduling),
                    "X-WR-TIMEZONE" => return Err(Issue::Zone),
                    k if k.starts_with("X-") => omitted = true,
                    _ => return Err(Issue::Format),
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
    let mut masters = HashMap::new();
    let mut rows = Vec::new();
    for (i, e) in events.iter().enumerate() {
        let uid = e.uid.as_deref().ok_or(Issue::Format)?;
        if e.original.is_none() && masters.insert(uid, i).is_some() {
            return Err(Issue::Recurrence);
        }
        rows.push(e.row()?);
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
    Ok(Candidates {
        rows,
        omitted_details: omitted,
    })
}

fn handle(_: Parse, file: Option<File>) -> std::result::Result<Parsed, String> {
    let mut bytes = Vec::new();
    file.ok_or("missing file")?
        .take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "read failed")?;
    let outcome = match parse(&bytes) {
        Ok(rows) => Outcome::Ready(rows),
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
