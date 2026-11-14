//! Which interface carries the default route (docs/spec/atrium-net.md §4.1,
//! decision 9). Pure: `main` feeds it each interface's state and applies the
//! answer.
//!
//! - Only a bound interface with a router and a live link is a candidate.
//! - Better is: internet not known to be broken, then wired over Wi-Fi.
//! - Losing the current egress (link down, lease gone) moves at once.
//! - Moving to a better interface waits until it has been that good for its
//!   hold-down: 5 s, doubled for each flap within 2 min of the last one (a
//!   flap is its link dropping or its internet failing while it carried the
//!   route), up to 5 min. So a bad cable or a flaky upstream cannot make the
//!   route bounce.
//! - With no candidate left, an interface that is still bound (Wi-Fi in its
//!   60 s grace after a drop) keeps the route; with nothing bound, none.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inet {
    /// Not probed yet (or probing is off): treated as working.
    Unknown,
    Yes,
    No,
    /// An HTTP answer that was not the probe's: a sign-in page.
    Captive,
}

impl Inet {
    pub fn ok(self) -> bool {
        matches!(self, Inet::Unknown | Inet::Yes)
    }

    pub fn word(self) -> &'static str {
        match self {
            Inet::Unknown => "unknown",
            Inet::Yes => "yes",
            Inet::No => "no",
            Inet::Captive => "captive",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cand {
    /// Has a checked lease with a router.
    pub bound: bool,
    /// Link up (Wi-Fi: associated with the port open).
    pub link: bool,
    pub wired: bool,
    pub inet: Inet,
}

impl Cand {
    fn eligible(&self) -> bool {
        self.bound && self.link
    }

    fn score(&self) -> u8 {
        (self.inet.ok() as u8) << 1 | self.wired as u8
    }
}

pub const HOLD_MS: u64 = 5_000;
pub const HOLD_MAX_MS: u64 = 300_000;
pub const FLAP_WINDOW_MS: u64 = 120_000;

#[derive(Clone, Debug, Default)]
struct Track {
    /// Since when it has been eligible with its current score.
    since: Option<(u64, u8)>,
    /// Flaps in a row, each within FLAP_WINDOW_MS of the last.
    flaps: u32,
    last_flap: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Egress {
    pub cur: Option<usize>,
    t: Vec<Track>,
}

impl Egress {
    pub fn new(n: usize) -> Egress {
        Egress { cur: None, t: vec![Track::default(); n] }
    }

    /// Its link dropped, or its internet failed while it had the route.
    pub fn flap(&mut self, k: usize, now: u64) {
        let t = &mut self.t[k];
        t.flaps = if t.flaps > 0 && now.saturating_sub(t.last_flap) < FLAP_WINDOW_MS { t.flaps + 1 } else { 1 };
        t.last_flap = now;
    }

    /// How long `k` must stay good before the route moves to it.
    pub fn hold_ms(&self, k: usize, now: u64) -> u64 {
        let t = &self.t[k];
        let n = if now.saturating_sub(t.last_flap) < FLAP_WINDOW_MS { t.flaps } else { 0 };
        (HOLD_MS << n.saturating_sub(1).min(10)).min(HOLD_MAX_MS)
    }

    /// The interface that should carry the default route now. Also returns
    /// when the answer could next change by time alone (a hold ending).
    pub fn decide(&mut self, c: &[Cand], now: u64) -> (Option<usize>, Option<u64>) {
        for (k, x) in c.iter().enumerate() {
            let t = &mut self.t[k];
            if !x.eligible() {
                t.since = None;
            } else if t.since.map(|(_, s)| s) != Some(x.score()) {
                t.since = Some((now, x.score()));
            }
        }
        let best = (0..c.len()).filter(|&k| c[k].eligible()).max_by_key(|&k| (c[k].score(), std::cmp::Reverse(k)));
        let mut wake = None;
        self.cur = match self.cur {
            Some(cur) if cur < c.len() && c[cur].eligible() => match best {
                Some(b) if b != cur && c[b].score() > c[cur].score() => {
                    let since = self.t[b].since.map_or(now, |(s, _)| s);
                    let due = since + self.hold_ms(b, now);
                    if now >= due {
                        Some(b)
                    } else {
                        wake = Some(due);
                        Some(cur)
                    }
                }
                _ => Some(cur),
            },
            // Lost the current one (or none yet): the best at once; else keep
            // a still-bound one (Wi-Fi in its grace period).
            cur => best.or(cur.filter(|&k| k < c.len() && c[k].bound)),
        };
        (self.cur, wake)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIRED: usize = 0;
    const WIFI: usize = 1;

    fn c(bound: bool, link: bool, wired: bool, inet: Inet) -> Cand {
        Cand { bound, link, wired, inet }
    }

    fn both(wired_link: bool, wired_inet: Inet, wifi_inet: Inet) -> [Cand; 2] {
        [c(true, wired_link, true, wired_inet), c(true, true, false, wifi_inet)]
    }

    #[test]
    fn ethernet_preferred_and_cable_pull_moves_at_once() {
        let mut e = Egress::new(2);
        let u = Inet::Unknown;
        assert_eq!(e.decide(&both(true, u, u), 0).0, Some(WIRED));
        // Cable out: Wi-Fi at once.
        e.flap(WIRED, 1000);
        assert_eq!(e.decide(&both(false, u, u), 1000).0, Some(WIFI));
        // Cable back: 5 s before the route returns.
        assert_eq!(e.decide(&both(true, u, u), 2000), (Some(WIFI), Some(7000)));
        assert_eq!(e.decide(&both(true, u, u), 6999).0, Some(WIFI));
        assert_eq!(e.decide(&both(true, u, u), 7000).0, Some(WIRED));
    }

    #[test]
    fn flapping_cable_doubles_the_hold() {
        let mut e = Egress::new(2);
        let u = Inet::Unknown;
        e.decide(&both(true, u, u), 0);
        let mut t = 1000;
        for want in [5_000, 10_000, 20_000, 40_000] {
            e.flap(WIRED, t);
            assert_eq!(e.decide(&both(false, u, u), t).0, Some(WIFI));
            t += 1000;
            let (cur, wake) = e.decide(&both(true, u, u), t);
            assert_eq!((cur, wake), (Some(WIFI), Some(t + want)), "hold {want}");
            t += want;
            assert_eq!(e.decide(&both(true, u, u), t).0, Some(WIRED));
            t += 1000;
        }
        // Capped at 5 min.
        for _ in 0..10 {
            e.flap(WIRED, t);
            t += 1000;
        }
        assert_eq!(e.hold_ms(WIRED, t), HOLD_MAX_MS);
        // Quiet for 2 min: back to 5 s.
        assert_eq!(e.hold_ms(WIRED, t + FLAP_WINDOW_MS), HOLD_MS);
        e.flap(WIRED, t + FLAP_WINDOW_MS + 1);
        assert_eq!(e.hold_ms(WIRED, t + FLAP_WINDOW_MS + 1), HOLD_MS);
    }

    #[test]
    fn dead_upstream_moves_to_working_wifi_and_back_after_hold() {
        let mut e = Egress::new(2);
        assert_eq!(e.decide(&both(true, Inet::Yes, Inet::Yes), 0).0, Some(WIRED));
        // Ethernet's probe fails: Wi-Fi has been fine all along, so at once.
        e.flap(WIRED, 10_000);
        assert_eq!(e.decide(&both(true, Inet::No, Inet::Yes), 10_000).0, Some(WIFI));
        // Ethernet's internet back: held 5 s from when it came back.
        assert_eq!(e.decide(&both(true, Inet::Yes, Inet::Yes), 20_000), (Some(WIFI), Some(25_000)));
        assert_eq!(e.decide(&both(true, Inet::Yes, Inet::Yes), 25_000).0, Some(WIRED));
    }

    #[test]
    fn captive_is_not_internet() {
        let mut e = Egress::new(2);
        assert_eq!(e.decide(&both(true, Inet::Captive, Inet::Yes), 0).0, Some(WIFI));
    }

    #[test]
    fn single_interface_kept_without_internet() {
        let mut e = Egress::new(1);
        let one = |i| [c(true, true, false, i)];
        assert_eq!(e.decide(&one(Inet::Yes), 0).0, Some(0));
        assert_eq!(e.decide(&one(Inet::No), 1000).0, Some(0));
        assert_eq!(e.decide(&one(Inet::Captive), 2000).0, Some(0));
    }

    #[test]
    fn both_broken_prefers_wired_and_does_not_bounce() {
        let mut e = Egress::new(2);
        assert_eq!(e.decide(&both(true, Inet::No, Inet::No), 0).0, Some(WIRED));
        assert_eq!(e.decide(&both(true, Inet::No, Inet::Captive), 1000).0, Some(WIRED));
        // Same score on both sides: never moves.
        let mut e = Egress::new(2);
        e.decide(&[c(false, false, true, Inet::No), c(true, true, false, Inet::No)], 0);
        assert_eq!(e.decide(&both(true, Inet::No, Inet::No), 1000).0, Some(WIFI));
        assert_eq!(e.decide(&both(true, Inet::No, Inet::No), 100_000).0, Some(WIRED), "wired is better at equal internet");
    }

    #[test]
    fn wifi_in_grace_keeps_the_route_only_when_alone() {
        let mut e = Egress::new(2);
        let grace = [c(false, false, true, Inet::Unknown), c(true, false, false, Inet::Unknown)];
        e.decide(&[c(false, false, true, Inet::Unknown), c(true, true, false, Inet::Unknown)], 0);
        assert_eq!(e.decide(&grace, 1000).0, Some(WIFI));
        // Grace over, lease dropped: no default route.
        assert_eq!(e.decide(&[c(false, false, true, Inet::Unknown), c(false, false, false, Inet::Unknown)], 61_000).0, None);
        // Nothing at all to start with.
        assert_eq!(Egress::new(2).decide(&grace, 0).0, None);
    }

    #[test]
    fn worse_newcomer_never_takes_over() {
        let mut e = Egress::new(2);
        e.decide(&[c(true, true, true, Inet::Yes), c(false, false, false, Inet::Unknown)], 0);
        assert_eq!(e.decide(&both(true, Inet::Yes, Inet::Yes), 1_000_000).0, Some(WIRED));
    }
}
