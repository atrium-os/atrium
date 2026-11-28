//! Synthetic installed-gate driver. The real Calendar window polls Fasti and
//! forwards the live handoff using its own signed app identity.
#[cfg(target_os = "freebsd")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use atrium_calendar::{
        wire::{call, exchange, page, EventChange, Request},
        EditScope, Entry, Kind, Span,
    };
    use chrono::NaiveDate;
    use std::io::Write;
    use std::path::Path;
    use uuid::Uuid;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let date: NaiveDate = "2031-10-14".parse()?;
    let cal = Path::new("/atrium/sockets/calendar/calendar.sock");
    let sock = praeco::client::socket();
    match args.first().map(String::as_str) {
        Some("seed") => {
            let mut rev = call(cal, &Request::Day { date })?.revision;
            for i in 0..21 {
                let (start, end) = match i {
                    18 => (546, 565),
                    19 => (545, 555),
                    20 => (548, 568),
                    _ => (540, 542),
                };
                let id = Uuid::new_v4();
                let entry = Entry {
                    id,
                    date,
                    title: format!("Synthetic reminder {i}"),
                    span: Span { start, end },
                    kind: Kind::Appointment,
                    excluded: vec![],
                    source: None,
                    zone: None,
                    invitation: None,
                    import_hash: None,
                };
                rev = page(
                    cal,
                    &Request::Add {
                        revision: rev,
                        entry,
                    },
                )?
                .revision;
                rev = page(
                    cal,
                    &Request::SetReminder {
                        revision: rev,
                        id,
                        shown: date,
                        enabled: true,
                    },
                )?
                .revision;
                if i == 19 {
                    rev = page(
                        cal,
                        &Request::Change {
                            revision: rev,
                            id,
                            shown: date,
                            scope: EditScope::ThisEvent,
                            operation: EventChange::Delete,
                        },
                    )?
                    .revision;
                }
            }
            let saved = page(cal, &Request::Day { date })?;
            assert_eq!(saved.reminders.len(), 20);
            assert_eq!(saved.entries.len(), 20);
            println!(
                "Seeded 21 opted-in appointments through Calendar IPC; one cancelled before due."
            );
        }
        Some("pin") => {
            let current = page(cal, &Request::Day { date })?;
            let pinned = Some(chrono_tz::UTC);
            assert_eq!(
                page(
                    cal,
                    &Request::SetViewingZone {
                        revision: current.revision,
                        date,
                        view: current.view,
                        pinned
                    }
                )?
                .pinned_zone,
                pinned
            );
        }
        Some("zone") => {
            let result = page(cal, &Request::Day { date });
            if args[1] == "unavailable" {
                if result.err().as_deref() != Some("Device time zone unavailable") {
                    return Err("expected unavailable clock".into());
                }
            } else if result?.viewing_zone.name() != args[1] {
                return Err("unexpected viewing zone".into());
            }
            println!("Calendar zone {}", args[1]);
        }
        Some("review") => {
            // Reading a review never takes a live offer away from Calendar.
            let r: atrium_calendar::store::reminders::Review =
                exchange(cal, &Request::ReminderReview)?;
            let current: usize = args[1].parse()?;
            let history: usize = args[2].parse()?;
            assert_eq!((r.upcoming, r.history), (current, history));
            assert_eq!(r.items.len(), current + history);
            assert!(r.items.iter().all(|r| r.title != "Synthetic reminder 19"));
            if current > 0 {
                assert_eq!(r.items[0].title, "Synthetic reminder 18");
            }
            println!(
                "Private review: {current} current, {history} history, cancelled item absent."
            );
        }
        Some("watch") => {
            let wanted = args.get(1).ok_or("missing expected result")?;
            let mut received = 0;
            let waits = args
                .get(2)
                .map(|n| n.parse::<usize>())
                .transpose()?
                .unwrap_or(5);
            assert!((1..=5).contains(&waits));
            for _ in 0..waits {
                match praeco::reminders::watch(&sock, || {
                    println!("READY");
                    std::io::stdout().flush().unwrap();
                }) {
                    Ok(Some(n)) => {
                        assert_eq!(n.app, "org.atrium.calendar");
                        assert_eq!(n.user, "calfixture");
                        assert_eq!(n.persona.as_deref(), Some("work"));
                        assert_eq!(n.title, "Reminder");
                        assert!(n.body.is_empty() && n.actions.is_empty());
                        println!(
                            "LIVE id={} due_ms={} owner={} persona=work",
                            n.id, n.at_ms, n.user
                        );
                        received += 1;
                    }
                    Ok(None) => (),
                    Err(e) if wanted == "denied" => {
                        assert_eq!(e.to_string(), "Live reminder unavailable");
                        println!("DENIED unresolved subscriber");
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            assert_ne!(wanted, "denied");
            assert_eq!(received, wanted.parse::<usize>()?);
            println!("PASS received {received} live reminders");
        }
        Some("history") => {
            use aqueduct::{flag, Connection};
            use praeco::proto::{self, op, status};
            let mut c = Connection::connect(&sock)?;
            c.send_message(proto::CLASS, op::SUBSCRIBE, flag::RESPONSE_EXPECTED, &[])?;
            let reply = c
                .recv_message_or_timeout(std::time::Duration::from_secs(2))?
                .ok_or("missing acknowledgement")?;
            assert_eq!(
                (reply.op, reply.payload.first()),
                (op::SUBSCRIBE, Some(&status::OK))
            );
            let mut frames = 0;
            while let Some(m) = c.recv_message_or_timeout(std::time::Duration::from_millis(500))? {
                frames += 1;
                assert!(frames <= 1, "unexpected ordinary history");
                assert_eq!(m.op, op::EV);
                // Ordinary history would carry the post's app ID and title.
                assert!(!m
                    .payload
                    .windows(b"org.atrium.calendar".len())
                    .any(|w| w == b"org.atrium.calendar"));
                assert!(m.payload.len() < 128);
            }
            println!("Ordinary subscription received zero Calendar history entries");
        }
        _ => return Err("expected seed, review, watch or history".into()),
    }
    Ok(())
}
#[cfg(not(target_os = "freebsd"))]
fn main() {}
