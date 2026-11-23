use atrium_calendar::{store::Store, Day, Entry, Kind, Span};
use chrono::NaiveDate;
use uuid::Uuid;

fn date() -> NaiveDate {
    "2026-11-24".parse().unwrap()
}
fn span(start: u16, end: u16) -> Span {
    Span { start, end }
}
fn entry(kind: Kind, time: Span) -> Entry {
    Entry {
        id: Uuid::new_v4(),
        kind,
        span: time,
        date: date(),
        title: "Budget review".into(),
    }
}

#[test]
fn synthetic_schedules() {
    // Independently specified free ranges from the seven planning scenarios.
    let window = span(540, 840);
    let cases: &[(&str, &[(u16, u16)], &[(u16, u16)])] = &[
        (
            "review",
            &[(540, 570), (630, 720), (720, 780)],
            &[(570, 630), (780, 840)],
        ),
        ("packed", &[(540, 630), (630, 735), (735, 840)], &[]),
        (
            "exact vs 44",
            &[(540, 600), (645, 796), (840, 870)],
            &[(600, 645)],
        ),
        (
            "overlap",
            &[(630, 660), (510, 600), (555, 630), (570, 585), (705, 855)],
            &[(660, 705)],
        ),
        ("empty", &[], &[(540, 840)]),
        (
            "blocking commitments only",
            &[(540, 600), (690, 840)],
            &[(600, 690)],
        ),
        (
            "four gaps",
            &[(585, 600), (645, 660), (705, 720), (765, 840)],
            &[(540, 585), (600, 645), (660, 705), (720, 765)],
        ),
    ];
    for (name, busy, expected) in cases {
        let day = Day {
            revision: 0,
            entries: busy
                .iter()
                .map(|&(a, b)| entry(Kind::Appointment, span(a, b)))
                .collect(),
        };
        assert_eq!(
            day.openings(window, 45).unwrap(),
            expected
                .iter()
                .map(|&(a, b)| span(a, b))
                .collect::<Vec<_>>(),
            "{name}"
        );
    }
}

#[test]
fn hold_races_restart_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    assert!(Store::open(dir.path()).is_err());
    let weekly = entry(Kind::Weekly, span(840, 900));
    store.add(0, weekly.clone()).unwrap();
    let hold = entry(Kind::Hold, span(780, 825));
    // A search is read-only, and a competing write invalidates its revision.
    assert_eq!(
        store.day(date()).openings(span(780, 900), 45).unwrap(),
        vec![span(780, 840)]
    );
    assert!(store.add(0, hold.clone()).is_err());
    store.add(1, hold.clone()).unwrap();
    assert!(store.add(1, hold.clone()).is_err());
    assert!(store.add(2, entry(Kind::Hold, span(800, 845))).is_err());
    assert!(store.release(2, date(), weekly.id).is_err());
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(store.day(date()).revision, 2);
    assert_eq!(store.day(date()).entries[0].id, hold.id);
    assert_eq!(
        store.day(date() + chrono::Days::new(7)).entries[0].id,
        weekly.id
    );
    assert!(store.day(date() + chrono::Days::new(1)).entries.is_empty());
    store.release(2, date(), hold.id).unwrap();
    assert_eq!(
        store.day(date()).openings(span(780, 900), 45).unwrap(),
        vec![span(780, 840)]
    );
}

#[test]
fn invalid_events_and_broken_snapshot_do_not_replace_saved_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    for invalid in [
        entry(Kind::Appointment, span(600, 600)),
        entry(Kind::Hold, span(1440, 1441)),
        entry(Kind::AllDay, span(540, 600)),
    ] {
        assert!(store.add(0, invalid).is_err());
    }
    let mut empty = entry(Kind::Appointment, span(1, 2));
    empty.title = " ".into();
    assert!(store.add(0, empty).is_err());
    store.add(0, entry(Kind::AllDay, span(0, 1440))).unwrap();
    assert!(store
        .day(date())
        .openings(span(540, 1020), 45)
        .unwrap()
        .is_empty());
    drop(store);
    let file = dir.path().join("calendar.json");
    std::fs::write(&file, b"bad snapshot").unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(std::fs::read(file).unwrap(), b"bad snapshot");
}
