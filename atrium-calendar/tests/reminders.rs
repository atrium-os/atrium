use atrium_calendar::{store::Store, EditScope, Entry, Kind, Span};
use chrono::{Days, TimeZone, Utc};
use uuid::Uuid;

#[test]
fn wake_groups_receipts_and_only_a_continuous_fresh_trigger_can_leave_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.sync_device(chrono_tz::UTC).unwrap();
    let day = Utc::now().date_naive() + Days::new(2);
    let time = |hh, mm, ss| Utc.from_utc_datetime(&day.and_hms_opt(hh, mm, ss).unwrap());
    let mut ids = Vec::new();
    for i in 0..21 {
        let (start, end) = match i {
            18 => (546, 565),
            19 => (545, 555),
            20 => (547, 568),
            _ => (540, 542),
        };
        let id = Uuid::new_v4();
        let row = Entry {
            id,
            date: day,
            title: format!("Meeting {i}"),
            kind: Kind::Appointment,
            span: Span { start, end },
            excluded: vec![],
            zone: None,
            source: None,
            import_hash: None,
            invitation: None,
        };
        s.add(s.day(day).revision, row).unwrap();
        s.set_reminder(s.day(day).revision, id, day, true).unwrap();
        ids.push(id);
    }
    s.change(
        s.day(day).revision,
        ids[19],
        day,
        EditScope::ThisEvent,
        None,
    )
    .unwrap();
    s.tick_reminders(time(8, 0, 0)).unwrap();
    let before = s.poll_reminders(None, time(8, 0, 0).timestamp());
    let wake = time(9, 6, 2);
    s.tick_reminders(wake).unwrap();
    let missed = s.poll_reminders(Some(before.token), wake.timestamp());
    assert_eq!(
        (
            missed.review.upcoming,
            missed.review.history,
            missed.fresh_due
        ),
        (1, 18, None)
    );
    assert_eq!(missed.review.items[0].title, "Meeting 18");
    for _ in 0..2 {
        s.tick_reminders(wake).unwrap();
        assert_eq!(
            s.poll_reminders(None, wake.timestamp()).review,
            missed.review
        );
    }
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.tick_reminders(time(9, 6, 58)).unwrap();
    assert_eq!(s.reminder_review(time(9, 6, 58).timestamp()), missed.review);
    let lease = s.poll_reminders(None, time(9, 6, 58).timestamp()).token;
    s.tick_reminders(time(9, 7, 0)).unwrap();
    let f = s.poll_reminders(Some(lease), time(9, 7, 0).timestamp());
    assert_eq!(f.fresh_due, Some(time(9, 7, 0).timestamp()));
    assert_eq!(f.review.items.len(), 20);
    assert_eq!(
        s.poll_reminders(Some(f.token), time(9, 7, 0).timestamp())
            .fresh_due,
        None
    );
    drop(s);
    let mut s = Store::open(dir.path()).unwrap();
    s.tick_reminders(time(9, 7, 1)).unwrap();
    assert_eq!(
        s.poll_reminders(Some(f.token), time(9, 7, 1).timestamp())
            .fresh_due,
        None
    );
}

#[test]
fn a_reminder_tracks_its_original_weekly_slot_through_a_local_move() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.sync_device(chrono_tz::UTC).unwrap();
    let first = Utc::now().date_naive() + Days::new(1);
    let slot = first + Days::new(7);
    let moved = slot + Days::new(2);
    let id = Uuid::new_v4();
    let row = Entry {
        id,
        date: first,
        span: Span {
            start: 600,
            end: 630,
        },
        title: "Weekly".into(),
        kind: Kind::Weekly,
        excluded: vec![],
        zone: None,
        source: None,
        import_hash: None,
        invitation: None,
    };
    s.add(0, row.clone()).unwrap();
    s.set_reminder(1, id, slot, true).unwrap();
    let mut change = row.fields_on(moved);
    change.date = moved;
    s.change(2, id, slot, EditScope::ThisEvent, Some(change))
        .unwrap();
    let old = slot.and_hms_opt(10, 0, 0).unwrap().and_utc();
    s.tick_reminders(old).unwrap();
    assert!(s.reminder_review(old.timestamp()).items.is_empty());
    let new = moved.and_hms_opt(10, 0, 0).unwrap().and_utc();
    s.tick_reminders(new).unwrap();
    assert_eq!(s.reminder_review(new.timestamp()).items.len(), 1);
    let occurrence = s.day(moved).entries[0].id;
    s.set_reminder(s.day(moved).revision, occurrence, moved, false)
        .unwrap();
    assert!(s.reminder_review(new.timestamp()).items.is_empty());
}

#[test]
fn a_failed_device_zone_write_suspends_unpinned_triggers_until_the_clock_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.observe_device(Some(chrono_tz::UTC)).unwrap();
    let snapshot = dir.path().join("calendar.json");
    std::fs::create_dir(&snapshot).unwrap();
    assert!(s.observe_device(Some(chrono_tz::Europe::Paris)).is_err());
    assert!(!s.clock_available());
    std::fs::remove_dir(&snapshot).unwrap();
    s.observe_device(Some(chrono_tz::Europe::Paris)).unwrap();
    assert!(s.clock_available());
    assert_eq!(s.viewing_zone(), chrono_tz::Europe::Paris);
    let rev = s.day(Utc::now().date_naive()).revision;
    s.set_viewing_zone(rev, Some(chrono_tz::UTC)).unwrap();
    assert!(s.observe_device(None).is_err());
    assert!(s.clock_available());
    assert_eq!(s.viewing_zone(), chrono_tz::UTC);
}

#[test]
fn a_failed_receipt_write_leaves_no_live_offer_and_recovers_silently() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let day = Utc::now().date_naive() + Days::new(1);
    let due = day.and_hms_opt(12, 0, 0).unwrap().and_utc();
    let id = Uuid::new_v4();
    store
        .add(
            0,
            Entry {
                id,
                date: day,
                title: "Receipt retry".into(),
                kind: Kind::Appointment,
                span: Span {
                    start: 720,
                    end: 730,
                },
                excluded: vec![],
                source: None,
                zone: None,
                invitation: None,
                import_hash: None,
            },
        )
        .unwrap();
    store.set_reminder(1, id, day, true).unwrap();
    let prior = due - chrono::Duration::seconds(1);
    store.tick_reminders(prior).unwrap();
    let lease = store.poll_reminders(None, prior.timestamp()).token;

    let path = dir.path().join("calendar.json");
    let saved = dir.path().join("previous");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(store.tick_reminders(due).is_err());
    let poll = store.poll_reminders(Some(lease), due.timestamp());
    assert!(poll.fresh_due.is_none() && poll.review.items.is_empty());

    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(saved, &path).unwrap();
    let retry = due + chrono::Duration::seconds(1);
    store.tick_reminders(retry).unwrap();
    let poll = store.poll_reminders(Some(poll.token), retry.timestamp());
    assert!(poll.fresh_due.is_none());
    assert_eq!(poll.review.upcoming, 1);
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.reminder_review(retry.timestamp()), poll.review);
}
