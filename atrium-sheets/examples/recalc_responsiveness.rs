//! Large native workbook fixture and live Sheets event-to-scene timing.
//! Build on the host and run on a private Atrium desktop.
//!
//! `recalc_responsiveness make FILE`
//! `recalc_responsiveness check FILE MULTIPLIER`
//! `recalc_responsiveness measure FILE`
//! `recalc_responsiveness visible FILE`

use std::fs::{self, File};
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::{Duration, Instant};

use atrium_sheets::app::{Sheets, Ui};
use atrium_sheets::doc::{Doc, RECALC_SLICE};
use officina_grid::{CellAddr, Value, Workbook};
use pergola::event::ScrollPhase;
use pergola::geom::Point;
use pergola::widgets::{GridSource, GridState, ScrollState};
use pergola::{App, Event, Key, KeyEventKind, Modifiers, Mutable, SizeSpec, Window, WindowDesc};

const ROWS: u32 = 1 << 20;
const FIRST: f64 = 2.0;
const LATEST: f64 = 7.0;
const TRIALS: usize = 3;

fn source(row: u32) -> f64 {
    f64::from(row % 999 + 1)
}

fn expected(row: u32, multiplier: f64) -> f64 {
    ((source(row) / 3.0 * 100.0).round() / 100.0) * multiplier
}

fn make(path: &Path) {
    let mut doc = Doc::new("responsive.osheet");
    doc.wb.set_value(CellAddr::new(0, 0, 0), Value::Num(1.0));
    for start in (0..ROWS).step_by(1 << 16) {
        let values: Vec<_> = (start..start + (1 << 16)).map(source).collect();
        doc.wb.set_numbers(0, 2, start, &values);
    }
    doc.wb.fill_formula(0, 1, 0, ROWS - 1, "=ROUND(C1/3,2)*$A$1").unwrap();
    doc.wb.fill_formula(0, 3, 0, ROWS - 1, "=B1+1").unwrap();
    doc.wb.recalc();
    verify(&doc.wb, 1.0);
    doc.write_native(File::create(path).unwrap()).unwrap();
}

fn verify(wb: &Workbook, multiplier: f64) {
    assert_eq!(wb.extent(0), (ROWS, 4));
    assert_eq!(wb.value(0, 0, 0), Value::Num(multiplier));
    let count = wb.groups().filter(|(_, g)| g.sheet == 0 && matches!(g.col, 1 | 3))
        .map(|(_, g)| g.r1 - g.r0 + 1).sum::<u32>();
    assert_eq!(count, 2 * ROWS);
    for row in 0..ROWS {
        let value = expected(row, multiplier);
        for (col, want) in [(1, value), (2, source(row)), (3, value + 1.0)] {
            let Value::Num(got) = wb.value(0, row, col) else { panic!("non-numeric at {row}/{col}") };
            assert!((got - want).abs() <= 1e-10, "row {row} col {col} expected {want} got {got}");
        }
    }
    assert!(!wb.recalc_pending());
}

fn key(key: Key, chars: &str, ctrl: bool) -> Event {
    Event::Key { kind: KeyEventKind::Down, key, chars: chars.into(), modifiers: Modifiers { ctrl, ..Default::default() } }
}

fn scroll(phase: ScrollPhase, dy: f32) -> Event {
    Event::Scroll { at: Some(Point::new(350.0, 300.0)), dx: 0.0, dy, modifiers: Modifiers::default(), phase }
}

#[derive(Default)]
struct Samples {
    idle_key: Vec<f64>,
    busy_key: Vec<f64>,
    idle_scroll: Vec<f64>,
    busy_scroll: Vec<f64>,
    commit: Vec<f64>,
    slice: Vec<f64>,
}

fn report(label: &str, values: &mut [f64]) {
    values.sort_by(f64::total_cmp);
    let p50 = if values.len() % 2 == 0 {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    };
    println!("{label}: n={} median={p50:.2} ms max={:.2} ms", values.len(), values[values.len() - 1]);
}

struct Driver {
    window: Window,
    app: App<Sheets>,
    sheets: Sheets,
    epoch: Instant,
}

impl Driver {
    fn new(doc: Doc) -> Self {
        let mut window = Window::open(&WindowDesc::new("Sheets responsiveness", SizeSpec::Fixed(1100, 720))).unwrap();
        let dirty = Arc::new(AtomicBool::new(true));
        let sheets = Sheets::new(
            doc,
            Mutable::with_dirty(ScrollState::default(), dirty.clone()),
            Mutable::with_dirty(GridState::default(), dirty.clone()),
            Mutable::with_dirty(Ui::default(), dirty.clone()),
            Mutable::with_dirty(0, dirty.clone()),
            (window.width, window.height),
        );
        sheets.doc.borrow_mut().recalc_budget = Some(RECALC_SLICE);
        let mut app = App::new_with_flag(sheets.clone(), dirty);
        window.paint(&mut app).unwrap();
        Self { window, app, sheets, epoch: Instant::now() }
    }

    fn dispatch(&mut self, event: Event) -> f64 {
        for _ in 0..64 {
            if self.window.connection().wait_event(Some(Duration::from_micros(1))).unwrap().is_none() {
                break;
            }
        }
        let start = Instant::now();
        self.app.set_time(self.epoch.elapsed().as_secs_f64() * 1000.0);
        self.app.handle_event(event);
        self.window.paint(&mut self.app).unwrap();
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        elapsed
    }

    fn turn(&mut self, samples: &mut Samples) {
        if !self.pending() { return; }
        let start = Instant::now();
        self.sheets.doc.borrow_mut().settle();
        samples.slice.push(start.elapsed().as_secs_f64() * 1000.0);
        self.sheets.changed();
    }

    fn pending(&self) -> bool {
        self.sheets.doc.borrow().wb.recalc_pending()
    }

    fn stale_tail(&self) -> bool {
        self.sheets.doc.borrow().wb.is_stale(0, ROWS - 1, 3)
    }

    fn type_digit(&mut self, digit: &str) -> f64 {
        let latency = self.dispatch(key(Key::Char, digit, false));
        assert_eq!(self.sheets.grid.get_cloned().edit.as_ref().map(|edit| edit.text()), Some(digit));
        assert!(self.window.last_paint.deltas > 0, "editor did not submit a scene change");
        latency
    }

    fn move_grid(&mut self) -> f64 {
        let before = self.sheets.scroll.get().physics.y;
        let latency = self.dispatch(scroll(ScrollPhase::Update, 120.0));
        assert!(self.sheets.scroll.get().physics.y > before, "scroll did not move the viewport");
        assert!(self.window.last_paint.deltas > 0, "scroll did not submit a scene change");
        latency
    }
}

fn measure(bytes: &[u8]) {
    let mut samples = Samples::default();
    println!("rows={ROWS} formulas={} bytes={} trials={TRIALS}", 2 * ROWS, bytes.len());
    for trial in 1..=TRIALS {
        let doc = Doc::from_native("responsive.osheet", bytes).unwrap();
        verify(&doc.wb, 1.0);
        println!("trial={trial} calculation_threads={}", doc.wb.threads);
        let mut driver = Driver::new(doc);
        let at = Point::new(50.0, 111.0);
        driver.dispatch(Event::PointerDown { at });
        driver.dispatch(Event::PointerUp { at });
        assert_eq!(driver.sheets.grid.get_cloned().cursor, (0, 0));

        samples.idle_key.push(driver.type_digit("2"));
        driver.dispatch(key(Key::Escape, "", false));
        driver.dispatch(scroll(ScrollPhase::Begin, 0.0));
        for _ in 0..3 { samples.idle_scroll.push(driver.move_grid()); }
        driver.dispatch(scroll(ScrollPhase::End, 0.0));
        driver.dispatch(key(Key::Home, "", true));
        assert_eq!(driver.sheets.grid.get_cloned().cursor, (0, 0));

        driver.type_digit("2");
        let commit1 = driver.dispatch(key(Key::Enter, "", false));
        samples.commit.push(commit1);
        assert_eq!(driver.sheets.doc.borrow().wb.value(0, 0, 0), Value::Num(FIRST));
        assert!(driver.pending() && driver.stale_tail(), "first edit must leave work pending");
        driver.turn(&mut samples);

        driver.dispatch(scroll(ScrollPhase::Begin, 0.0));
        for _ in 0..3 {
            assert!(driver.pending());
            samples.busy_scroll.push(driver.move_grid());
            driver.turn(&mut samples);
        }
        driver.dispatch(scroll(ScrollPhase::End, 0.0));
        driver.dispatch(key(Key::Home, "", true));
        assert_eq!(driver.sheets.grid.get_cloned().cursor, (0, 0));
        assert!(driver.pending() && driver.stale_tail());
        samples.busy_key.push(driver.type_digit("7"));
        driver.turn(&mut samples);
        assert!(driver.pending() && driver.stale_tail(), "second edit must supersede pending work");
        let latest = Instant::now();
        let commit2 = driver.dispatch(key(Key::Enter, "", false));
        samples.commit.push(commit2);
        assert_eq!(driver.sheets.doc.borrow().wb.value(0, 0, 0), Value::Num(LATEST));
        let mut passes = 0;
        while driver.pending() {
            assert!(latest.elapsed() < Duration::from_secs(60), "latest edit never settled");
            driver.turn(&mut samples);
            driver.window.paint(&mut driver.app).unwrap();
            passes += 1;
        }
        let settle_ms = latest.elapsed().as_secs_f64() * 1000.0;
        assert!(passes > 0);
        verify(&driver.sheets.doc.borrow().wb, LATEST);
        println!("trial={trial} first_commit_ms={commit1:.2} latest_commit_ms={commit2:.2} latest_settle_ms={settle_ms:.2} additional_slices={passes}");
    }
    report("idle typing", &mut samples.idle_key);
    report("pending typing", &mut samples.busy_key);
    report("idle scrolling", &mut samples.idle_scroll);
    report("pending scrolling", &mut samples.busy_scroll);
    report("committing edits", &mut samples.commit);
    report("background recalculation slices", &mut samples.slice);
}

fn measure_visible(bytes: &[u8]) {
    let doc = Doc::from_native("responsive.osheet", bytes).unwrap();
    verify(&doc.wb, 1.0);
    let mut driver = Driver::new(doc);
    let at = Point::new(50.0, 111.0);
    driver.dispatch(Event::PointerDown { at });
    driver.dispatch(Event::PointerUp { at });
    driver.type_digit("7");
    driver.dispatch(key(Key::Enter, "", false));
    let background = ROWS / 3;
    let stale = |driver: &Driver, row| driver.sheets.doc.borrow().wb.is_stale(0, row, 3);
    assert!(stale(&driver, background));
    for target in [ROWS / 2, ROWS * 3 / 4] {
        assert!(stale(&driver, target), "the target must be stale before scrolling");
        driver.dispatch(scroll(ScrollPhase::Begin, 0.0));
        let dy = driver.sheets.source.row_top(target) - driver.sheets.scroll.get().physics.y;
        driver.dispatch(scroll(ScrollPhase::Update, dy as f32));
        let top = driver.sheets.scroll.get().physics.y;
        let row = driver.sheets.source.row_at(top);
        assert_eq!(row, target);
        let (w, h) = driver.sheets.size.get();
        let height = Sheets::grid_rect(w, h).h() - pergola::widgets::grid_view::HEADER_H;
        let last = driver.sheets.source.row_at(top + f64::from(height) - 0.001);
        driver.dispatch(scroll(ScrollPhase::End, 0.0));
        let start = Instant::now();
        let mut slices = 0;
        while (row..=last).any(|r| stale(&driver, r)) {
            assert!(start.elapsed() < Duration::from_secs(10));
            assert!(stale(&driver, background), "background finished before the viewport");
            driver.sheets.doc.borrow_mut().settle();
            driver.sheets.changed();
            driver.window.paint(&mut driver.app).unwrap();
            slices += 1;
        }
        let doc = driver.sheets.doc.borrow();
        for r in row..=last {
            let value = expected(r, LATEST);
            assert!(!doc.wb.is_stale(0, r, 1));
            assert_eq!(doc.wb.value(0, r, 1), Value::Num(value));
            assert_eq!(doc.wb.value(0, r, 3), Value::Num(value + 1.0));
        }
        assert!(doc.wb.is_stale(0, background, 3));
        println!("PASS visible rows {}–{} updated in {slices} slices; row {} still stale", row + 1, last + 1, background + 1);
    }
    let start = Instant::now();
    while driver.pending() {
        assert!(start.elapsed() < Duration::from_secs(60));
        driver.sheets.doc.borrow_mut().settle();
    }
    verify(&driver.sheets.doc.borrow().wb, LATEST);
    println!("PASS final calculation verified across {ROWS} rows");
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, path] if mode == "make" => make(Path::new(path)),
        [mode, path, factor] if mode == "check" => {
            // Read the caches from disk without triggering recalculation.
            let wb = officina_grid::native::load(&fs::read(path).unwrap()).unwrap();
            let factor = factor.parse().unwrap();
            verify(&wb, factor);
            println!("verified {ROWS} rows, {} formula results, multiplier {factor}", 2 * ROWS);
        }
        [mode, path] if mode == "measure" => measure(&fs::read(path).unwrap()),
        [mode, path] if mode == "visible" => measure_visible(&fs::read(path).unwrap()),
        _ => panic!("usage: recalc_responsiveness make FILE | check FILE MULTIPLIER | measure FILE | visible FILE"),
    }
}
