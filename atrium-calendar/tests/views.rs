use atrium_calendar::{store::Store, CalendarView, Entry, Kind, Span};
use chrono::{Days, NaiveDate};
use uuid::Uuid;

fn date(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

fn add(store: &mut Store, date: NaiveDate, kind: Kind, title: &str, start: u16, end: u16) {
    let revision = store.day(date).revision;
    store
        .add(
            revision,
            Entry {
                excluded: Vec::new(),
                zone: None,
                source: None,
                id: Uuid::new_v4(),
                date,
                kind,
                title: title.into(),
                span: Span { start, end },
            },
        )
        .unwrap();
}

#[test]
fn calendar_boundaries_and_month_navigation() {
    use CalendarView::{Month, Week};
    let sunday = date("2027-01-03");
    let week: Vec<_> = Week.dates(sunday).collect();
    assert_eq!(week.len(), 7);
    assert_eq!((week[0], week[6]), (date("2026-12-28"), sunday));
    assert_eq!(
        Month.dates(date("2028-02-15")).last(),
        Some(date("2028-02-29"))
    );
    assert_eq!(
        Month.adjacent(date("2027-03-31"), false),
        Some(date("2027-02-28"))
    );
    assert_eq!(
        Month.adjacent(date("2026-12-31"), true),
        Some(date("2027-01-31"))
    );
    for view in CalendarView::ALL {
        assert_eq!(view.dates(NaiveDate::MIN).next(), Some(NaiveDate::MIN));
        assert_eq!(view.dates(NaiveDate::MAX).last(), Some(NaiveDate::MAX));
        assert!(view.adjacent(NaiveDate::MIN, false).is_none());
        assert!(view.adjacent(NaiveDate::MAX, true).is_none());
    }
}

#[test]
fn week_and_month_project_repeating_series_once_and_distinguish_real_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let monday = date("2028-02-07");
    add(
        &mut store,
        monday - Days::new(14),
        Kind::Weekly,
        "Design",
        540,
        550,
    );
    add(&mut store, monday, Kind::Appointment, "Handoff", 550, 560);
    add(&mut store, monday, Kind::Hold, "Preparation", 580, 620);
    add(
        &mut store,
        monday,
        Kind::Appointment,
        "Moved review",
        610,
        630,
    );
    add(
        &mut store,
        date("2028-02-29"),
        Kind::AllDay,
        "Travel",
        0,
        1440,
    );
    add(
        &mut store,
        date("2028-03-01"),
        Kind::Weekly,
        "Future",
        540,
        600,
    );

    let week = store.page(monday, Some(CalendarView::Week));
    assert_eq!(week.entries.len(), 4);
    let occupancy = week.half_hours(monday);
    assert_eq!(
        (occupancy[18], occupancy[19], occupancy[20], occupancy[21]),
        (1, 1, 2, 0)
    );
    let month = store.page(monday, Some(CalendarView::Month));
    assert_eq!(month.entries.len(), 5);
    assert_eq!(
        month
            .on(date("2028-02-21"))
            .map(|e| e.title)
            .collect::<Vec<_>>(),
        ["Design"]
    );
    assert_eq!(month.half_hours(date("2028-02-29")), [1; 48]);
}

#[test]
fn opening_views_survive_restart_and_old_snapshots_while_browsing_is_read_only() {
    let work = tempfile::tempdir().unwrap();
    let personal = tempfile::tempdir().unwrap();
    let path = work.path().join("calendar.json");
    let old = br#"{"version":1,"revision":9,"entries":[]}"#;
    std::fs::write(&path, old).unwrap();
    let mut w = Store::open(work.path()).unwrap();
    let mut p = Store::open(personal.path()).unwrap();
    let d = date("2026-11-23");
    assert_eq!(w.page(d, None).view, CalendarView::Today);
    assert_eq!(std::fs::read(&path).unwrap(), old);
    w.set_opening_view(9, CalendarView::Week).unwrap();
    assert_eq!(p.page(d, None).view, CalendarView::Today);
    p.set_opening_view(0, CalendarView::Month).unwrap();
    assert!(w.set_opening_view(9, CalendarView::Month).is_err());
    w.set_opening_view(10, CalendarView::Week).unwrap();
    assert_eq!(w.page(d, None).revision, 10);
    add(&mut w, d, Kind::Weekly, "Review", 600, 630);
    let before = std::fs::read(&path).unwrap();
    for store in [&w, &p] {
        assert_eq!(
            store.page(d, Some(CalendarView::Today)).view,
            CalendarView::Today
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop((w, p));
    let w = Store::open(work.path()).unwrap().page(d, None);
    let p = Store::open(personal.path()).unwrap().page(d, None);
    assert_eq!((w.view, p.view), (CalendarView::Week, CalendarView::Month));
    assert_eq!(w.entries[0].title, "Review");
    assert!(p.entries.is_empty());
}

#[test]
fn search_uses_actual_occurrences_and_revalidates_results() {
    use atrium_calendar::{EditScope::ThisEvent, SearchQuery};
    let work = tempfile::tempdir().unwrap();
    let personal = tempfile::tempdir().unwrap();
    let mut store = Store::open(work.path()).unwrap();
    let mut other = Store::open(personal.path()).unwrap();
    let first = date("2026-11-02");
    let moved = date("2026-11-09");
    add(
        &mut store,
        first,
        Kind::Weekly,
        "Café design review",
        840,
        900,
    );
    add(
        &mut other,
        first,
        Kind::AllDay,
        "Café design review",
        0,
        1440,
    );
    let series = store.day(first).entries.remove(0);
    let mut changed = series.fields_on(moved);
    changed.span = Span {
        start: 600,
        end: 645,
    };
    store
        .change(1, series.id, first, ThisEvent, Some(changed))
        .unwrap();
    store
        .change(2, series.id, moved + Days::new(7), ThisEvent, None)
        .unwrap();
    let query = SearchQuery {
        text: " REVIEW   CAFÉ ".into(),
        start: first,
        end: moved + Days::new(7),
    };
    let snapshot = std::fs::read(work.path().join("calendar.json")).unwrap();
    let found = store.search(&query, 0, None).unwrap();
    let personal_hit = other.search(&query, 0, None).unwrap();
    assert_eq!(personal_hit.total, 1);
    assert_eq!(personal_hit.hits[0].date, first);
    assert_eq!(personal_hit.hits[0].kind, Kind::AllDay);
    assert_eq!(found.total, 2);
    let hit = &found.hits[0];
    assert_eq!((hit.date, hit.span.start), (moved, 600));
    assert_eq!(
        hit.kind,
        Kind::Occurrence {
            series: series.id,
            original: first
        }
    );
    assert_eq!(found.hits[1].id, series.id);
    assert_eq!(found.hits[1].date, moved);
    let day = store
        .open_search_hit(found.revision, hit.id, hit.date)
        .unwrap();
    assert_eq!(day.entries.len(), 2);
    assert_eq!(
        std::fs::read(work.path().join("calendar.json")).unwrap(),
        snapshot
    );
    assert!(store
        .open_search_hit(found.revision, hit.id, first)
        .is_err());
    store
        .change(found.revision, hit.id, moved, ThisEvent, None)
        .unwrap();
    assert!(store
        .open_search_hit(found.revision, hit.id, moved)
        .is_err());
    drop(store);
    let store = Store::open(work.path()).unwrap();
    let results = store.search(&query, 0, None).unwrap();
    assert_eq!(results.total, 1);
    assert_eq!(results.hits[0].id, series.id);
}

#[test]
fn search_pages_are_ordered_bounded_and_revision_checked() {
    use atrium_calendar::{SearchQuery, SEARCH_PAGE_SIZE};
    let dir = tempfile::tempdir().unwrap();
    let start = date("2028-01-01");
    // A v1 snapshot with enough old series to exceed the annual expansion budget.
    let entries: Vec<_> = (0..200)
        .map(|i| Entry {
            id: Uuid::from_u128(i + 1),
            date: start - Days::new(7 * 400),
            kind: Kind::Weekly,
            span: Span {
                start: 480,
                end: 540,
            },
            title: "Weekly team".into(),
            excluded: Vec::new(),
            zone: None,
            source: None,
        })
        .rev()
        .collect();
    std::fs::write(
        dir.path().join("calendar.json"),
        serde_json::to_vec(&serde_json::json!({"version":1,"revision":3,"entries":entries}))
            .unwrap(),
    )
    .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut q = SearchQuery {
        text: "team".into(),
        start,
        end: start,
    };
    let first = store.search(&q, 0, None).unwrap();
    assert_eq!((first.hits.len(), first.total), (SEARCH_PAGE_SIZE, 200));
    assert_eq!(first.hits[0].id, Uuid::from_u128(1));
    let page = store.search(&q, SEARCH_PAGE_SIZE, Some(3)).unwrap();
    assert_eq!(
        page.hits[0].id,
        Uuid::from_u128(SEARCH_PAGE_SIZE as u128 + 1)
    );
    assert!(store.search(&q, 200, Some(3)).is_err());
    assert!(store.search(&q, 5, None).is_err());
    store.set_opening_view(3, CalendarView::Week).unwrap();
    assert!(store.search(&q, 5, Some(3)).is_err());
    assert!(store.search(&q, 0, Some(3)).is_err());
    q.end = start + Days::new(365);
    assert!(store.search(&q, 0, None).unwrap_err().contains("Too many"));
    q.text = "nothing matches".into();
    assert_eq!(store.search(&q, 0, None).unwrap().total, 0);
    q.end = start + Days::new(366);
    assert!(store.search(&q, 0, None).is_err());
    q.end = start.pred_opt().unwrap();
    assert!(store.search(&q, 0, None).is_err());
    q.start = NaiveDate::MAX;
    q.end = NaiveDate::MAX;
    q.text = "team".into();
    assert!(store.search(&q, 0, None).is_ok());
    for invalid in ["  ".to_owned(), "x".repeat(161), "a\nb".into()] {
        q.text = invalid;
        assert!(store.search(&q, 0, None).is_err());
    }
}

fn zoned(title: &str, day: &str, start: u16, end: u16, name: chrono_tz::Tz, kind: Kind) -> Entry {
    Entry {
        id: Uuid::new_v4(),
        title: title.into(),
        date: date(day),
        span: Span { start, end },
        kind,
        zone: Some(atrium_calendar::Zone {
            name,
            start_fold: None,
            end_fold: None,
        }),
        source: None,
        excluded: vec![],
    }
}

#[test]
fn gaps_and_both_fold_offsets_validate_before_publication() {
    use atrium_calendar::Fold::{Earlier, Later};
    use chrono_tz::America::New_York;
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut event = zoned(
        "Night handover",
        "2026-03-08",
        130,
        160,
        New_York,
        Kind::Appointment,
    );
    assert!(store
        .add(0, event.clone())
        .unwrap_err()
        .contains("does not exist"));
    assert!(!dir.path().join("calendar.json").exists());
    event.date = date("2026-11-01");
    event.span = Span {
        start: 70,
        end: 100,
    };
    assert!(store.add(0, event.clone()).unwrap_err().contains("repeats"));
    event.zone.as_mut().unwrap().start_fold = Some(Earlier);
    assert!(store.add(0, event.clone()).unwrap_err().contains("repeats"));
    event.zone.as_mut().unwrap().end_fold = Some(Earlier);
    store.add(0, event.clone()).unwrap();
    event.id = Uuid::new_v4();
    event.zone.as_mut().unwrap().start_fold = Some(Later);
    let path = dir.path().join("calendar.json");
    let before = std::fs::read(&path).unwrap();
    assert!(store.add(1, event.clone()).unwrap_err().contains("after"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    event.zone.as_mut().unwrap().end_fold = Some(Later);
    store.add(1, event.clone()).unwrap();
    event.id = Uuid::new_v4();
    event.zone.as_mut().unwrap().start_fold = Some(Earlier);
    event.span = Span {
        start: 110,
        end: 70,
    }; // 01:50 EDT to 01:10 EST is twenty minutes.
    store.add(2, event.clone()).unwrap();
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    let day = store.day(event.date);
    let spans: Vec<_> = day
        .entries
        .iter()
        .map(|e| (e.span.start, e.span.end))
        .collect();
    assert_eq!(spans, [(310, 340), (350, 370), (370, 400)]);
    assert_eq!(
        day.entries[1].fields_on(event.date),
        event.fields_on(event.date)
    );
    assert!(store.add(3, day.entries[1].clone()).is_err());
}

#[test]
fn weekly_meetings_follow_the_source_clock_across_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let zone = chrono_tz::America::New_York;
    let event = zoned("Monday review", "2026-03-02", 540, 600, zone, Kind::Weekly);
    store.add(0, event).unwrap();
    for (d, start) in [("2026-03-02", 840), ("2026-03-09", 780)] {
        let day = date(d);
        for view in CalendarView::ALL {
            let row = store.page(day, Some(view)).on(day).next().unwrap();
            assert_eq!(
                row.span,
                Span {
                    start,
                    end: start + 60
                }
            );
            assert_eq!(row.fields_on(day).span.start, 540);
        }
        let mut hold = zoned("Hold", d, start, start + 30, zone, Kind::Hold);
        hold.zone = None;
        assert!(store.add(1, hold).unwrap_err().contains("busy"));
    }
    let event = zoned("Early Sunday", "2026-03-01", 130, 160, zone, Kind::Weekly);
    store.add(1, event).unwrap();
    assert!(store.day(date("2026-03-08")).entries.is_empty());
    assert_eq!(store.day(date("2026-03-15")).entries[0].span.start, 370);
    let event = zoned("Night duty", "2026-10-25", 70, 100, zone, Kind::Weekly);
    store.add(2, event).unwrap();
    assert_eq!(store.day(date("2026-11-01")).entries[0].span.start, 310);
}

#[test]
fn midnight_segments_share_a_source_occurrence_for_search_and_editing() {
    use atrium_calendar::{EditScope, SearchQuery};
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let event = zoned(
        "Evening review",
        "2026-12-31",
        1110,
        1170,
        chrono_tz::America::New_York,
        Kind::Weekly,
    );
    store.add(0, event.clone()).unwrap();
    let tail = date("2027-01-01");
    let day = store.day(tail);
    assert_eq!(day.entries[0].span, Span { start: 0, end: 30 });
    assert_eq!(
        store
            .page(tail, Some(CalendarView::Month))
            .on(tail)
            .next()
            .unwrap()
            .source
            .unwrap()
            .date,
        event.date
    );
    let mut query = SearchQuery {
        text: "review".into(),
        start: event.date,
        end: tail,
    };
    let results = store.search(&query, 0, None).unwrap();
    assert_eq!(results.total, 1);
    assert_eq!(
        results.hits[0].span,
        Span {
            start: 1410,
            end: 1440
        }
    );
    query.start = tail;
    let hit = &store.search(&query, 0, None).unwrap().hits[0];
    assert_eq!(hit.date, tail);
    assert_eq!(hit.source_zone, Some(chrono_tz::America::New_York));
    let selected = store
        .open_search_hit(1, hit.id, hit.date)
        .unwrap()
        .on(tail)
        .next()
        .unwrap();
    let mut edit = selected.fields_on(tail);
    assert_eq!(edit, event.fields_on(event.date));
    edit.zone.as_mut().unwrap().name = chrono_tz::Asia::Kolkata;
    edit.date = tail;
    edit.span = Span { start: 30, end: 90 };
    store
        .change(1, event.id, tail, EditScope::ThisEvent, Some(edit))
        .unwrap();
    assert!(store.day(tail).entries.is_empty());
    let moved = store.day(event.date).entries.remove(0);
    assert_eq!(
        moved.span,
        Span {
            start: 1140,
            end: 1200
        }
    );
    assert_eq!(
        moved.kind,
        Kind::Occurrence {
            series: event.id,
            original: event.date
        }
    );
    assert_eq!(moved.source.unwrap().date, tail);
    store
        .change(2, moved.id, event.date, EditScope::ThisEvent, None)
        .unwrap();
    assert!(store.day(event.date).entries.is_empty());
    assert_eq!(store.day(tail + Days::new(7)).entries[0].id, event.id);
}

#[test]
fn zoned_write_preserves_version_two_floating_and_all_day_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("calendar.json");
    let legacy = br#"{"version":2,"revision":8,"opening_view":"Week","entries":[
        {"id":"00000000-0000-0000-0000-000000000001","date":"2026-03-09","title":"Desk","span":{"start":540,"end":600},"kind":"Appointment"},
        {"id":"00000000-0000-0000-0000-000000000002","date":"2026-03-09","title":"Travel","span":{"start":0,"end":1440},"kind":"AllDay"}
    ]}"#;
    std::fs::write(&path, legacy).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let day = date("2026-03-09");
    assert_eq!(store.page(day, None).view, CalendarView::Week);
    assert_eq!(std::fs::read(&path).unwrap(), legacy);
    store
        .add(
            8,
            zoned(
                "Zoned review",
                "2026-03-09",
                540,
                600,
                chrono_tz::America::New_York,
                Kind::Appointment,
            ),
        )
        .unwrap();
    drop(store);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(snapshot["version"], 4);
    let page = Store::open(dir.path()).unwrap().page(day, None);
    assert_eq!((page.revision, page.view), (9, CalendarView::Week));
    let rows: Vec<_> = page
        .on(day)
        .map(|e| (e.title, e.span, e.zone.is_some()))
        .collect();
    assert_eq!(
        rows,
        [
            (
                "Travel".into(),
                Span {
                    start: 0,
                    end: 1440
                },
                false
            ),
            (
                "Desk".into(),
                Span {
                    start: 540,
                    end: 600
                },
                false
            ),
            (
                "Zoned review".into(),
                Span {
                    start: 780,
                    end: 840
                },
                true
            )
        ]
    );
}

#[test]
fn following_device_and_pinning_are_persona_local_and_survive_restart() {
    use atrium_calendar::SearchQuery;
    use chrono_tz::{America::Los_Angeles, Asia::Kolkata, Pacific::Auckland, UTC};
    let work = tempfile::tempdir().unwrap();
    let personal = tempfile::tempdir().unwrap();
    let mut w = Store::open(work.path()).unwrap();
    let mut p = Store::open(personal.path()).unwrap();
    for store in [&mut w, &mut p] {
        store.sync_device(Los_Angeles).unwrap();
        store
            .add(
                0,
                zoned("Handover", "2026-03-02", 1350, 1410, UTC, Kind::Weekly),
            )
            .unwrap();
    }
    w.set_viewing_zone(1, Some(Kolkata)).unwrap();
    let local = date("2026-03-02");
    let next = local.succ_opt().unwrap();
    assert_eq!(p.day(local).entries[0].span.start, 870);
    assert_eq!(w.day(next).entries[0].span.start, 240);
    let stale = p.day(local).revision;
    p.sync_device(Auckland).unwrap();
    w.sync_device(Auckland).unwrap();
    assert_eq!(w.day(next).revision, 2);
    assert!(p.day(local).entries.is_empty());
    assert!(p.set_viewing_zone(stale, Some(UTC)).is_err());
    for view in CalendarView::ALL {
        let page = p.page(next, Some(view));
        assert_eq!(page.viewing_zone, Auckland);
        assert_eq!(page.pinned_zone, None);
        let row = page.on(next).next().unwrap();
        assert_eq!(
            row.span,
            Span {
                start: 690,
                end: 750
            }
        );
        assert_eq!(row.source.unwrap().date, local);
        assert_eq!(row.source.unwrap().span.start, 1350);
    }
    let query = SearchQuery {
        text: "handover".into(),
        start: local,
        end: next,
    };
    let hit = &p.search(&query, 0, None).unwrap().hits[0];
    assert_eq!((hit.date, hit.span.start), (next, 690));
    assert!(p.day(next).openings(hit.span, 45).unwrap().is_empty());
    let mut hold = zoned("Hold", "2026-03-03", 700, 745, UTC, Kind::Hold);
    hold.zone = None;
    assert!(p.add(2, hold).is_err());
    drop(w);
    let mut w = Store::open(work.path()).unwrap();
    w.sync_device(Auckland).unwrap();
    assert_eq!(w.viewing_zone(), Kolkata);
    w.set_viewing_zone(2, None).unwrap();
    assert_eq!(w.day(next).entries[0].span, hit.span);
}

#[test]
fn viewing_zone_repeated_hour_has_two_offsets_and_no_false_overlap() {
    use chrono_tz::{America::New_York, UTC};
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.sync_device(New_York).unwrap();
    let day = date("2026-11-01");
    for (i, (name, start, end)) in [
        ("First", 345, 355),
        ("Second", 405, 415),
        ("Bridge", 355, 365),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .add(
                i as u64,
                zoned(name, "2026-11-01", start, end, UTC, Kind::Appointment),
            )
            .unwrap();
    }
    let page = store.page(day, Some(CalendarView::Today));
    let titles = page.on(day).map(|e| e.title).collect::<Vec<_>>();
    assert_eq!(titles, ["First", "Bridge", "Second"]);
    assert_eq!(page.half_hours(day)[2..4], [1, 1]);
    let bridge = page.entries.iter().find(|e| e.title == "Bridge").unwrap();
    assert_eq!(
        bridge.source.unwrap().clock_label(),
        "01:55–02:00 -04:00 / 01:00–01:05 -05:00"
    );
    let spring = date("2026-03-08");
    let skipped = Span {
        start: 120,
        end: 180,
    };
    assert!(store.day(spring).openings(skipped, 15).unwrap().is_empty());
    let mut hold = zoned("Hold", "2026-03-08", 125, 160, UTC, Kind::Hold);
    hold.zone = None;
    assert!(store.add(3, hold).is_err());
}

#[test]
fn meetings_crossing_the_date_line_use_the_same_day_in_search_and_browse() {
    use chrono_tz::{Etc::GMTPlus12, Pacific::Kiritimati};
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.sync_device(GMTPlus12).unwrap();
    let event = zoned("Date line", "2027-01-02", 10, 30, Kiritimati, Kind::Weekly);
    store.add(0, event).unwrap();
    let day = date("2026-12-31");
    let page = store.page(day, Some(CalendarView::Month));
    assert_eq!(
        page.on(day).next().unwrap().span,
        Span {
            start: 1330,
            end: 1350
        }
    );
    let query = atrium_calendar::SearchQuery {
        text: "Date line".into(),
        start: day,
        end: day,
    };
    assert_eq!(store.search(&query, 0, None).unwrap().total, 1);
    let id = page.on(day).next().unwrap().id;
    store
        .change(1, id, day, atrium_calendar::EditScope::ThisEvent, None)
        .unwrap();
    assert_eq!(store.search(&query, 0, None).unwrap().total, 0);
    assert_eq!(store.day(day + Days::new(7)).entries.len(), 1);
}
