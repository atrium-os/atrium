//! Guest gate only. Run at the actual app UID inside each installed UI jail.
use aqueduct::{classes::CLASS_CALENDAR, flag, Header};
use atrium_calendar::{
    wire::{call, Request},
    CalendarView, Entry, Kind, Span,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [socket, date, expected] = args.as_slice() else {
        return Err("expected socket, date and title (or --denied)".into());
    };
    if matches!(expected.as_str(), "--work-schedule" | "--personal-schedule") {
        seed(
            Path::new(socket),
            date.parse()?,
            expected == "--work-schedule",
        )?;
        return Ok(());
    }
    let request = Request::Day {
        date: date.parse()?,
    };
    if expected == "--denied" {
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
