//! Boot order for the supervised services (docs/spec/fast-boot.md §6).
//!
//! Pure bookkeeping, no I/O: which services may be launched now, given each
//! one's `after = [...]` edges, its boot class and what has become ready,
//! failed or given up so far. The supervisor asks [`BootGraph::launchable`]
//! after every event and launches what it returns.
//!
//! - A service starts as soon as everything it is `after` is ready. Services
//!   with no edges start at once.
//! - `login` services come first, then `system`; `idle` ones wait until every
//!   `login` service is ready (or has failed), so they do not compete with the
//!   path to the login screen.
//! - A service whose dependency failed is *held*, never started, and says why.
//!   That holds transitively. Nothing crash-loops on a missing dependency.
//! - A cycle is refused when the graph is built, naming every service in it.
//! - An `after` name that is not an enabled manifest is dropped with a
//!   warning: host rc daemons (atrium-volumes, atrium-net) are not managed
//!   here, and the bootstrap waits for those itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default,
         serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootClass {
    /// On the path to the login screen: frescod, ostiarius.
    Login,
    #[default]
    System,
    /// Started once every login service is ready.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Not launched yet: waiting for dependencies (or for its class's turn).
    Waiting,
    /// Launched; not ready yet (it reports readiness itself).
    Starting,
    Ready,
    /// Failed for good (gave up restarting, or never became ready in time).
    Failed(String),
    /// Never started because a dependency failed.
    Held { because: String },
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name:  String,
    pub after: Vec<String>,
    pub class: BootClass,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Cycle(pub Vec<String>);

impl fmt::Display for Cycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dependency cycle: {}", self.0.join(" -> "))
    }
}

pub struct BootGraph {
    /// Manifest order (file name), kept so launches stay predictable.
    order: Vec<String>,
    after: BTreeMap<String, Vec<String>>,
    class: BTreeMap<String, BootClass>,
    state: BTreeMap<String, State>,
    /// `after` names that are not enabled manifests (dropped).
    pub unknown: Vec<(String, String)>,
    /// Cycles refused at build time; their members are failed.
    pub cycles: Vec<Cycle>,
}

impl BootGraph {
    /// Build the graph. A cycle does not stop the others: every service in
    /// it is marked failed ("dependency cycle: a -> b -> a") and whatever
    /// waits for one of them is held. The cycles found are in `cycles`.
    pub fn new(nodes: Vec<Node>) -> BootGraph {
        let names: BTreeSet<String> = nodes.iter().map(|n| n.name.clone()).collect();
        let mut g = BootGraph {
            order: nodes.iter().map(|n| n.name.clone()).collect(),
            after: BTreeMap::new(),
            class: BTreeMap::new(),
            state: BTreeMap::new(),
            unknown: Vec::new(),
            cycles: Vec::new(),
        };
        for n in nodes {
            let mut deps = Vec::new();
            for d in n.after {
                if !names.contains(&d) {
                    g.unknown.push((n.name.clone(), d));
                } else if !deps.contains(&d) {
                    deps.push(d);
                }
            }
            g.after.insert(n.name.clone(), deps);
            g.class.insert(n.name.clone(), n.class);
            g.state.insert(n.name, State::Waiting);
        }
        while let Some(c) = g.find_cycle() {
            let why = Cycle(c.clone()).to_string();
            for m in &c {
                g.state.insert(m.clone(), State::Failed(why.clone()));
                // Break it so the search moves on to the next cycle.
                g.after.get_mut(m).expect("member").retain(|d| !c.contains(d));
            }
            g.cycles.push(Cycle(c));
        }
        if !g.cycles.is_empty() { g.hold_dependents(); }
        g
    }

    fn find_cycle(&self) -> Option<Vec<String>> {
        // Iterative-enough DFS: the graph is a few dozen services at most.
        #[derive(Clone, Copy, PartialEq)] enum Mark { New, Open, Done }
        let mut mark: BTreeMap<&str, Mark> = self.order.iter().map(|n| (n.as_str(), Mark::New)).collect();
        fn visit<'a>(g: &'a BootGraph, n: &'a str, mark: &mut BTreeMap<&'a str, Mark>,
                     path: &mut Vec<&'a str>) -> Option<Vec<String>> {
            mark.insert(n, Mark::Open);
            path.push(n);
            for d in &g.after[n] {
                match mark[d.as_str()] {
                    Mark::Open => {
                        let i = path.iter().position(|p| *p == d).unwrap_or(0);
                        let mut c: Vec<String> = path[i..].iter().map(|s| s.to_string()).collect();
                        c.push(d.clone());
                        return Some(c);
                    }
                    Mark::New => if let Some(c) = visit(g, d, mark, path) { return Some(c); },
                    Mark::Done => {}
                }
            }
            path.pop();
            mark.insert(n, Mark::Done);
            None
        }
        for n in &self.order {
            if mark[n.as_str()] == Mark::New {
                if let Some(c) = visit(self, n, &mut mark, &mut Vec::new()) { return Some(c); }
            }
        }
        None
    }

    pub fn state(&self, name: &str) -> Option<&State> { self.state.get(name) }

    /// Services to launch now, login class first, then manifest order.
    /// The caller launches each and reports back with
    /// [`launched`](Self::launched) (or [`failed`](Self::failed)).
    pub fn launchable(&self) -> Vec<String> {
        let login_settled = self.order.iter()
            .filter(|n| self.class[*n] == BootClass::Login)
            .all(|n| matches!(self.state[n], State::Ready | State::Failed(_) | State::Held { .. }));
        let mut out: Vec<&String> = self.order.iter().filter(|n| {
            self.state[*n] == State::Waiting
                && (self.class[*n] != BootClass::Idle || login_settled)
                && self.after[*n].iter().all(|d| self.state[d] == State::Ready)
        }).collect();
        out.sort_by_key(|n| self.class[*n]); // stable: manifest order within a class
        out.into_iter().cloned().collect()
    }

    pub fn launched(&mut self, name: &str) {
        if let Some(s) = self.state.get_mut(name) {
            if *s == State::Waiting { *s = State::Starting; }
        }
    }

    /// It reported ready (or, without `[ready] notify`, it was launched).
    /// Also after a restart: a service that was ready once is ready again.
    pub fn ready(&mut self, name: &str) {
        if let Some(s) = self.state.get_mut(name) {
            if matches!(s, State::Starting | State::Waiting) { *s = State::Ready; }
        }
    }

    /// It failed for good. Every service that (transitively) waits for it and
    /// has not started is held; returns those, in manifest order.
    pub fn failed(&mut self, name: &str, why: &str) -> Vec<String> {
        if let Some(s) = self.state.get_mut(name) {
            *s = State::Failed(why.to_string());
        }
        self.hold_dependents()
    }

    fn hold_dependents(&mut self) -> Vec<String> {
        let mut held = Vec::new();
        loop {
            let next = self.order.iter().find(|n| {
                self.state[*n] == State::Waiting
                    && self.after[*n].iter().any(|d| matches!(self.state[d], State::Failed(_) | State::Held { .. }))
            }).cloned();
            let Some(n) = next else { break };
            let culprit = self.after[&n].iter()
                .find(|d| matches!(self.state[*d], State::Failed(_) | State::Held { .. }))
                .cloned().unwrap_or_default();
            let because = match &self.state[&culprit] {
                State::Failed(w) => format!("waiting for {culprit} (failed: {w})"),
                State::Held { because } => format!("waiting for {culprit} ({because})"),
                _ => format!("waiting for {culprit}"),
            };
            self.state.insert(n.clone(), State::Held { because });
            held.push(n);
        }
        held
    }

    /// Ready services, in manifest order.
    pub fn ready_names(&self) -> Vec<&str> {
        self.order.iter().filter(|n| self.state[*n] == State::Ready).map(|n| n.as_str()).collect()
    }

    /// One line per service that is not ready, for the log and status:
    /// "atrium-x: waiting for atrium-y (failed: ...)".
    pub fn not_ready(&self) -> Vec<String> {
        self.order.iter().filter_map(|n| match &self.state[n] {
            State::Ready => None,
            State::Starting => Some(format!("{n}: started, not ready yet")),
            State::Failed(w) => Some(format!("{n}: failed: {w}")),
            State::Held { because } => Some(format!("{n}: held, {because}")),
            State::Waiting => {
                let pending: Vec<&str> = self.after[n].iter()
                    .filter(|d| self.state[*d] != State::Ready).map(|d| d.as_str()).collect();
                Some(if pending.is_empty() {
                    format!("{n}: waiting for the login services (idle class)")
                } else {
                    format!("{n}: waiting for {}", pending.join(", "))
                })
            }
        }).collect()
    }

    /// Every service has settled: nothing is waiting or starting.
    pub fn settled(&self) -> bool {
        self.state.values().all(|s| !matches!(s, State::Waiting | State::Starting))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, after: &[&str], class: BootClass) -> Node {
        Node { name: name.into(), after: after.iter().map(|s| s.to_string()).collect(), class }
    }
    use BootClass::*;

    #[test]
    fn no_edges_all_launch_at_once_login_first() {
        let g = BootGraph::new(vec![n("a", &[], System), n("b", &[], Login), n("c", &[], System)]);
        assert_eq!(g.launchable(), vec!["b", "a", "c"]);
    }

    #[test]
    fn after_waits_for_ready_not_for_launch() {
        let mut g = BootGraph::new(vec![n("db", &[], System), n("app", &["db"], System)]);
        assert_eq!(g.launchable(), vec!["db"]);
        g.launched("db");
        assert!(g.launchable().is_empty(), "app must wait until db is ready");
        assert_eq!(g.not_ready(), vec!["db: started, not ready yet", "app: waiting for db"]);
        g.ready("db");
        assert_eq!(g.launchable(), vec!["app"]);
        g.launched("app");
        g.ready("app");
        assert!(g.settled());
    }

    #[test]
    fn idle_waits_for_every_login_service() {
        let mut g = BootGraph::new(vec![
            n("sheet", &[], Idle), n("fresco", &[], Login), n("ostiarius", &[], Login), n("lyrad", &[], System),
        ]);
        assert_eq!(g.launchable(), vec!["fresco", "ostiarius", "lyrad"]);
        for s in ["fresco", "ostiarius", "lyrad"] { g.launched(s); }
        g.ready("fresco");
        assert!(g.launchable().is_empty());
        // A failed login service does not hold idle work forever.
        g.failed("ostiarius", "gave up");
        assert_eq!(g.launchable(), vec!["sheet"]);
    }

    #[test]
    fn a_failed_dependency_holds_its_dependents_transitively() {
        let mut g = BootGraph::new(vec![
            n("arcad", &[], System), n("courier", &["arcad"], System),
            n("mail", &["courier"], System), n("other", &[], System),
        ]);
        g.launched("arcad");
        g.launched("other");
        assert_eq!(g.failed("arcad", "failure budget exhausted"), vec!["courier", "mail"]);
        assert_eq!(g.state("courier"), Some(&State::Held {
            because: "waiting for arcad (failed: failure budget exhausted)".into() }));
        assert_eq!(g.state("mail"), Some(&State::Held {
            because: "waiting for courier (waiting for arcad (failed: failure budget exhausted))".into() }));
        assert!(g.launchable().is_empty());
        g.ready("other");
        assert!(g.settled());
    }

    #[test]
    fn cycles_fail_their_members_and_hold_dependents_only() {
        let mut g = BootGraph::new(vec![
            n("a", &["c"], System), n("b", &["a"], System), n("c", &["b"], System),
            n("d", &["b"], System), n("e", &[], System),
        ]);
        assert_eq!(g.cycles.len(), 1);
        let c = &g.cycles[0].0;
        assert_eq!(c.first(), c.last());
        assert_eq!(c.len(), 4);
        assert!(matches!(g.state("a"), Some(State::Failed(w)) if w.starts_with("dependency cycle: ")));
        assert!(matches!(g.state("d"), Some(State::Held { because }) if because.starts_with("waiting for b (failed: dependency cycle")));
        assert_eq!(g.launchable(), vec!["e"]);
        g.launched("e");
        g.ready("e");
        assert!(g.settled());
        let s = BootGraph::new(vec![n("a", &["a"], System)]);
        assert_eq!(s.cycles, vec![Cycle(vec!["a".into(), "a".into()])]);
    }

    #[test]
    fn unknown_names_are_dropped_not_waited_for() {
        let g = BootGraph::new(vec![n("lyrad", &["atrium-volumes"], System)]);
        assert_eq!(g.unknown, vec![("lyrad".to_string(), "atrium-volumes".to_string())]);
        assert_eq!(g.launchable(), vec!["lyrad"]);
    }

    #[test]
    fn restart_keeps_ready_and_late_ready_is_harmless() {
        let mut g = BootGraph::new(vec![n("a", &[], System)]);
        g.launched("a");
        g.ready("a");
        g.launched("a"); // relaunch after a crash: still ready for its dependents
        assert_eq!(g.state("a"), Some(&State::Ready));
        g.failed("a", "x");
        g.ready("a"); // a stale report after failing does not resurrect it
        assert_eq!(g.state("a"), Some(&State::Failed("x".into())));
    }
}
