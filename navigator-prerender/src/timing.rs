//! ★ WHERE A CONVERSION'S TIME GOES (legacy-web §5.4.5.7, time to first
//! display). Observation only: nothing here reaches an artifact, and no
//! decision in the converter reads a clock (threat model §2.6 — budgets count
//! work, never time). This is the stopwatch the measurement reads.
//!
//! Phases are EXCLUSIVE spans on a stack: entering a nested span pauses the
//! enclosing one, so a layout inside a script, or a fetch inside a layout, is
//! charged to the layout or the fetch and not twice. The exclusive times of
//! one conversion therefore sum to its wall time (less whatever ran outside
//! any span, reported as `other`). Each span is also recorded INCLUSIVELY,
//! for the questions that want the whole of something ("how long did
//! exploration take, relayouts included").
//!
//! Fetches are logged one by one (kind, start, duration, bytes), so the
//! report can say whether they overlapped: the union of their intervals
//! against their sum.
//!
//! One conversion per thread at a time, which is how the converter runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Default, Clone, Debug)]
pub struct Totals {
    pub exclusive: Duration,
    pub inclusive: Duration,
    pub count: u32,
}

#[derive(Clone, Debug)]
pub struct FetchRec {
    /// What the fetch was for: the span that was open when it started
    /// (`script`, `module`, `page`, `layout`, `subresource`, …).
    pub kind: String,
    pub start: Duration,
    pub dur: Duration,
    pub bytes: u64,
    pub ok: bool,
}

#[derive(Default)]
struct State {
    origin: Option<Instant>,
    /// (name, entered at, last resumed at)
    stack: Vec<(String, Instant, Instant)>,
    totals: BTreeMap<String, Totals>,
    fetches: Vec<FetchRec>,
}

thread_local! {
    static T: RefCell<State> = RefCell::new(State::default());
}

/// Start a conversion's stopwatch, discarding the last one.
pub fn reset() {
    T.with(|t| *t.borrow_mut() = State { origin: Some(Instant::now()), ..State::default() });
}

/// An open span; closing it (drop) charges the time since it last resumed.
pub struct Span(());

/// Enter a phase. Pauses whatever phase is open.
pub fn span(name: &str) -> Span {
    let now = Instant::now();
    T.with(|t| {
        let mut t = t.borrow_mut();
        if t.origin.is_none() { t.origin = Some(now) }
        if let Some((n, _, resumed)) = t.stack.last().cloned() {
            t.totals.entry(n).or_default().exclusive += now - resumed;
        }
        t.stack.push((name.to_string(), now, now));
    });
    Span(())
}

impl Drop for Span {
    fn drop(&mut self) {
        let now = Instant::now();
        T.with(|t| {
            let mut t = t.borrow_mut();
            let Some((n, entered, resumed)) = t.stack.pop() else { return };
            let e = t.totals.entry(n).or_default();
            e.exclusive += now - resumed;
            e.inclusive += now - entered;
            e.count += 1;
            if let Some(top) = t.stack.last_mut() { top.2 = now }
        });
    }
}

/// The innermost open span's name, or "".
pub fn current() -> String {
    T.with(|t| t.borrow().stack.last().map(|s| s.0.clone()).unwrap_or_default())
}

/// Time one fetch: charged to a `fetch.<kind>` span, where `kind` is the
/// phase that asked for it, and logged for the overlap figure.
pub fn fetch<T>(f: impl FnOnce() -> Result<T, String>, len: impl Fn(&T) -> u64) -> Result<T, String> {
    let kind = current();
    let kind = kind.strip_prefix("fetch.").unwrap_or(&kind).to_string();
    let kind = if kind.is_empty() { "other".to_string() } else { kind };
    let start = Instant::now();
    let r = {
        let _s = span(&format!("fetch.{kind}"));
        f()
    };
    let dur = start.elapsed();
    T.with(|t| {
        let mut t = t.borrow_mut();
        let origin = t.origin.unwrap_or(start);
        let bytes = r.as_ref().map(&len).unwrap_or(0);
        t.fetches.push(FetchRec { kind, start: start - origin, dur, bytes, ok: r.is_ok() });
    });
    r
}

/// Everything recorded since `reset`.
pub fn snapshot() -> (Duration, BTreeMap<String, Totals>, Vec<FetchRec>) {
    T.with(|t| {
        let t = t.borrow();
        let wall = t.origin.map(|o| o.elapsed()).unwrap_or_default();
        (wall, t.totals.clone(), t.fetches.clone())
    })
}

/// The union of fetch intervals: equal to their sum when nothing overlapped.
pub fn union(fetches: &[FetchRec]) -> Duration {
    let mut iv: Vec<(Duration, Duration)> = fetches.iter().map(|f| (f.start, f.start + f.dur)).collect();
    iv.sort();
    let mut total = Duration::ZERO;
    let mut cur: Option<(Duration, Duration)> = None;
    for (s, e) in iv {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => { total += ce - cs; cur = Some((s, e)) }
            None => cur = Some((s, e)),
        }
    }
    if let Some((cs, ce)) = cur { total += ce - cs }
    total
}

/// The report line: `TM` then `key=value` fields, tab-separated. Times in
/// milliseconds with one decimal. Per phase `x.<name>` (exclusive),
/// `i.<name>` (inclusive) and `n.<name>` (count); per fetch kind
/// `f.<kind>=<count>/<bytes>/<sum ms>`; and `fetch_union`, `fetch_sum`,
/// `wall`.
pub fn report_line() -> String {
    let (wall, totals, fetches) = snapshot();
    let ms = |d: Duration| format!("{:.1}", d.as_secs_f64() * 1000.0);
    let mut o = format!("TM\twall={}", ms(wall));
    let mut covered = Duration::ZERO;
    for (k, v) in &totals {
        covered += v.exclusive;
        o.push_str(&format!("\tx.{k}={}\ti.{k}={}\tn.{k}={}", ms(v.exclusive), ms(v.inclusive), v.count));
    }
    o.push_str(&format!("\tx.other={}", ms(wall.saturating_sub(covered))));
    let mut kinds: BTreeMap<&str, (u32, u64, Duration, u32)> = BTreeMap::new();
    for f in &fetches {
        let e = kinds.entry(&f.kind).or_default();
        e.0 += 1; e.1 += f.bytes; e.2 += f.dur; if !f.ok { e.3 += 1 }
    }
    for (k, (n, b, d, failed)) in &kinds {
        o.push_str(&format!("\tf.{k}={n}/{b}/{}/{failed}", ms(*d)));
    }
    let sum: Duration = fetches.iter().map(|f| f.dur).sum();
    o.push_str(&format!("\tfetch_sum={}\tfetch_union={}\tfetches={}", ms(sum), ms(union(&fetches)), fetches.len()));
    // When the first and last fetch started and ended: the network's
    // share of the critical path, from the conversion's own clock.
    if let (Some(first), Some(last)) = (fetches.iter().map(|f| f.start).min(),
                                        fetches.iter().map(|f| f.start + f.dur).max()) {
        o.push_str(&format!("\tfetch_first={}\tfetch_last_end={}", ms(first), ms(last)));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_spans_are_exclusive_and_sum_to_the_whole() {
        reset();
        {
            let _a = span("a");
            std::thread::sleep(Duration::from_millis(20));
            {
                let _b = span("b");
                std::thread::sleep(Duration::from_millis(30));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, t, _) = snapshot();
        let a = &t["a"]; let b = &t["b"];
        assert!(b.exclusive >= Duration::from_millis(30));
        assert!(a.exclusive >= Duration::from_millis(30) && a.exclusive < Duration::from_millis(55), "{a:?}");
        assert!(a.inclusive >= a.exclusive + b.exclusive);
        assert_eq!((a.count, b.count), (1, 1));
    }

    #[test]
    fn fetches_take_the_kind_of_the_phase_that_asked() {
        reset();
        {
            let _s = span("script");
            let _ = fetch(|| Ok::<_, String>(vec![0u8; 7]), |v| v.len() as u64);
        }
        let (_, t, f) = snapshot();
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].kind.as_str(), f[0].bytes, f[0].ok), ("script", 7, true));
        assert!(t.contains_key("fetch.script"));
    }

    #[test]
    fn serial_fetches_union_equals_sum_overlap_does_not() {
        let r = |s: u64, d: u64| FetchRec { kind: "k".into(), start: Duration::from_millis(s),
            dur: Duration::from_millis(d), bytes: 0, ok: true };
        assert_eq!(union(&[r(0, 10), r(10, 10), r(30, 5)]), Duration::from_millis(25));
        assert_eq!(union(&[r(0, 10), r(5, 10)]), Duration::from_millis(15));
    }
}
