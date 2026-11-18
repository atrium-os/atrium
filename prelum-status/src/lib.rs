//! What the person sees while it prints (printing.md §6.6), as pure logic:
//! [`Notices`] turns prelumd's job list, each time it changes, into the
//! Praeco notices to post and withdraw, and [`panel`] into the Forum status
//! item. The binary (`prelum-status`) does the talking.
//!
//! - A notice when the person must act: out of paper, a jam, a door or tray
//!   open, the printer not the one saved; offline only once it has lasted
//!   [`Notices::offline_after`] (2 minutes). It is withdrawn when the job
//!   moves on.
//! - A notice when a job fails; "Printed" as a quiet one (Praeco's LOW,
//!   which the shelf keeps without a toast). Nothing for a cancel: the
//!   person did it.
//! - Jobs that had already ended when it started are not told again.

use std::collections::{HashMap, HashSet};

use forum_ctl::{StatusContent, StatusNode, StatusPanel, StatusText};
use prelum_proto::print::{Alert, Job, Phase};

/// Praeco's urgencies (praeco::proto::urgency).
pub const LOW: u8 = 0;
pub const NORMAL: u8 = 1;
pub const HIGH: u8 = 2;

/// What a notice is about: withdrawn by the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Alert(u64, Alert),
    Ended(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Post { key: Key, urgency: u8, title: String, body: String },
    Withdraw(Key),
}

pub struct Notices {
    pub offline_after: u64,
    /// Ended jobs already told (or ended before we started).
    told_ended: HashSet<u64>,
    /// The alert notices up now.
    up: HashSet<Key>,
    started: bool,
}

impl Notices {
    pub fn new(offline_after: u64) -> Notices {
        Notices { offline_after, told_ended: HashSet::new(), up: HashSet::new(), started: false }
    }

    /// The jobs now, the printers' names, the time (seconds since the
    /// epoch): what to post and withdraw.
    pub fn update(&mut self, jobs: &[Job], names: &HashMap<u32, String>, now: u64) -> Vec<Action> {
        let mut out = Vec::new();
        let first = !self.started;
        self.started = true;
        let name = |id: u32| names.get(&id).cloned().unwrap_or_else(|| "The printer".into());
        let mut wanted: HashSet<Key> = HashSet::new();
        for j in jobs {
            if j.phase.ended() {
                if self.told_ended.insert(j.id) && !first {
                    let printer = name(j.printer);
                    match j.phase {
                        Phase::Done => out.push(Action::Post {
                            key: Key::Ended(j.id),
                            urgency: LOW,
                            title: format!("Printed \u{201c}{}\u{201d}", j.title),
                            body: format!("On {printer}."),
                        }),
                        Phase::Failed => out.push(Action::Post {
                            key: Key::Ended(j.id),
                            urgency: NORMAL,
                            title: format!("\u{201c}{}\u{201d} didn't print", j.title),
                            body: if j.message.is_empty() { format!("On {printer}.") } else { format!("On {printer}: {}.", j.message) },
                        }),
                        _ => {}
                    }
                }
                continue;
            }
            let Some(a) = j.alert else { continue };
            let due = match a {
                Alert::Stopped => false,
                Alert::Offline => now.saturating_sub(j.alert_since) >= self.offline_after,
                _ => true,
            };
            if due {
                let k = Key::Alert(j.id, a);
                wanted.insert(k);
                if self.up.insert(k) {
                    let (title, body) = a.words(&name(j.printer));
                    out.push(Action::Post { key: k, urgency: HIGH, title, body });
                }
            }
        }
        let gone: Vec<Key> = self.up.iter().filter(|k| !wanted.contains(k)).copied().collect();
        for k in gone {
            self.up.remove(&k);
            out.push(Action::Withdraw(k));
        }
        out
    }

    /// An offline alert that will come due: seconds until then, if any.
    pub fn next_due(&self, jobs: &[Job], now: u64) -> Option<u64> {
        jobs.iter()
            .filter(|j| !j.phase.ended() && j.alert == Some(Alert::Offline) && !self.up.contains(&Key::Alert(j.id, Alert::Offline)))
            .map(|j| (j.alert_since + self.offline_after).saturating_sub(now))
            .min()
    }
}

/// The status item's panel while any job is active, `None` when none is.
/// Each job's Cancel button has the job's number as its id.
pub fn panel(jobs: &[Job], names: &HashMap<u32, String>) -> Option<(String, StatusContent)> {
    let active: Vec<&Job> = jobs.iter().filter(|j| !j.phase.ended()).collect();
    if active.is_empty() {
        return None;
    }
    let mut nodes = vec![StatusNode::Heading { text: "Printing".into() }];
    for (i, j) in active.iter().enumerate() {
        if i > 0 {
            nodes.push(StatusNode::Separator);
        }
        let printer = names.get(&j.printer).cloned().unwrap_or_else(|| "The printer".into());
        nodes.push(StatusNode::Text { text: j.title.clone(), style: StatusText::Body });
        nodes.push(StatusNode::Text { text: format!("{printer} \u{b7} {}", progress(j)), style: StatusText::Secondary });
        if j.of > 0 {
            nodes.push(StatusNode::Progress { label: String::new(), value: j.sheets as f32 / j.of as f32 });
        }
        nodes.push(StatusNode::Button { id: (j.id & 0xffff_ffff) as u32, label: "Cancel".into(), primary: false, enabled: true });
    }
    let tooltip = match active.len() {
        1 => format!("Printing \u{201c}{}\u{201d}", active[0].title),
        n => format!("Printing {n} documents"),
    };
    Some((tooltip, StatusContent::Panel(StatusPanel { width: 300, nodes })))
}

/// Where a job is, in words.
pub fn progress(j: &Job) -> String {
    let page = |n: u32| if j.of > 0 { format!("page {} of {}", n.max(1).min(j.of), j.of) } else { String::new() };
    let base = match j.phase {
        Phase::Pending => "Waiting".to_string(),
        Phase::Preparing if j.of > 0 => format!("Preparing {}", page(j.sheets)),
        Phase::Preparing => "Preparing".to_string(),
        Phase::Sending => "Sending".to_string(),
        Phase::Printing if j.of > 0 => format!("Printing {}", page(j.sheets + 1)),
        Phase::Printing => "Printing".to_string(),
        Phase::Held => "Held".to_string(),
        Phase::Done => "Printed".to_string(),
        Phase::Cancelled => "Cancelled".to_string(),
        Phase::Failed => "Failed".to_string(),
    };
    if !j.message.is_empty() && matches!(j.phase, Phase::Pending | Phase::Held | Phase::Printing) {
        format!("{base}: {}", j.message)
    } else {
        base
    }
}

/// The status item's icon: a printer, in the bar's 0..24 stroke style.
pub const ICON: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9V3h12v6"/><path d="M6 18H4a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2h-2"/><path d="M6 14h12v7H6z"/></svg>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, phase: Phase, alert: Option<Alert>, since: u64) -> Job {
        Job { id, printer: 1, title: "Q3".into(), phase, sheets: 1, of: 3, message: String::new(), submitted: 0, alert, alert_since: since }
    }

    fn names() -> HashMap<u32, String> {
        [(1, "Office".to_string())].into()
    }

    #[test]
    fn out_of_paper_is_told_at_once_and_withdrawn_when_it_goes_on() {
        let mut n = Notices::new(120);
        assert!(n.update(&[], &names(), 1000).is_empty());
        let a = n.update(&[job(1, Phase::Pending, Some(Alert::OutOfPaper), 1000)], &names(), 1000);
        assert_eq!(a, vec![Action::Post { key: Key::Alert(1, Alert::OutOfPaper), urgency: HIGH, title: "Office is out of paper".into(), body: "Add paper and the job carries on.".into() }]);
        // Asked again, the same: nothing new.
        assert!(n.update(&[job(1, Phase::Pending, Some(Alert::OutOfPaper), 1000)], &names(), 1001).is_empty());
        let a = n.update(&[job(1, Phase::Printing, None, 0)], &names(), 1010);
        assert_eq!(a, vec![Action::Withdraw(Key::Alert(1, Alert::OutOfPaper))]);
    }

    #[test]
    fn offline_waits_two_minutes() {
        let mut n = Notices::new(120);
        n.update(&[], &names(), 0);
        let j = [job(2, Phase::Pending, Some(Alert::Offline), 1000)];
        assert!(n.update(&j, &names(), 1100).is_empty());
        assert_eq!(n.next_due(&j, 1100), Some(20));
        let a = n.update(&j, &names(), 1120);
        assert!(matches!(&a[..], [Action::Post { key: Key::Alert(2, Alert::Offline), title, .. }] if title == "Office is offline"));
        assert_eq!(n.next_due(&j, 1121), None);
    }

    #[test]
    fn endings_are_told_once_and_not_those_from_before() {
        let mut n = Notices::new(120);
        // Already ended when it started: not told.
        assert!(n.update(&[job(1, Phase::Done, None, 0)], &names(), 0).is_empty());
        let a = n.update(&[job(1, Phase::Done, None, 0), job(2, Phase::Done, None, 0)], &names(), 1);
        assert!(matches!(&a[..], [Action::Post { key: Key::Ended(2), urgency: LOW, title, .. }] if title == "Printed \u{201c}Q3\u{201d}"));
        let mut f = job(3, Phase::Failed, None, 0);
        f.message = "the printer stopped the job".into();
        let a = n.update(&[f.clone()], &names(), 2);
        assert!(matches!(&a[..], [Action::Post { key: Key::Ended(3), urgency: NORMAL, body, .. }] if body == "On Office: the printer stopped the job."));
        assert!(n.update(&[f, job(4, Phase::Cancelled, None, 0)], &names(), 3).is_empty(), "a cancel is the person's own doing");
    }

    #[test]
    fn the_panel_shows_active_jobs_with_cancel() {
        assert!(panel(&[job(1, Phase::Done, None, 0)], &names()).is_none());
        let mut j = job(7, Phase::Printing, None, 0);
        j.sheets = 1;
        let (tip, c) = panel(&[j], &names()).unwrap();
        assert_eq!(tip, "Printing \u{201c}Q3\u{201d}");
        let StatusContent::Panel(p) = c else { panic!() };
        assert!(p.nodes.contains(&StatusNode::Text { text: "Office \u{b7} Printing page 2 of 3".into(), style: StatusText::Secondary }));
        assert!(p.nodes.contains(&StatusNode::Button { id: 7, label: "Cancel".into(), primary: false, enabled: true }));
        assert!(ICON.len() <= forum_ctl::STATUS_MAX_SVG);
    }
}
