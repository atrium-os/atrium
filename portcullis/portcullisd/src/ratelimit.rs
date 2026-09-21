//! What a client may ask the daemon to create, and how fast.
//!
//! ★★ `ExecInstance` IS THE FIRST DAEMON VERB A PROGRAM CALLS IN A LOOP.
//! `Launch` is driven by a person clicking something; a worker pool is driven
//! by pages opening. Nothing on that path restrained it: a client could ask
//! for jails as fast as the daemon could make them, and the only thing
//! standing in the way was the *client's* own session bound — a limit held by
//! the thing being limited.
//!
//! Two different exhaustions, so two different limits:
//!
//!   - **Concurrency** bounds what exists at once. Each one-shot jail carries
//!     a tmpfs and a mount stack, so this is the memory-and-kernel-table
//!     limit.
//!   - **Rate** bounds churn. A client that creates and destroys jails in a
//!     tight loop holds little at any instant and can still saturate the
//!     daemon, `jail(8)`, and the mount table's insert/remove path.
//!
//! Per user first, because a global-only limit lets one client starve
//! everyone else; with a global ceiling behind it, because per-user-only lets
//! N users do the same thing together.
//!
//! ★ DERIVED FROM A MEASUREMENT: the Navigator's corpus run creates 98 jails
//! back to back in 58.7s — 1.67 jails/second sustained by a legitimate
//! client, with a burst of up to 16 when a broker opens every session it is
//! allowed. The defaults below are several times that, so ordinary use never
//! meets them, and orders of magnitude under what a loop would ask for.
//!
//! ★★ AND THE CLOCK IS AN ARGUMENT, as everywhere else in this work. A rate
//! limiter tested by sleeping is a slow test that becomes a flaky one; here
//! every window is exact.

use std::collections::HashMap;

/// Milliseconds from any fixed origin, required to be monotonic.
pub type Millis = u64;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// One-shot jails a single user may hold at once. Two brokers' worth of
    /// the Navigator's own 16-session bound.
    pub per_user_concurrent: usize,
    /// Across every user — the machine's ceiling, so per-user limits cannot
    /// be multiplied by opening more accounts.
    pub global_concurrent: usize,
    /// Creations a user may make in a burst: a broker opening every session
    /// it is allowed, at once.
    pub burst: u32,
    /// Sustained creations per second once the burst is spent. ~5x the
    /// measured 1.67/s of a real corpus run.
    pub refill_per_sec: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            per_user_concurrent: 32,
            global_concurrent: 64,
            burst: 32,
            refill_per_sec: 8,
        }
    }
}

/// Why a request was turned away. ★ Each names its own limit: "slow down" and
/// "you are holding too many" call for different actions from a client, and a
/// single "denied" would leave it guessing which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    TooManyForUser { held: usize, allowed: usize },
    TooManyOnHost { held: usize, allowed: usize },
    TooFast { allowed_per_sec: u32 },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::TooManyForUser { held, allowed } => write!(f,
                "holding {held} one-shot jails, the per-user limit is {allowed}"),
            Refusal::TooManyOnHost { held, allowed } => write!(f,
                "{held} one-shot jails on this host, the limit is {allowed}"),
            Refusal::TooFast { allowed_per_sec } => write!(f,
                "creating jails faster than {allowed_per_sec}/s sustained"),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Bucket {
    held: usize,
    tokens: f64,
    last: Millis,
}

#[derive(Debug)]
pub struct Limiter {
    limits: Limits,
    users: HashMap<String, Bucket>,
    held_total: usize,
}

impl Default for Limiter {
    fn default() -> Self { Limiter::new(Limits::default()) }
}

impl Limiter {
    pub fn new(limits: Limits) -> Self {
        Limiter { limits, users: HashMap::new(), held_total: 0 }
    }

    pub fn held(&self) -> usize { self.held_total }
    pub fn held_by(&self, user: &str) -> usize {
        self.users.get(user).map(|b| b.held).unwrap_or(0)
    }

    /// Take a slot, or say why not.
    ///
    /// ★ THE CONCURRENCY CHECKS COME FIRST, and the token is spent only if
    /// they pass. A refusal that consumed a token would let a client at its
    /// concurrency limit also exhaust its rate budget by retrying — turning
    /// one limit into two punishments for the same condition.
    pub fn acquire(&mut self, user: &str, now: Millis) -> Result<(), Refusal> {
        if self.held_total >= self.limits.global_concurrent {
            return Err(Refusal::TooManyOnHost {
                held: self.held_total, allowed: self.limits.global_concurrent });
        }
        let limits = self.limits;
        let b = self.users.entry(user.to_string()).or_insert(Bucket {
            held: 0, tokens: limits.burst as f64, last: now,
        });
        if b.held >= limits.per_user_concurrent {
            return Err(Refusal::TooManyForUser {
                held: b.held, allowed: limits.per_user_concurrent });
        }
        // Refill. ★ `saturating_sub`: a clock that steps backwards must not
        // hand out a negative elapsed time, which would drain the bucket
        // rather than leave it alone.
        let elapsed = now.saturating_sub(b.last) as f64 / 1000.0;
        b.tokens = (b.tokens + elapsed * limits.refill_per_sec as f64)
            .min(limits.burst as f64);
        b.last = b.last.max(now);
        if b.tokens < 1.0 {
            return Err(Refusal::TooFast { allowed_per_sec: limits.refill_per_sec });
        }
        b.tokens -= 1.0;
        b.held += 1;
        self.held_total += 1;
        Ok(())
    }

    /// Give a slot back.
    ///
    /// ★ MUST RUN ON EVERY PATH. A leaked count is permanent: it lowers the
    /// limit for the rest of the daemon's life, with nothing to show why, and
    /// the machine slowly refuses work it could do. The daemon calls this
    /// through a `Slot` guard so an early return or a panicking thread cannot
    /// skip it.
    pub fn release(&mut self, user: &str) {
        if let Some(b) = self.users.get_mut(user) {
            b.held = b.held.saturating_sub(1);
        }
        self.held_total = self.held_total.saturating_sub(1);
    }
}

/// An acquired slot, released when dropped.
pub struct Slot {
    limiter: std::sync::Arc<std::sync::Mutex<Limiter>>,
    user: String,
}

impl Slot {
    pub fn acquire(
        limiter: &std::sync::Arc<std::sync::Mutex<Limiter>>,
        user: &str,
        now: Millis,
    ) -> Result<Slot, Refusal> {
        // ★ A poisoned lock is treated as usable rather than fatal: a panic in
        // one connection thread must not make the daemon refuse every future
        // jail. The counts it protects are advisory, not a safety invariant.
        let mut l = limiter.lock().unwrap_or_else(|e| e.into_inner());
        l.acquire(user, now)?;
        drop(l);
        Ok(Slot { limiter: limiter.clone(), user: user.to_string() })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut l = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        l.release(&self.user);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: Millis = 1000;

    fn limiter(per_user: usize, global: usize, burst: u32, rate: u32) -> Limiter {
        Limiter::new(Limits {
            per_user_concurrent: per_user, global_concurrent: global,
            burst, refill_per_sec: rate,
        })
    }

    /// ★ THE COUNTERWEIGHT FIRST: a real workload must never meet these. The
    /// corpus run creates 98 jails back to back at 1.67/s, one at a time.
    #[test]
    fn a_real_corpus_run_is_never_limited() {
        let mut l = Limiter::default();
        let mut now = 0;
        for i in 0..98 {
            l.acquire("reader", now).unwrap_or_else(|e| panic!("refused jail {i}: {e}"));
            now += 600;            // measured: ~0.6s per document
            l.release("reader");
        }
        assert_eq!(l.held(), 0);
    }

    /// And a broker opening every session it is allowed, all at once.
    #[test]
    fn a_full_broker_opening_every_session_at_once_is_allowed() {
        let mut l = Limiter::default();
        for i in 0..16 { l.acquire("reader", 0).unwrap_or_else(|e| panic!("session {i}: {e}")); }
        assert_eq!(l.held(), 16);
    }

    #[test]
    fn concurrency_is_capped_per_user_and_names_the_limit() {
        let mut l = limiter(2, 10, 100, 100);
        l.acquire("a", 0).unwrap();
        l.acquire("a", 0).unwrap();
        match l.acquire("a", 0).unwrap_err() {
            Refusal::TooManyForUser { held, allowed } => assert_eq!((held, allowed), (2, 2)),
            other => panic!("{other}"),
        }
    }

    /// ★ ONE USER MUST NOT STARVE ANOTHER. That is why the per-user limit
    /// exists at all rather than only a global one.
    #[test]
    fn one_user_at_its_limit_does_not_block_another() {
        let mut l = limiter(2, 10, 100, 100);
        l.acquire("a", 0).unwrap();
        l.acquire("a", 0).unwrap();
        assert!(l.acquire("a", 0).is_err());
        l.acquire("b", 0).expect("a different user must still be served");
    }

    /// ★ AND PER-USER LIMITS MUST NOT MULTIPLY BY ADDING USERS.
    #[test]
    fn the_host_ceiling_holds_across_users() {
        let mut l = limiter(2, 3, 100, 100);
        l.acquire("a", 0).unwrap();
        l.acquire("a", 0).unwrap();
        l.acquire("b", 0).unwrap();
        match l.acquire("c", 0).unwrap_err() {
            Refusal::TooManyOnHost { held, allowed } => assert_eq!((held, allowed), (3, 3)),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn releasing_frees_the_slot() {
        let mut l = limiter(1, 10, 100, 100);
        l.acquire("a", 0).unwrap();
        assert!(l.acquire("a", 0).is_err());
        l.release("a");
        l.acquire("a", 0).expect("the slot came back");
    }

    /// Churn is limited even when nothing is held: create-and-destroy in a
    /// loop holds nothing at any instant and still saturates the daemon.
    #[test]
    fn a_tight_create_destroy_loop_is_rate_limited() {
        let mut l = limiter(100, 100, 4, 2);
        for i in 0..4 {
            l.acquire("loop", 0).unwrap_or_else(|e| panic!("burst {i}: {e}"));
            l.release("loop");
        }
        match l.acquire("loop", 0).unwrap_err() {
            Refusal::TooFast { allowed_per_sec } => assert_eq!(allowed_per_sec, 2),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let mut l = limiter(100, 100, 2, 2);
        l.acquire("x", 0).unwrap();
        l.acquire("x", 0).unwrap();
        assert!(l.acquire("x", 0).is_err(), "burst should be spent");
        // Half a second at 2/s is one token.
        l.acquire("x", 500).expect("a token should have refilled");
        assert!(l.acquire("x", 500).is_err(), "only one token had refilled");
    }

    /// ★ An hour of idleness must not buy an hour's worth of tokens.
    ///
    /// The first version of this test was VACUOUS: it made every call at the
    /// late timestamp, so the bucket was CREATED there, full, and the refill
    /// path never ran. Removing the cap from the implementation left it
    /// green. The bucket has to exist first and then idle, which is what a
    /// real client that pauses and resumes does.
    #[test]
    fn the_bucket_does_not_refill_past_its_burst() {
        let mut l = limiter(100, 100, 2, 2);
        l.acquire("x", 0).unwrap();                       // bucket exists, 1 token left
        l.release("x");
        // An hour later it may hold at most `burst`, not 7200 seconds' worth.
        for i in 0..2 {
            l.acquire("x", 3_600 * S).unwrap_or_else(|e| panic!("burst {i}: {e}"));
            l.release("x");
        }
        assert!(l.acquire("x", 3_600 * S).is_err(), "idling bought more than the burst");
    }

    /// ★ A REFUSAL MUST NOT COST A TOKEN. A client at its concurrency limit
    /// that retries would otherwise exhaust its rate budget too, and be
    /// punished twice for one condition.
    #[test]
    fn a_concurrency_refusal_does_not_spend_rate_budget() {
        let mut l = limiter(1, 10, 4, 1);
        l.acquire("a", 0).unwrap();
        for _ in 0..10 { assert!(l.acquire("a", 0).is_err()); }
        l.release("a");
        // Three tokens must remain from the burst of four.
        for i in 0..3 { l.acquire("a", 0).unwrap_or_else(|e| panic!("retry {i}: {e}")); l.release("a"); }
    }

    /// A clock that steps backwards must not drain the bucket.
    #[test]
    fn a_backwards_clock_does_not_punish() {
        let mut l = limiter(100, 100, 2, 2);
        l.acquire("x", 10 * S).unwrap();
        l.acquire("x", 5 * S).unwrap_or_else(|e| panic!("backwards clock refused: {e}"));
    }

    /// The guard releases on every path, including a panic.
    #[test]
    fn the_slot_guard_releases_when_dropped() {
        use std::sync::{Arc, Mutex};
        let l = Arc::new(Mutex::new(limiter(1, 10, 100, 100)));
        {
            let _slot = Slot::acquire(&l, "a", 0).expect("acquires");
            assert!(Slot::acquire(&l, "a", 0).is_err(), "the slot is held");
        }
        Slot::acquire(&l, "a", 0).expect("dropped guard freed the slot");
    }

    #[test]
    fn a_panicking_holder_still_frees_its_slot() {
        use std::sync::{Arc, Mutex};
        let l = Arc::new(Mutex::new(limiter(1, 10, 100, 100)));
        let l2 = l.clone();
        let _ = std::thread::spawn(move || {
            let _slot = Slot::acquire(&l2, "a", 0).expect("acquires");
            panic!("worker thread died");
        }).join();
        Slot::acquire(&l, "a", 0).expect("a panicking thread must not leak its slot");
    }
}
