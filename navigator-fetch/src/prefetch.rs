//! ★★ FETCHES IN PARALLEL, ASKED FOR IN SERIES (legacy-web §5.4.5.7, step 4).
//!
//! The converter asks for one URL at a time, synchronously: its engine is
//! single-threaded and every fetch is a call that returns bytes. Measured on
//! both corpora, that made a conversion's fetches strictly serial (the union
//! of their intervals equals their sum) and each its own connection, and a
//! page's subresources dominated its time to first display.
//!
//! So the converter also says what it is ABOUT to ask for: a hint, for every
//! stylesheet, image and script of the document as parsed. The prefetcher
//! fetches hinted URLs on a bounded pool of threads, each a `sibling` of one
//! `Conversation` (one set of pins, one pool of idle connections), and keeps
//! each result until the converter asks. A request for a URL:
//! - already fetched: answered at once;
//! - in flight: waits for it;
//! - hinted but not started: taken out of the queue and fetched now, on the
//!   asking thread, so a request never waits behind hints;
//! - never hinted: fetched now, on the asking thread.
//!
//! ★ HEADERS AHEAD OF BODIES (cut A). A header hint (`hint_header`) asks
//! for an image's first `HEAD_BYTES` only (a `Range` request, read to at
//! most that whatever the server sends), so the converter can size the
//! image's box before its body arrives. Every worker takes jobs in the
//! order they were hinted — so the stylesheets the converter hints first
//! are not held up by headers, and headers hinted before the images' bodies
//! are not held up by them — and `HEAD_WORKERS` more threads take header
//! jobs alone, so a header never waits behind a large image. `peek` answers from a
//! finished body, a finished header, or a new range job, waiting at most
//! as long as it is told.
//!
//! What it does NOT change: every fetch is still the Conversation's, under
//! its address rule and pins; a result is handed over at most once (a
//! second request for the URL fetches again, as before); hints are bounded
//! (`MAX_HINTS`) and a hint for a URL already known is ignored.

use crate::{Conversation, FetchError, Fetched, Ranged};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

/// Hints accepted per prefetcher. A page names more than this rarely
/// (the corpus's most: 150 images); past it, hints are dropped and the
/// URLs are fetched when asked for, as without a prefetcher.
pub const MAX_HINTS: usize = 512;
/// Default concurrency: what a browser opens per origin.
pub const DEFAULT_WORKERS: usize = 6;
/// ★ Bytes the prefetcher may pull for hints, unless told otherwise
/// (`set_budget`). Hints are fetched before anyone is charged for them, so
/// this is what bounds the bytes a page can make the fetcher pull that the
/// converter never asked for: past it, a hint is dropped and its URL is
/// fetched when asked, under the caller's own budget.
pub const DEFAULT_BUDGET: u64 = 128 * 1024 * 1024;

enum Entry {
    Queued,
    InFlight,
    Done(Result<Fetched, FetchError>),
}

/// The first bytes a header hint fetches: 16 KiB sizes 99.4% of the
/// corpus's images (`navigator_dom::natural::FIRST_HEADER_READ`).
pub const HEAD_BYTES: u64 = 16 * 1024;
/// The most bytes from the start a `peek` may ask for: the converter's
/// header cap (`navigator_dom::natural::MAX_HEADER_BYTES`).
pub const MAX_PEEK: u64 = 64 * 1024;
/// ★ How long a conversion's first display may wait for its images'
/// headers, from its first peek (`serve_with`). The headers were hinted
/// with the stylesheets, so most are in hand when the first peek comes;
/// this bounds what the rest may add to the first display.
pub const PEEK_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
/// Threads that serve header and range jobs only.
pub const HEAD_WORKERS: usize = 2;
/// Range jobs (header hints and peeks past them) kept, at most.
pub const MAX_RANGE_JOBS: usize = 2 * MAX_HINTS;

/// What a range job is doing.
enum RangeState {
    Queued,
    InFlight,
    Done(Result<Ranged, String>),
}

struct RangeJob { url: String, start: u64, len: u64, state: RangeState }

/// What `peek` hands back: bytes `start ..` of the body — exactly the
/// length asked for, or fewer only where the body ends.
#[derive(Debug, Clone, PartialEq)]
pub struct Peeked { pub status: u16, pub redirects: usize, pub url: String, pub bytes: Vec<u8> }

#[derive(Default)]
struct State {
    /// Body hints, with the order they were given in (`seq`).
    queue: VecDeque<(u64, String)>,
    seq: u64,
    entries: HashMap<String, Entry>,
    hinted: usize,
    closed: bool,
    /// Bytes pulled by prefetches, and the most they may pull.
    pulled: u64,
    budget: u64,
    /// Hints dropped because the budget was spent.
    over: usize,
    /// Requests answered from a finished prefetch, waited for, or fetched
    /// on the asking thread.
    hits: usize,
    waits: usize,
    direct: usize,
    /// Range jobs, in the order made; `range_queue` indexes the queued ones,
    /// with their `seq` (0 for a peek's job, which goes first).
    ranges: Vec<RangeJob>,
    range_queue: VecDeque<(u64, usize)>,
    /// Peeks answered, and those that ran out of time.
    peeks: usize,
    peek_timeouts: usize,
}

pub struct Prefetcher<'a> {
    state: Arc<(Mutex<State>, Condvar)>,
    conv: Conversation<'a>,
}

impl<'a> Prefetcher<'a> {
    /// Start `workers` fetching threads in `scope`, each a sibling of `conv`.
    pub fn start<'scope>(scope: &'scope std::thread::Scope<'scope, 'a>, conv: &Conversation<'a>, workers: usize) -> Self
    where 'a: 'scope
    {
        let state: Arc<(Mutex<State>, Condvar)> = Default::default();
        state.0.lock().unwrap().budget = DEFAULT_BUDGET;
        for w in 0..workers.max(1) + HEAD_WORKERS {
            let st = state.clone();
            let mut c = conv.sibling();
            let heads_only = w >= workers.max(1);
            scope.spawn(move || loop {
                let url = {
                    let (m, cv) = &*st;
                    let mut g = m.lock().unwrap();
                    loop {
                        if g.closed { return }
                        // ★ Whichever was hinted first: a header or a body.
                        // (A head-only worker takes headers alone.)
                        let head_first = match (g.range_queue.front(), g.queue.front()) {
                            (Some(_), _) if heads_only => true,
                            (Some((rs, _)), Some((bs, _))) => rs < bs,
                            (Some(_), None) => true,
                            _ => false,
                        };
                        if head_first {
                            let (_, i) = g.range_queue.pop_front().expect("checked");
                            if !matches!(g.ranges[i].state, RangeState::Queued) { continue }
                            g.ranges[i].state = RangeState::InFlight;
                            let (u, start, len) = (g.ranges[i].url.clone(), g.ranges[i].start, g.ranges[i].len);
                            drop(g);
                            let r = c.fetch_range(&u, start, len).map_err(|e| e.to_string());
                            g = m.lock().unwrap();
                            if let Ok(rg) = &r { g.pulled += rg.bytes.len() as u64 }
                            g.ranges[i].state = RangeState::Done(r);
                            cv.notify_all();
                            continue;
                        }
                        if heads_only { g = cv.wait(g).unwrap(); continue }
                        if g.pulled >= g.budget && !g.queue.is_empty() {
                            // Spent: what is still queued is fetched when
                            // asked, under the asker's budget.
                            let dropped: Vec<String> = g.queue.drain(..).map(|(_, u)| u).collect();
                            for u in dropped { if matches!(g.entries.get(&u), Some(Entry::Queued)) { g.entries.remove(&u); g.over += 1 } }
                        }
                        if let Some((_, u)) = g.queue.pop_front() {
                            // A request may have taken it out of the queue already.
                            if matches!(g.entries.get(&u), Some(Entry::Queued)) {
                                g.entries.insert(u.clone(), Entry::InFlight);
                                break u;
                            }
                            continue;
                        }
                        g = cv.wait(g).unwrap();
                    }
                };
                let r = c.fetch(&url);
                let (m, cv) = &*st;
                let mut g = m.lock().unwrap();
                if let Ok(f) = &r { g.pulled += f.response.body.len() as u64 }
                g.entries.insert(url, Entry::Done(r));
                drop(g);
                cv.notify_all();
            });
        }
        Prefetcher { state, conv: conv.sibling() }
    }

    /// The most the prefetcher may pull for hints, from now on.
    pub fn set_budget(&self, bytes: u64) { self.state.0.lock().unwrap().budget = bytes }

    /// (bytes pulled for hints, hints dropped because the budget was spent)
    pub fn pulled(&self) -> (u64, usize) { let g = self.state.0.lock().unwrap(); (g.pulled, g.over) }

    /// The converter is about to ask for `url`.
    pub fn hint(&self, url: &str) {
        let (m, cv) = &*self.state;
        let mut g = m.lock().unwrap();
        if g.closed || g.hinted >= MAX_HINTS || g.entries.contains_key(url) { return }
        if g.pulled >= g.budget { g.over += 1; return }
        g.hinted += 1;
        g.entries.insert(url.to_string(), Entry::Queued);
        g.seq += 1;
        let seq = g.seq;
        g.queue.push_back((seq, url.to_string()));
        cv.notify_one();
    }

    /// ★ The converter is about to ask for `url`'s HEADER: its first
    /// `HEAD_BYTES`, fetched ahead of every queued body.
    pub fn hint_header(&self, url: &str) {
        let (m, cv) = &*self.state;
        let mut g = m.lock().unwrap();
        if g.closed || g.ranges.len() >= MAX_RANGE_JOBS || g.ranges.iter().any(|j| j.url == url && j.start == 0) { return }
        if g.pulled >= g.budget { g.over += 1; return }
        let i = g.ranges.len();
        g.ranges.push(RangeJob { url: url.to_string(), start: 0, len: HEAD_BYTES, state: RangeState::Queued });
        g.seq += 1;
        let seq = g.seq;
        g.range_queue.push_back((seq, i));
        cv.notify_all();
    }

    /// ★ Bytes `start .. start + len` of `url`'s body (`start + len` at most
    /// `MAX_PEEK`): exactly `len` of them, or fewer only where the body
    /// ends. Answered from the body if a prefetch has it (which stays for
    /// the request that will come), else from a header or range job that
    /// covers them, else from a new range job queued ahead of every body.
    /// Waits at most `wait`; past it the answer is an error and the job, if
    /// any, carries on (its bytes are not wasted: a later peek may use them).
    pub fn peek(&mut self, url: &str, start: u64, len: u64, wait: std::time::Duration) -> Result<Peeked, String> {
        if len == 0 || start.saturating_add(len) > MAX_PEEK { return Err(format!("refused: a peek past {MAX_PEEK} bytes")) }
        let deadline = std::time::Instant::now() + wait;
        let (m, cv) = &*self.state;
        let mut g = m.lock().unwrap();
        let slice = |b: &[u8], from: u64| -> Vec<u8> {
            let a = (start.saturating_sub(from) as usize).min(b.len());
            b[a..(a + len as usize).min(b.len())].to_vec()
        };
        let mut queued = false;
        loop {
            let mut answer: Option<Result<Peeked, String>> = match g.entries.get(url) {
                Some(Entry::Done(Ok(f))) if (200..300).contains(&f.response.status) =>
                    Some(Ok(Peeked { status: f.response.status, redirects: f.redirects, url: f.url.clone(), bytes: slice(&f.response.body, 0) })),
                Some(Entry::Done(Ok(f))) => Some(Err(format!("HTTP {}", f.response.status))),
                Some(Entry::Done(Err(e))) => Some(Err(e.to_string())),
                _ => None,
            };
            // A range job whose bytes cover the ask: done, in flight, or queued.
            let covers = |j: &RangeJob| j.url == url && j.start <= start && start + len <= j.start + j.len;
            let mut waiting = false;
            if answer.is_none() {
                for j in g.ranges.iter().filter(|j| j.url == url && j.start <= start) {
                    match &j.state {
                        RangeState::Done(Ok(r)) if (200..300).contains(&r.status) => {
                            let have = j.start + r.bytes.len() as u64;
                            if start + len <= have || r.end {
                                answer = Some(if !r.end && (r.bytes.len() as u64) < j.len { Err("a short range that is not the end".into()) }
                                    else { Ok(Peeked { status: r.status, redirects: r.redirects, url: r.url.clone(), bytes: slice(&r.bytes, j.start) }) });
                                break;
                            }
                        }
                        RangeState::Done(Ok(r)) if covers(j) => { answer = Some(Err(format!("HTTP {}", r.status))); break }
                        RangeState::Done(Err(e)) if covers(j) => { answer = Some(Err(e.clone())); break }
                        RangeState::InFlight | RangeState::Queued if covers(j) => waiting = true,
                        _ => {}
                    }
                }
            }
            if let Some(a) = answer { g.peeks += 1; return a }
            if !waiting && !queued {
                if g.closed || g.ranges.len() >= MAX_RANGE_JOBS { return Err("no more range jobs".into()) }
                // Read from where the covering bytes must start: the whole
                // of what is missing, in one job, ahead of every body.
                let i = g.ranges.len();
                g.ranges.push(RangeJob { url: url.to_string(), start, len, state: RangeState::Queued });
                g.range_queue.push_front((0, i));
                cv.notify_all();
                queued = true;
                continue;
            }
            let now = std::time::Instant::now();
            if now >= deadline { g.peek_timeouts += 1; return Err("not in time".into()) }
            g = cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }

    /// (peeks answered, peeks that ran out of time)
    pub fn peek_stats(&self) -> (usize, usize) { let g = self.state.0.lock().unwrap(); (g.peeks, g.peek_timeouts) }

    /// Fetch `url`: from a prefetch if there is one, else now.
    pub fn fetch(&mut self, url: &str) -> Result<Fetched, FetchError> {
        {
            let (m, cv) = &*self.state;
            let mut g = m.lock().unwrap();
            let mut waited = false;
            loop {
                match g.entries.get(url) {
                    Some(Entry::Done(_)) => {
                        let Some(Entry::Done(r)) = g.entries.remove(url) else { unreachable!() };
                        if waited { g.waits += 1 } else { g.hits += 1 }
                        return r;
                    }
                    Some(Entry::InFlight) => { waited = true; g = cv.wait(g).unwrap() }
                    Some(Entry::Queued) => { g.entries.remove(url); break }
                    None => break,
                }
            }
            g.direct += 1;
        }
        self.conv.fetch(url)
    }

    /// (answered from a finished prefetch, waited for one in flight,
    /// fetched on the asking thread, hints accepted)
    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let g = self.state.0.lock().unwrap();
        (g.hits, g.waits, g.direct, g.hinted)
    }

    /// Stop the workers: queued hints are dropped, a fetch in flight runs
    /// to its end and is discarded.
    pub fn close(&self) {
        let (m, cv) = &*self.state;
        let mut g = m.lock().unwrap();
        g.closed = true;
        g.queue.clear();
        g.range_queue.clear();
        cv.notify_all();
    }
}

impl Drop for Prefetcher<'_> {
    fn drop(&mut self) { self.close() }
}
