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
//! What it does NOT change: every fetch is still the Conversation's, under
//! its address rule and pins; a result is handed over at most once (a
//! second request for the URL fetches again, as before); hints are bounded
//! (`MAX_HINTS`) and a hint for a URL already known is ignored.

use crate::{Conversation, FetchError, Fetched};
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

#[derive(Default)]
struct State {
    queue: VecDeque<String>,
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
        for _ in 0..workers.max(1) {
            let st = state.clone();
            let mut c = conv.sibling();
            scope.spawn(move || loop {
                let url = {
                    let (m, cv) = &*st;
                    let mut g = m.lock().unwrap();
                    loop {
                        if g.closed { return }
                        if g.pulled >= g.budget && !g.queue.is_empty() {
                            // Spent: what is still queued is fetched when
                            // asked, under the asker's budget.
                            let dropped: Vec<String> = g.queue.drain(..).collect();
                            for u in dropped { if matches!(g.entries.get(&u), Some(Entry::Queued)) { g.entries.remove(&u); g.over += 1 } }
                        }
                        if let Some(u) = g.queue.pop_front() {
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
        g.queue.push_back(url.to_string());
        cv.notify_one();
    }

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
        cv.notify_all();
    }
}

impl Drop for Prefetcher<'_> {
    fn drop(&mut self) { self.close() }
}
