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
            .map(|e| &*e.title)
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
