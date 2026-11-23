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
        excluded: Vec::new(),
        zone: None,
        source: None,
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
            clock_gap: None,
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

#[test]
fn moving_one_weekly_review_preserves_its_identity_gaps_and_siblings_after_restart() {
    use atrium_calendar::{CalendarView, EditScope::ThisEvent};
    use chrono::Days;
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let weekly = entry(Kind::Weekly, span(840, 900));
    let original = date() + Days::new(7);
    let next = original + Days::new(7);
    let destination = next; // Both occurrences now share a day.
    store.add(0, weekly.clone()).unwrap();
    let mut fields = weekly.fields_on(original);
    store
        .change(1, weekly.id, original, ThisEvent, Some(fields.clone()))
        .unwrap();
    assert_eq!(store.day(original).revision, 1); // Saving untouched fields keeps the series intact.
    fields.date = destination;
    fields.span = span(720, 765);
    store
        .change(1, weekly.id, original, ThisEvent, Some(fields.clone()))
        .unwrap();
    drop(store);

    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.day(original).openings(span(840, 900), 45).unwrap(),
        [span(840, 900)]
    );
    let both = store.day(destination);
    assert_eq!(both.entries.len(), 2);
    let moved = &both.entries[0];
    assert_eq!(moved.fields_on(destination), fields);
    assert_eq!(
        moved.kind,
        Kind::Occurrence {
            series: weekly.id,
            original
        }
    );
    assert_eq!(both.entries[1].id, weekly.id);
    assert_eq!(both.entries[1].span, span(840, 900));
    assert!(both.openings(span(720, 765), 45).unwrap().is_empty());
    let month = store.page(next, Some(CalendarView::Month));
    assert_eq!(month.on(original).count(), 0);
    assert_eq!(month.on(next).count(), 2);

    let mut hold = entry(Kind::Hold, span(720, 765));
    hold.date = destination;
    assert!(store.add(2, hold.clone()).is_err());
    hold.date = original;
    hold.span = span(840, 900);
    store.add(2, hold).unwrap();
    assert!(store
        .change(2, moved.id, destination, ThisEvent, None)
        .is_err());
    let again = original - Days::new(1);
    fields.date = again;
    store
        .change(3, moved.id, destination, ThisEvent, Some(fields))
        .unwrap();
    let again_day = store.day(again);
    assert_eq!(again_day.entries[0].id, moved.id);
    assert_eq!(again_day.entries[0].kind, moved.kind);
    store.change(4, moved.id, again, ThisEvent, None).unwrap();
    assert!(store.day(again).entries.is_empty());
    assert_eq!(store.day(original).entries.len(), 1);
    assert_eq!(store.day(original).entries[0].kind, Kind::Hold);
    assert_eq!(store.day(next).entries[0].id, weekly.id);
}

#[test]
fn changes_to_whole_series_keep_exceptions_and_deletion_cleans_them_up() {
    use atrium_calendar::EditScope::{ThisEvent, WholeSeries};
    use chrono::Days;
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let weekly = entry(Kind::Weekly, span(600, 645));
    let original = weekly.date;
    let next = original + Days::new(7);
    let mut exception = weekly.fields_on(original - Days::new(35));
    exception.title = "Offsite".into();
    store.add(0, weekly.clone()).unwrap();
    store
        .change(1, weekly.id, original, ThisEvent, Some(exception.clone()))
        .unwrap();
    store.change(2, weekly.id, next, ThisEvent, None).unwrap();
    let moved_page = store.page(exception.date, None);
    let moved = moved_page.entries[0].id;
    assert_eq!(moved_page.series[&weekly.id], weekly.fields_on(original));
    let mut update = weekly.fields_on(original);
    update.span = span(750, 800);
    update.title = "Design crit".into();
    store
        .change(3, moved, exception.date, WholeSeries, Some(update.clone()))
        .unwrap();
    assert_eq!(
        store.day(exception.date).entries[0].fields_on(exception.date),
        exception
    );
    assert!(store.day(original).entries.is_empty() && store.day(next).entries.is_empty());
    assert_eq!(store.day(next + Days::new(7)).entries[0].span, update.span);
    store
        .change(4, moved, exception.date, WholeSeries, None)
        .unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.day(exception.date).entries.is_empty());
    assert!(store.day(next + Days::new(7)).entries.is_empty());
}

#[test]
fn ordinary_edits_validate_fields_and_reject_invalid_occurrence_targets() {
    use atrium_calendar::EditScope::{ThisEvent, WholeSeries};
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let event = entry(Kind::AllDay, span(0, 1440));
    store.add(0, event.clone()).unwrap();
    let mut fields = event.fields_on(date().succ_opt().unwrap());
    assert!(store
        .change(1, event.id, fields.date, ThisEvent, None)
        .is_err());
    assert!(store
        .change(1, event.id, date(), WholeSeries, None)
        .is_err());
    fields.span = span(540, 600);
    assert!(store
        .change(1, event.id, date(), ThisEvent, Some(fields.clone()))
        .is_err());
    fields.span = event.span;
    fields.title = "Family visit".into();
    store
        .change(1, event.id, date(), ThisEvent, Some(fields.clone()))
        .unwrap();
    assert!(store.day(date()).entries.is_empty());
    let saved = store.day(fields.date);
    assert_eq!(saved.entries[0].id, event.id);
    assert_eq!(saved.entries[0].fields_on(fields.date), fields);
    store
        .change(2, event.id, fields.date, ThisEvent, None)
        .unwrap();
    assert!(store.day(fields.date).entries.is_empty());
    let mut forged = event.clone();
    forged.kind = Kind::Occurrence {
        series: event.id,
        original: date(),
    };
    assert!(store.add(3, forged).is_err());
    let mut weekly = entry(Kind::Weekly, span(600, 630));
    weekly.excluded.push(date());
    assert!(store.add(3, weekly).is_err());
}

#[test]
fn exception_limit_and_invalid_series_links_preserve_the_last_snapshot() {
    use atrium_calendar::EditScope::ThisEvent;
    use chrono::Days;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("calendar.json");
    let mut weekly = entry(Kind::Weekly, span(840, 885));
    weekly.excluded = (0..1024).map(|n| date() + Days::new(7 * n)).collect();
    let snapshot = |entries: &[Entry]| {
        serde_json::to_vec(&serde_json::json!({"version": 2, "revision": 4, "entries": entries}))
            .unwrap()
    };
    let bytes = snapshot(&[weekly.clone()]);
    std::fs::write(&path, &bytes).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let after = date() + Days::new(7 * 1024);
    assert_eq!(store.day(after).entries[0].id, weekly.id);
    assert!(store
        .change(4, weekly.id, after, ThisEvent, None)
        .unwrap_err()
        .contains("limit"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(store);

    let orphan = entry(
        Kind::Occurrence {
            series: Uuid::new_v4(),
            original: date(),
        },
        span(720, 765),
    );
    let damaged = snapshot(&[weekly, orphan]);
    std::fs::write(&path, &damaged).unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), damaged);
}
