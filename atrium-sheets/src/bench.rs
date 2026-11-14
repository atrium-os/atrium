//! The O2 scroll gates' workload and arithmetic (docs/spec/officina.md §16).
//!
//! The same scripted input drives the app headless (`examples/scroll_cost.rs`, on a
//! virtual clock with the app's real costs) and live on frescod (`atrium-sheets
//! --bench`, on the wall clock), and both report with [`Report`]: frame time p50, p99,
//! p99.9 and max, missed vsyncs, and stalls (what is on screen more than 100 ms behind
//! the input), the ScrollView gates of pergola.md §10.

use pergola::event::ScrollPhase;
use pergola::geom::Point;
use pergola::{Event, Key, KeyEventKind, Modifiers};

use officina_grid::addr::CellAddr;
use officina_grid::Value;

use crate::doc::Doc;

/// A sheet of `rows` rows: an index, seven columns of numbers, a text column and a
/// formula filled down the last column (one formula group).
pub fn big_sheet(rows: u32) -> Doc {
    let mut d = Doc::new(&format!("{rows} rows"));
    let wb = &mut d.wb;
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    const BATCH: u32 = 1 << 16;
    let mut r0 = 0;
    while r0 < rows {
        let n = BATCH.min(rows - r0);
        let idx: Vec<f64> = (r0..r0 + n).map(|r| r as f64 + 1.0).collect();
        wb.set_numbers(0, 0, r0, &idx);
        for c in 1..8 {
            let v: Vec<f64> = (0..n)
                .map(|_| {
                    let r = next();
                    match c {
                        1 => (r % 100_000) as f64 / 100.0,
                        2 => (r % 1000) as f64,
                        3 => (r % 2_000_000) as f64 - 1_000_000.0,
                        _ => ((r % 10_000_000) as f64) / 7.0,
                    }
                })
                .collect();
            wb.set_numbers(0, c, r0, &v);
        }
        r0 += n;
    }
    // Text every 16th row, so the column is mostly empty chunks (as real sheets are).
    for r in (0..rows).step_by(16) {
        wb.set_value(CellAddr::new(0, r, 8), Value::str(&format!("item {}", r / 16)));
    }
    if rows > 0 {
        wb.fill_formula(0, 9, 0, rows - 1, "=B1*C1").expect("formula");
    }
    wb.recalc();
    d
}

/// How the script moves the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// A 4,000 px/s touchpad swipe down every 1.5 s, released into a fling; every
    /// fourth one goes up.
    Swipe,
    /// The same, diagonally (two-axis shift).
    Diagonal,
    /// Mouse-wheel notches, 120 px, ten a second.
    Wheel,
    /// Page Down four times a second.
    Page,
    /// Ctrl+End, then Ctrl+Home, every second: the far ends of the sheet.
    Jump,
}

impl Pattern {
    pub const ALL: [Pattern; 5] = [Pattern::Swipe, Pattern::Diagonal, Pattern::Wheel, Pattern::Page, Pattern::Jump];
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "swipe" => Pattern::Swipe,
            "diagonal" => Pattern::Diagonal,
            "wheel" => Pattern::Wheel,
            "page" => Pattern::Page,
            "jump" => Pattern::Jump,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Pattern::Swipe => "swipe",
            Pattern::Diagonal => "diagonal",
            Pattern::Wheel => "wheel",
            Pattern::Page => "page",
            Pattern::Jump => "jump",
        }
    }
}

/// The input for `secs` seconds of `p` over a grid whose middle is `at`, as
/// (time ms, event). It starts with a press on a cell, so keys reach the grid.
pub fn script(p: Pattern, secs: f64, at: Point) -> Vec<(f64, Event)> {
    let none = Modifiers::default();
    let ctrl = Modifiers { ctrl: true, ..Default::default() };
    let mut ev = vec![(0.0, Event::PointerDown { at }), (1.0, Event::PointerUp { at })];
    let end = secs * 1000.0;
    let scroll = |dx: f32, dy: f32, phase| Event::Scroll { at: Some(at), dx, dy, modifiers: Modifiers::default(), phase };
    let key = |k, m| Event::Key { kind: KeyEventKind::Down, key: k, modifiers: m, chars: String::new() };
    match p {
        Pattern::Swipe | Pattern::Diagonal => {
            let mut t = 100.0;
            let mut i = 0;
            while t + 100.0 < end {
                let sign = if i % 4 == 3 { -1.0 } else { 1.0 };
                // 4,000 px/s at 120 Hz input.
                let dy = sign * 4000.0 / 120.0;
                let dx = if p == Pattern::Diagonal { dy * 0.6 } else { 0.0 };
                ev.push((t, scroll(0.0, 0.0, ScrollPhase::Begin)));
                for k in 1..=8 {
                    ev.push((t + k as f64 * 1000.0 / 120.0, scroll(dx, dy, ScrollPhase::Update)));
                }
                ev.push((t + 9.0 * 1000.0 / 120.0, scroll(0.0, 0.0, ScrollPhase::End)));
                t += 1500.0;
                i += 1;
            }
        }
        Pattern::Wheel => {
            let mut t = 100.0;
            while t < end {
                ev.push((t, scroll(0.0, 120.0, ScrollPhase::None)));
                t += 100.0;
            }
        }
        Pattern::Page => {
            let mut t = 100.0;
            while t < end {
                ev.push((t, key(Key::PageDown, none)));
                t += 250.0;
            }
        }
        Pattern::Jump => {
            let mut t = 100.0;
            let mut down = true;
            while t < end {
                ev.push((t, key(if down { Key::End } else { Key::Home }, ctrl)));
                down = !down;
                t += 1000.0;
            }
        }
    }
    ev
}

/// One frame: when its work began (ms), what it cost (ms), whether it changed the
/// screen.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub start: f64,
    pub cost: f64,
}

/// The gates' numbers for one run.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub frames: usize,
    pub p50: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
    /// Vsyncs after one where the view wanted a frame that brought none.
    pub missed: usize,
    pub vsyncs_moving: usize,
    /// Stretches over 100 ms where the screen was behind: (start ms, length ms).
    pub stalls: Vec<(f64, f64)>,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

impl Report {
    /// From frames presented at the first vsync (period `period` ms) after their work
    /// ended; `moving` says, per vsync, whether the view wanted a new frame then.
    pub fn new(frames: &[Frame], period: f64, moving: &[bool]) -> Self {
        let mut costs: Vec<f64> = frames.iter().map(|f| f.cost).collect();
        costs.sort_by(f64::total_cmp);
        let mut shown = vec![false; moving.len()];
        for f in frames {
            let v = ((f.start + f.cost) / period).ceil() as usize;
            if v < shown.len() {
                shown[v] = true;
            }
        }
        // Work wanted at vsync v is due on screen at v + 1.
        let mut missed = 0;
        let mut vsyncs_moving = 0;
        let mut stalls = Vec::new();
        let mut run: Option<usize> = None;
        for v in 0..moving.len() {
            if moving[v] {
                vsyncs_moving += 1;
                if !shown.get(v + 1).copied().unwrap_or(true) {
                    missed += 1;
                    run.get_or_insert(v);
                    continue;
                }
            }
            if let Some(s) = run.take() {
                let len = (v - s) as f64 * period;
                if len > 100.0 {
                    stalls.push((s as f64 * period, len));
                }
            }
        }
        if let Some(s) = run {
            let len = (moving.len() - s) as f64 * period;
            if len > 100.0 {
                stalls.push((s as f64 * period, len));
            }
        }
        Report { frames: frames.len(), p50: pct(&costs, 0.5), p99: pct(&costs, 0.99), p999: pct(&costs, 0.999), max: costs.last().copied().unwrap_or(0.0), missed, vsyncs_moving, stalls }
    }

    pub fn missed_pct(&self) -> f64 {
        if self.vsyncs_moving == 0 {
            0.0
        } else {
            100.0 * self.missed as f64 / self.vsyncs_moving as f64
        }
    }

    /// The gates: no stall over 100 ms, p99.9 at most two periods, at most 1 % missed.
    pub fn passes(&self, period: f64) -> bool {
        self.stalls.is_empty() && self.p999 <= 2.0 * period && self.missed_pct() <= 1.0
    }

    pub fn line(&self, period: f64) -> String {
        format!(
            "frames {} p50 {:.2} ms p99 {:.2} ms p99.9 {:.2} ms max {:.2} ms missed {:.1}% ({}/{}) stalls>100ms {} {}",
            self.frames,
            self.p50,
            self.p99,
            self.p999,
            self.max,
            self.missed_pct(),
            self.missed,
            self.vsyncs_moving,
            self.stalls.len(),
            if self.passes(period) { "PASS" } else { "FAIL" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stall_is_found_and_timed() {
        let period = 1000.0 / 60.0;
        let moving = vec![true; 120];
        // A frame every vsync, but one takes 300 ms.
        let mut frames = Vec::new();
        let mut t = 0.0;
        while t < 2000.0 {
            let cost = if (500.0..516.0).contains(&t) { 300.0 } else { 2.0 };
            frames.push(Frame { start: t, cost });
            t = ((t + cost) / period).ceil() * period;
        }
        let r = Report::new(&frames, period, &moving);
        assert_eq!(r.stalls.len(), 1, "{r:?}");
        assert!((r.stalls[0].1 - 300.0).abs() < 20.0, "{r:?}");
        assert!(!r.passes(period));
    }

    #[test]
    fn the_big_sheet_has_what_it_says() {
        let d = big_sheet(10_000);
        assert_eq!(d.wb.extent(0), (10_000, 10));
        assert_eq!(d.wb.get(CellAddr::new(0, 9_999, 0)), Value::Num(10_000.0));
        let (b, c) = (d.wb.get(CellAddr::new(0, 77, 1)), d.wb.get(CellAddr::new(0, 77, 2)));
        if let (Value::Num(b), Value::Num(c)) = (b, c) {
            assert_eq!(d.wb.get(CellAddr::new(0, 77, 9)), Value::Num(b * c));
        } else {
            panic!("numbers");
        }
    }
}
