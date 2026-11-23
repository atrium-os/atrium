//! Guest gate only. Run at the actual app UID inside each installed UI jail.
use aqueduct::{classes::CLASS_CALENDAR, flag, Header};
use atrium_calendar::{
    wire::{call, search, EventChange, Request},
    CalendarView, EditScope, Entry, Kind, SearchQuery, Span,
};
use chrono::{Datelike, Days, NaiveDate};
use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};
use uuid::Uuid;

fn seed(socket: &Path, date: NaiveDate, work: bool) -> Result<(), String> {
    let monday = CalendarView::Week.dates(date).next().unwrap();
    let first = date.with_day(1).unwrap();
    let entries = if work {
        vec![
            (monday, Kind::Weekly, "Planning review", 555, 600),
            (monday, Kind::Appointment, "Budget check", 570, 605),
            (
                monday + Days::new(1),
                Kind::Appointment,
                "Studio time",
                660,
                750,
            ),
            (
                monday + Days::new(3),
                Kind::Weekly,
                "Release review",
                840,
                885,
            ),
            (monday + Days::new(4), Kind::AllDay, "Field day", 0, 1440),
        ]
    } else {
        vec![
            (
                first + Days::new(3),
                Kind::Weekly,
                "Community garden",
                660,
                720,
            ),
            (first + Days::new(11), Kind::AllDay, "Family visit", 0, 1440),
            (
                first + Days::new(17),
                Kind::Appointment,
                "Bike repair",
                720,
                750,
            ),
        ]
    };
    let mut revision = call(socket, &Request::Day { date })?.revision;
    for (date, kind, title, start, end) in entries {
        let entry = Entry {
            excluded: Vec::new(),
            id: Uuid::new_v4(),
            date,
            kind,
            title: title.into(),
            span: Span { start, end },
        };
        revision = call(socket, &Request::Add { revision, entry })?.revision;
    }
    Ok(())
}

// Read back exactly the seeded series, including next week, through the app socket.
fn check_review(socket: &Path, date: NaiveDate, moved: bool) -> Result<(), String> {
    let current = call(socket, &Request::Day { date })?;
    let next = call(
        socket,
        &Request::Day {
            date: date + Days::new(7),
        },
    )?;
    let old_slot = Span {
        start: 840,
        end: 885,
    };
    let new_slot = Span {
        start: 900,
        end: 945,
    };
    let siblings: Vec<_> = next
        .entries
        .iter()
        .filter(|e| e.title == "Release review")
        .collect();
    let [series] = siblings.as_slice() else {
        return Err("Missing or duplicated sibling".into());
    };
    let rows: Vec<_> = current
        .entries
        .iter()
        .filter(|e| e.title == "Release review")
        .collect();
    let [row] = rows.as_slice() else {
        return Err("Missing or duplicated review".into());
    };
    let (expected_kind, busy, open) = if moved {
        (
            Kind::Occurrence {
                series: series.id,
                original: date,
            },
            new_slot,
            old_slot,
        )
    } else {
        (Kind::Weekly, old_slot, new_slot)
    };
    if series.kind != Kind::Weekly
        || series.span != old_slot
        || row.kind != expected_kind
        || row.span != busy
        || !current.openings(busy, 45)?.is_empty()
        || current.openings(open, 45)? != [open]
    {
        return Err("Unexpected occurrence or availability after editing".into());
    }
    Ok(())
}

fn search_fixture(socket: &Path, today: NaiveDate, mode: &str) -> Result<(), String> {
    let first = today.with_day(1).unwrap();
    let query = SearchQuery {
        text: "lantern".into(),
        start: first,
        end: CalendarView::Month.dates(first).last().unwrap(),
    };
    let moved = first + Days::new(1);
    let private = first + Days::new(2);
    let personal = mode.ends_with("personal");
    let change = |revision, id, shown, operation| {
        call(
            socket,
            &Request::Change {
                revision,
                id,
                shown,
                scope: EditScope::ThisEvent,
                operation,
            },
        )
    };
    if mode.starts_with("--seed-search-") {
        let event = Entry {
            id: Uuid::new_v4(),
            date: if personal { private } else { first },
            title: "Lantern".into(),
            excluded: Vec::new(),
            kind: if personal {
                Kind::Appointment
            } else {
                Kind::Weekly
            },
            span: if personal {
                Span {
                    start: 1035,
                    end: 1065,
                }
            } else {
                Span {
                    start: 660,
                    end: 690,
                }
            },
        };
        let revision = call(socket, &Request::Day { date: first })?.revision;
        let saved = call(
            socket,
            &Request::Add {
                revision,
                entry: event.clone(),
            },
        )?;
        if !personal {
            let mut fields = event.fields_on(moved);
            fields.span = Span {
                start: 720,
                end: 750,
            };
            let edited = change(saved.revision, event.id, first, EventChange::Update(fields))?;
            let skipped = change(
                edited.revision,
                event.id,
                first + Days::new(7),
                EventChange::Delete,
            )?;
            // Put Lantern beyond the first six rows in its day and exercise search pagination.
            let mut revision = skipped.revision;
            for n in 0..7 {
                let desk = Entry {
                    id: Uuid::new_v4(),
                    title: format!("Desk prep {}", n + 1),
                    kind: Kind::Appointment,
                    date: moved,
                    span: Span {
                        start: 480 + n * 15,
                        end: 495 + n * 15,
                    },
                    excluded: vec![],
                };
                revision = call(
                    socket,
                    &Request::Add {
                        revision,
                        entry: desk,
                    },
                )?
                .revision;
            }
        }
    }
    let mut result = search(socket, query.clone(), 0, None)?;
    if mode == "--cancel-search-hit" {
        let hit = result.hits.first().ok_or("Missing moved meeting")?;
        if hit.date != moved
            || !matches!(hit.kind, Kind::Occurrence {original,..} if original == first)
        {
            return Err("Expected the moved meeting before cancellation".into());
        }
        change(result.revision, hit.id, hit.date, EventChange::Delete)?;
        result = search(socket, query.clone(), 0, None)?;
    }
    let mut expected = Vec::new();
    if personal {
        expected.push((private, 1035));
    } else {
        if mode != "--cancel-search-hit" {
            expected.push((moved, 720));
        }
        for day in [14, 21, 28] {
            let date = first + Days::new(day);
            if date <= query.end {
                expected.push((date, 660));
            }
        }
    }
    let actual: Vec<_> = result.hits.iter().map(|h| (h.date, h.span.start)).collect();
    if actual != expected
        || result.total != expected.len()
        || result.hits.iter().any(|h| h.title != "Lantern")
    {
        return Err("Incorrect persona search dates, times or count".into());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [socket, date, expected] = args.as_slice() else {
        return Err("expected socket, date and title (or --denied)".into());
    };
    if matches!(
        expected.as_str(),
        "--seed-search-personal"
            | "--seed-search-work"
            | "--check-search-personal"
            | "--cancel-search-hit"
    ) {
        search_fixture(Path::new(socket), date.parse()?, expected)?;
        return Ok(());
    }
    if matches!(expected.as_str(), "--work-schedule" | "--personal-schedule") {
        seed(
            Path::new(socket),
            date.parse()?,
            expected == "--work-schedule",
        )?;
        return Ok(());
    }
    if matches!(expected.as_str(), "--original-review" | "--moved-review") {
        check_review(
            Path::new(socket),
            date.parse()?,
            expected == "--moved-review",
        )?;
        return Ok(());
    }
    let date = date.parse()?;
    let request = if expected == "--denied-search" {
        Request::Search {
            query: SearchQuery {
                text: "lantern".into(),
                start: date,
                end: date,
            },
            offset: 0,
            revision: None,
        }
    } else {
        Request::Day { date }
    };
    if expected.starts_with("--denied") {
        // The fixture temporarily grants connect permission. Prove that
        // Fasti itself closes the connection without returning a single byte.
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let body = serde_json::to_vec(&request)?;
        let mut frame = Header::new(
            CLASS_CALENDAR,
            1,
            flag::RESPONSE_EXPECTED,
            body.len() as u32,
        )
        .encode()
        .to_vec();
        frame.extend(body);
        if let Err(e) = stream.write_all(&frame) {
            if !matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) {
                return Err(e.into());
            }
        }
        match stream.read(&mut [0]) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => return Err(format!("expected peer refusal, got {other:?}").into()),
        }
    } else {
        let day = call(Path::new(socket), &request)?;
        if day.revision != 1 || day.entries.len() != 1 || day.entries[0].title != *expected {
            return Err("incorrect synthetic day or revision".into());
        }
    }
    Ok(())
}
