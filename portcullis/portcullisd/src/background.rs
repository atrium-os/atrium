//! Background apps (background-apps.md §3.3): which of a person's apps run
//! while they are signed in, and whether an exit is restarted.
//!
//! Pure bookkeeping. The daemon's runner launches each instance through the
//! ordinary Launch path, tells this registry how it ended, and does what the
//! answer says. The rules here are the ones the person relies on:
//!
//! - A second `StartBackground` in a session starts nothing that is running,
//!   paused, quit or stopped: it never undoes a Quit.
//! - Once a person is signing out, nothing of theirs restarts: sign-out kills
//!   every app, and those exits are not failures.
//! - A clean exit (0), a launch that failed, `restart = "never"`, or a grant
//!   that no longer holds stops the instance until the next sign-in.
//! - More than [`MAX_RESTARTS`] restarts inside [`BURST_WINDOW`] pauses it
//!   for [`COOLDOWN`]; the grant is checked again before it resumes.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// The window restarts are counted in.
pub const BURST_WINDOW: Duration = Duration::from_secs(60);
/// Restarts allowed inside the window; the next failure pauses it. Two
/// restarts and a third crash in a minute pause it (the B2 check).
pub const MAX_RESTARTS: usize = 2;
/// How long a burst pauses restarts.
pub const COOLDOWN: Duration = Duration::from_secs(60);
/// At most this many background instances per person.
pub const MAX_PER_PERSON: usize = 32;

/// One app in one persona (`None` is Personal).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub app_id: String,
    pub persona: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    /// Restarts are paused until then.
    Paused(Instant),
    /// The person quit it: not started again this session.
    Quit,
    /// It ended and isn't restarted this session.
    Stopped,
}

#[derive(Debug)]
struct Inst {
    state: State,
    restarts: VecDeque<Instant>,
    /// Which supervising run this is: a thread left from an earlier
    /// session (or an earlier open) never acts on a newer one.
    run: Run,
}

/// A supervising run's number, unique for the daemon's life.
pub type Run = u64;

#[derive(Debug, Default)]
struct Person {
    stopping: bool,
    apps: BTreeMap<Key, Inst>,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by a signal.
    Signal,
    /// The launch itself failed (a locked persona, a refused manifest).
    LaunchFailed,
}

/// What the runner does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    Restart,
    /// Wait until then, then ask [`Registry::resume`].
    Pause(Instant),
    Stop(&'static str),
}

/// What the runner checked again before a restart: the installed manifest
/// still asks, the person's grant holds for it, and its restart policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recheck {
    pub allowed: bool,
    pub restart_never: bool,
}

#[derive(Debug, Default)]
pub struct Registry {
    people: HashMap<String, Person>,
    next_run: Run,
}

impl Registry {
    /// `user` signed in: of `wanted`, the instances to start now, which are
    /// those not seen yet this session. Nothing while they are signing out.
    pub fn sign_in(&mut self, user: &str, wanted: impl IntoIterator<Item = Key>) -> Vec<(Key, Run)> {
        let p = self.people.entry(user.into()).or_default();
        if p.stopping {
            return Vec::new();
        }
        let mut start = Vec::new();
        for k in wanted {
            if p.apps.len() >= MAX_PER_PERSON || p.apps.contains_key(&k) {
                continue;
            }
            self.next_run += 1;
            p.apps.insert(k.clone(), Inst { state: State::Running, restarts: VecDeque::new(), run: self.next_run });
            start.push((k, self.next_run));
        }
        start
    }

    /// The person opened `k` themselves (a `manual` app, or one they quit):
    /// whether it is now supervised. Only in a live session, and not while
    /// it already runs supervised.
    pub fn opened(&mut self, user: &str, k: &Key) -> Option<Run> {
        let p = self.people.get_mut(user).filter(|p| !p.stopping)?;
        let full = p.apps.len() >= MAX_PER_PERSON;
        let run = self.next_run + 1;
        match p.apps.get_mut(k) {
            Some(i) if matches!(i.state, State::Running | State::Paused(_)) => return None,
            Some(i) => *i = Inst { state: State::Running, restarts: VecDeque::new(), run },
            None if full => return None,
            None => { p.apps.insert(k.clone(), Inst { state: State::Running, restarts: VecDeque::new(), run }); }
        }
        self.next_run = run;
        Some(run)
    }

    /// `user` has a session this registry knows and is not signing out.
    pub fn live(&self, user: &str) -> bool {
        self.people.get(user).is_some_and(|p| !p.stopping)
    }

    /// `k`'s run ended with `exit` at `now`; `check` is what the runner
    /// found when it looked again.
    pub fn on_exit(&mut self, user: &str, k: &Key, run: Run, exit: Exit, now: Instant, check: Recheck) -> Next {
        let Some(p) = self.people.get_mut(user) else { return Next::Stop("signed out") };
        if p.stopping {
            return Next::Stop("signing out");
        }
        let Some(i) = p.apps.get_mut(k).filter(|i| i.run == run) else { return Next::Stop("not supervised") };
        if i.state == State::Quit {
            return Next::Stop("quit");
        }
        let stop = match exit {
            Exit::Code(0) => Some("exited cleanly"),
            Exit::LaunchFailed => Some("could not start"),
            _ if check.restart_never => Some("restart = \"never\""),
            _ if !check.allowed => Some("no longer allowed"),
            _ => None,
        };
        if let Some(why) = stop {
            i.state = State::Stopped;
            return Next::Stop(why);
        }
        while i.restarts.front().is_some_and(|t| now.duration_since(*t) >= BURST_WINDOW) {
            i.restarts.pop_front();
        }
        if i.restarts.len() >= MAX_RESTARTS {
            let until = now + COOLDOWN;
            i.state = State::Paused(until);
            i.restarts.clear();
            return Next::Pause(until);
        }
        i.restarts.push_back(now);
        i.state = State::Running;
        Next::Restart
    }

    /// The pause of `k` is over: restart it if it is still paused, the
    /// person is still signed in, and the grant still holds.
    pub fn resume(&mut self, user: &str, k: &Key, run: Run, check: Recheck) -> Next {
        let Some(p) = self.people.get_mut(user).filter(|p| !p.stopping) else { return Next::Stop("signing out") };
        let Some(i) = p.apps.get_mut(k).filter(|i| i.run == run) else { return Next::Stop("not supervised") };
        if !matches!(i.state, State::Paused(_)) {
            return Next::Stop("quit");
        }
        if !check.allowed || check.restart_never {
            i.state = State::Stopped;
            return Next::Stop("no longer allowed");
        }
        i.state = State::Running;
        Next::Restart
    }

    /// The person quit `k`: whether it was running or paused (so the
    /// runner should end its jail).
    pub fn quit(&mut self, user: &str, k: &Key) -> bool {
        let Some(i) = self.people.get_mut(user).and_then(|p| p.apps.get_mut(k)) else { return false };
        let was = matches!(i.state, State::Running | State::Paused(_));
        if was {
            i.state = State::Quit;
        }
        was
    }

    /// Stop tracking `k`: it runs outside the registry (the person opened it
    /// before sign-in's start reached it).
    pub fn forget(&mut self, user: &str, k: &Key) {
        if let Some(p) = self.people.get_mut(user) {
            p.apps.remove(k);
        }
    }

    /// `user` is signing out: nothing of theirs restarts from now on.
    pub fn stopping(&mut self, user: &str) {
        if let Some(p) = self.people.get_mut(user) {
            p.stopping = true;
        }
    }

    /// `user`'s sign-out is done: forget them, so the next sign-in starts
    /// afresh.
    pub fn ended(&mut self, user: &str) {
        self.people.remove(user);
    }

    /// `user`'s instances and their states, by app then persona.
    pub fn list(&self, user: &str) -> Vec<(Key, State)> {
        self.people.get(user).map(|p| p.apps.iter().map(|(k, i)| (k.clone(), i.state)).collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(app: &str, persona: Option<&str>) -> Key {
        Key { app_id: app.into(), persona: persona.map(String::from) }
    }
    /// The run the registry holds for `k` now.
    fn run(r: &Registry, user: &str, k: &Key) -> Run {
        r.people.get(user).and_then(|p| p.apps.get(k)).map(|i| i.run).unwrap_or(0)
    }
    const OK: Recheck = Recheck { allowed: true, restart_never: false };

    #[test]
    fn a_second_sign_in_starts_nothing_twice_and_never_undoes_a_quit() {
        let mut r = Registry::default();
        let bar = k("org.x.bar", None);
        let work = k("org.x.bar", Some("work"));
        assert_eq!(r.sign_in("alice", [bar.clone(), work.clone()]).len(), 2);
        assert!(r.sign_in("alice", [bar.clone()]).is_empty(), "running already");
        assert!(r.quit("alice", &bar));
        assert!(r.sign_in("alice", [bar.clone(), work.clone()]).is_empty(), "a quit stays quit");
        assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, Instant::now(), OK), Next::Stop("quit"));
        // The person may open it again themselves.
        let old = run(&r, "alice", &bar);
        assert!(r.opened("alice", &bar).is_some());
        assert!(r.opened("alice", &bar).is_none(), "already supervised");
        // The thread of the run it replaced never acts on the new one.
        assert_eq!(r.on_exit("alice", &bar, old, Exit::Signal, Instant::now(), OK), Next::Stop("not supervised"));
        assert!(r.sign_in("bob", [bar.clone()]).len() == 1, "per person");
    }

    #[test]
    fn crashes_restart_until_a_burst_pauses_them() {
        let mut r = Registry::default();
        let bar = k("org.x.bar", None);
        r.sign_in("alice", [bar.clone()]);
        let t0 = Instant::now();
        assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t0, OK), Next::Restart, "kill -9 restarts it");
        assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Code(1), t0 + Duration::from_secs(5), OK), Next::Restart);
        let third = t0 + Duration::from_secs(10);
        assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, third, OK), Next::Pause(third + COOLDOWN), "third crash in a minute");
        assert_eq!(r.list("alice"), vec![(bar.clone(), State::Paused(third + COOLDOWN))]);
        assert_eq!(r.resume("alice", &bar, run(&r, "alice", &bar), OK), Next::Restart);
        // Crashes spread out never pause it.
        let mut t = third + COOLDOWN;
        for _ in 0..5 {
            t += BURST_WINDOW;
            assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t, OK), Next::Restart);
        }
    }

    #[test]
    fn what_stops_an_instance_for_the_session() {
        let now = Instant::now();
        for (exit, check, why) in [
            (Exit::Code(0), OK, "exited cleanly"),
            (Exit::LaunchFailed, OK, "could not start"),
            (Exit::Signal, Recheck { allowed: false, restart_never: false }, "no longer allowed"),
            (Exit::Code(3), Recheck { allowed: true, restart_never: true }, "restart = \"never\""),
        ] {
            let mut r = Registry::default();
            let bar = k("org.x.bar", None);
            r.sign_in("alice", [bar.clone()]);
            assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), exit, now, check), Next::Stop(why));
            assert_eq!(r.list("alice"), vec![(bar.clone(), State::Stopped)]);
            assert!(r.sign_in("alice", [bar.clone()]).is_empty(), "until the next sign-in");
        }
    }

    #[test]
    fn sign_out_kills_are_not_failures_and_the_next_sign_in_starts_afresh() {
        let mut r = Registry::default();
        let bar = k("org.x.bar", None);
        r.sign_in("alice", [bar.clone()]);
        r.stopping("alice");
        assert!(!r.live("alice"));
        assert_eq!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, Instant::now(), OK), Next::Stop("signing out"));
        assert!(r.sign_in("alice", [bar.clone()]).is_empty(), "not while signing out");
        assert!(r.opened("alice", &bar).is_none());
        let old = run(&r, "alice", &bar);
        r.ended("alice");
        assert_eq!(r.sign_in("alice", [bar.clone()]).len(), 1);
        // A thread from the last session, ending after this sign-in, doesn't
        // restart anything: the new session's run is another.
        assert_eq!(r.on_exit("alice", &bar, old, Exit::Signal, Instant::now(), OK), Next::Stop("not supervised"));
        // A pause that ends after sign-out doesn't restart.
        let t = Instant::now();
        r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t, OK);
        r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t, OK);
        assert!(matches!(r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t, OK), Next::Pause(_)));
        r.stopping("alice");
        assert_eq!(r.resume("alice", &bar, run(&r, "alice", &bar), OK), Next::Stop("signing out"));
    }

    #[test]
    fn a_revoked_grant_is_not_resumed_and_the_registry_is_bounded() {
        let mut r = Registry::default();
        let bar = k("org.x.bar", None);
        r.sign_in("alice", [bar.clone()]);
        let t = Instant::now();
        for _ in 0..2 { r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t, OK); }
        r.on_exit("alice", &bar, run(&r, "alice", &bar), Exit::Signal, t, OK);
        assert_eq!(r.resume("alice", &bar, run(&r, "alice", &bar), Recheck { allowed: false, restart_never: false }), Next::Stop("no longer allowed"));
        let many: Vec<Key> = (0..MAX_PER_PERSON + 5).map(|i| k(&format!("org.x.a{i}"), None)).collect();
        assert_eq!(r.sign_in("bob", many).len(), MAX_PER_PERSON);
        assert!(r.opened("bob", &k("org.x.more", None)).is_none());
        assert!(r.opened("carol", &bar).is_none(), "no session: not supervised");
    }
}
