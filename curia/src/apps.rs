//! The Apps page's model (curia.md §13): every installed app, what it asks
//! for, and who allowed it. Allow writes the person's grant, as the prompt
//! at an app's first launch would, and only with the press on Allow, which
//! portcullisd spends with frescod; Remove takes the person's grant away.
//! What the system policy grants, and any app asking for a restricted
//! capability, is not the person's to change here. Pure: `main.rs` asks
//! portcullisd and hands back what it says.
//!
//! Its Links section (opening-links.md §4.4): each link scheme some app
//! takes, the app that opens it, and a press to change that or to ask
//! each time, as Monstrator's "Always use this app" would.

use portcullis_ipc::{AccessBy, AccessEntry, LinkScheme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Refresh,
    Allow { app_id: String, activation: [u8; 16] },
    Remove { app_id: String },
    /// Make `app` the default for `scheme` links; `None`: ask each time.
    SetLink { scheme: String, app: Option<String>, activation: [u8; 16] },
}

#[derive(Clone, Debug)]
pub enum Got {
    List(Result<Vec<AccessEntry>, String>),
    /// An Allow or Remove came back.
    Done { app_id: String, result: Result<(), String> },
    Links(Result<Vec<LinkScheme>, String>),
    /// A SetLink came back.
    LinkDone { scheme: String, result: Result<(), String> },
}

#[derive(Clone, Debug, Default)]
pub struct AppsPage {
    /// None until portcullisd answers; `Err` says why it can't be asked.
    pub list: Option<Result<Vec<AccessEntry>, String>>,
    /// The app whose details are open.
    pub open: Option<String>,
    /// An Allow or Remove on its way, for this app.
    pub busy: Option<String>,
    /// What went wrong for an app, shown under it.
    pub note: Option<(String, String)>,
    /// Remove asks once more before it goes.
    pub confirm: Option<String>,
    /// The Links section: None until portcullisd answers.
    pub links: Option<Result<Vec<LinkScheme>, String>>,
    /// The scheme whose apps are open.
    pub link_open: Option<String>,
    /// A SetLink on its way, for this scheme.
    pub link_busy: Option<String>,
    /// What went wrong for a scheme.
    pub link_note: Option<(String, String)>,
    pub wants: Vec<Want>,
}

/// "Opens in Mail", "Asks each time".
pub fn link_status(l: &LinkScheme) -> String {
    match l.apps.iter().find(|c| c.default) {
        Some(c) if l.yours => format!("Opens in {}", c.name),
        Some(c) => format!("Opens in {} (the system's choice)", c.name),
        None => "Asks which app each time".into(),
    }
}

/// One line on what an app may do now.
pub fn status(e: &AccessEntry) -> String {
    match e.by {
        Some(AccessBy::System) => "Part of the system: allowed by the system".into(),
        Some(AccessBy::You) => match &e.yours {
            Some(at) => format!("Allowed by you{}", day(at).map(|d| format!(" on {d}")).unwrap_or_default()),
            None => "Allowed by you".into(),
        },
        None if !e.restricted.is_empty() => "Needs the system's approval; it won't open".into(),
        None if e.missing.first().is_some_and(|l| l.starts_with("Manifest has changed")) => "Updated: asks again when it opens".into(),
        None => "Asks when it opens".into(),
    }
}

/// "2026-11-15" from an RFC 3339 time.
fn day(at: &str) -> Option<&str> {
    let d = at.get(..10)?;
    (d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-').then_some(d)
}

/// What Allow would give, or why it can't be offered.
pub fn can_allow(e: &AccessEntry) -> bool {
    e.restricted.is_empty() && !e.missing.is_empty()
}

/// The person's grant can be taken away.
pub fn can_remove(e: &AccessEntry) -> bool {
    e.yours.is_some()
}

impl AppsPage {
    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    pub fn apps(&self) -> &[AccessEntry] {
        match &self.list {
            Some(Ok(v)) => v,
            _ => &[],
        }
    }

    fn find(&self, id: &str) -> Option<&AccessEntry> {
        self.apps().iter().find(|e| e.id == id)
    }

    pub fn refresh(&mut self) {
        if !self.wants.contains(&Want::Refresh) {
            self.wants.push(Want::Refresh);
        }
    }

    pub fn apply(&mut self, g: Got) {
        match g {
            Got::List(r) => {
                self.list = Some(r);
                let ids: Vec<String> = self.apps().iter().map(|e| e.id.clone()).collect();
                for slot in [&mut self.open, &mut self.confirm] {
                    if slot.as_ref().is_some_and(|o| !ids.contains(o)) {
                        *slot = None;
                    }
                }
            }
            Got::Done { app_id, result } => {
                if self.busy.as_deref() == Some(&app_id) {
                    self.busy = None;
                }
                match result {
                    Ok(()) => self.note = None,
                    Err(why) => self.note = Some((app_id, why)),
                }
                self.refresh();
            }
            Got::Links(r) => {
                self.links = Some(r);
                if self.link_open.as_ref().is_some_and(|o| !self.schemes().iter().any(|l| &l.scheme == o)) {
                    self.link_open = None;
                }
            }
            Got::LinkDone { scheme, result } => {
                if self.link_busy.as_deref() == Some(&scheme) {
                    self.link_busy = None;
                }
                match result {
                    Ok(()) => self.link_note = None,
                    Err(why) => self.link_note = Some((scheme, why)),
                }
                self.refresh();
            }
        }
    }

    pub fn schemes(&self) -> &[LinkScheme] {
        match &self.links {
            Some(Ok(v)) => v,
            _ => &[],
        }
    }

    pub fn toggle_link(&mut self, scheme: &str) {
        self.link_open = if self.link_open.as_deref() == Some(scheme) { None } else { Some(scheme.into()) };
        if self.link_note.as_ref().is_some_and(|(s, _)| s != scheme) {
            self.link_note = None;
        }
    }

    /// Choose `app` for `scheme` (`None`: ask each time), with the press
    /// that made it; nothing is asked without one, or when it changes
    /// nothing.
    pub fn set_link(&mut self, scheme: &str, app: Option<&str>, activation: Option<[u8; 16]>) {
        let Some(l) = self.schemes().iter().find(|l| l.scheme == scheme) else { return };
        let now = l.apps.iter().find(|c| c.default).map(|c| c.id.as_str());
        let known = app.is_none_or(|a| l.apps.iter().any(|c| c.id == a));
        let same = if app.is_none() { !l.yours } else { l.yours && now == app };
        if self.link_busy.is_some() || !known || same {
            return;
        }
        let Some(activation) = activation else {
            self.link_note = Some((scheme.into(), "Press with the pointer or Enter".into()));
            return;
        };
        self.link_busy = Some(scheme.into());
        self.link_note = None;
        self.wants.push(Want::SetLink { scheme: scheme.into(), app: app.map(str::to_string), activation });
    }

    pub fn toggle_open(&mut self, id: &str) {
        self.open = if self.open.as_deref() == Some(id) { None } else { Some(id.into()) };
        self.confirm = None;
        if self.note.as_ref().is_some_and(|(a, _)| a != id) {
            self.note = None;
        }
    }

    /// Allow, with the press that made it (`None`: not from a press, so
    /// nothing is asked: portcullisd would refuse it).
    pub fn allow(&mut self, id: &str, activation: Option<[u8; 16]>) {
        if self.busy.is_some() || !self.find(id).is_some_and(can_allow) {
            return;
        }
        let Some(activation) = activation else {
            self.note = Some((id.into(), "Press Allow with the pointer or Enter".into()));
            return;
        };
        self.busy = Some(id.into());
        self.note = None;
        self.wants.push(Want::Allow { app_id: id.into(), activation });
    }

    /// Remove: the first press asks, the second takes the grant away.
    pub fn remove(&mut self, id: &str) {
        if self.busy.is_some() || !self.find(id).is_some_and(can_remove) {
            return;
        }
        if self.confirm.as_deref() != Some(id) {
            self.confirm = Some(id.into());
            return;
        }
        self.confirm = None;
        self.busy = Some(id.into());
        self.note = None;
        self.wants.push(Want::Remove { app_id: id.into() });
    }

    pub fn cancel_remove(&mut self) {
        self.confirm = None;
    }

    /// "12 apps, 2 waiting for you".
    pub fn summary(&self) -> String {
        let v = self.apps();
        let waiting = v.iter().filter(|e| e.by.is_none() && e.restricted.is_empty()).count();
        let n = if v.len() == 1 { "1 app".to_string() } else { format!("{} apps", v.len()) };
        if waiting == 0 { n } else { format!("{n}, {waiting} not allowed yet") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: &str, by: Option<AccessBy>, missing: &[&str], yours: bool, restricted: &[&str]) -> AccessEntry {
        AccessEntry {
            id: id.into(),
            name: id.into(),
            by,
            asks: vec!["Play audio".into()],
            missing: missing.iter().map(|s| s.to_string()).collect(),
            yours: yours.then(|| "2026-11-15T10:00:00Z".into()),
            restricted: restricted.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn page() -> AppsPage {
        let mut p = AppsPage::default();
        p.apply(Got::List(Ok(vec![
            e("new", None, &["Play audio"], false, &[]),
            e("mine", Some(AccessBy::You), &[], true, &[]),
            e("sys", Some(AccessBy::System), &[], false, &["audio-control"]),
            e("odd", None, &["Change the volume"], false, &["audio-control"]),
        ])));
        p
    }

    #[test]
    fn statuses_say_who_allowed_it() {
        let p = page();
        let s: Vec<String> = p.apps().iter().map(status).collect();
        assert_eq!(s, ["Asks when it opens", "Allowed by you on 2026-11-15", "Part of the system: allowed by the system", "Needs the system's approval; it won't open"]);
        let upd = e("u", None, &["Manifest has changed since last grant", "Play audio"], true, &[]);
        assert_eq!(status(&upd), "Updated: asks again when it opens");
        assert_eq!(p.summary(), "4 apps, 1 not allowed yet");
    }

    #[test]
    fn allow_needs_a_press_and_only_what_the_person_may_allow() {
        let mut p = page();
        p.allow("new", None);
        assert!(p.take_wants().is_empty());
        assert_eq!(p.note.as_ref().unwrap().0, "new");
        p.allow("sys", Some([1; 16]));
        p.allow("odd", Some([1; 16]));
        p.allow("mine", Some([1; 16]));
        assert!(p.take_wants().is_empty(), "restricted, or nothing to allow");
        p.allow("new", Some([7; 16]));
        p.allow("new", Some([8; 16]));
        assert_eq!(p.take_wants(), vec![Want::Allow { app_id: "new".into(), activation: [7; 16] }], "one at a time");
        p.apply(Got::Done { app_id: "new".into(), result: Err("the window lost focus; open it again".into()) });
        assert_eq!(p.busy, None);
        assert_eq!(p.note, Some(("new".into(), "the window lost focus; open it again".into())));
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
    }

    #[test]
    fn remove_asks_once_and_never_the_systems() {
        let mut p = page();
        p.remove("sys");
        p.remove("new");
        assert!(p.take_wants().is_empty() && p.confirm.is_none());
        p.remove("mine");
        assert_eq!(p.confirm.as_deref(), Some("mine"));
        assert!(p.take_wants().is_empty());
        p.remove("mine");
        assert_eq!(p.take_wants(), vec![Want::Remove { app_id: "mine".into() }]);
        p.apply(Got::Done { app_id: "mine".into(), result: Ok(()) });
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        // An app gone from the list closes.
        p.toggle_open("mine");
        p.apply(Got::List(Ok(vec![e("new", None, &["Play audio"], false, &[])])));
        assert_eq!(p.open, None);
    }

    #[test]
    fn links_change_with_a_press_and_only_to_an_app_that_takes_them() {
        use portcullis_ipc::LinkChoice;
        let c = |id: &str, default: bool| LinkChoice { id: id.into(), name: id.to_uppercase(), default };
        let mut p = AppsPage::default();
        p.apply(Got::Links(Ok(vec![
            LinkScheme { scheme: "https".into(), apps: vec![c("nav", true)], yours: false },
            LinkScheme { scheme: "mailto".into(), apps: vec![c("a", false), c("b", false)], yours: false },
        ])));
        let s: Vec<String> = p.schemes().iter().map(link_status).collect();
        assert_eq!(s, ["Opens in NAV (the system's choice)", "Asks which app each time"]);
        p.set_link("mailto", Some("a"), None);
        assert!(p.take_wants().is_empty() && p.link_note.is_some(), "no press");
        p.set_link("mailto", Some("zz"), Some([1; 16]));
        p.set_link("mailto", None, Some([1; 16]));
        p.set_link("ftp", Some("a"), Some([1; 16]));
        assert!(p.take_wants().is_empty(), "not its app, nothing to change, no such scheme");
        p.set_link("mailto", Some("b"), Some([2; 16]));
        p.set_link("mailto", Some("a"), Some([3; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetLink { scheme: "mailto".into(), app: Some("b".into()), activation: [2; 16] }], "one at a time");
        p.apply(Got::LinkDone { scheme: "mailto".into(), result: Ok(()) });
        assert_eq!(p.link_busy, None);
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        p.apply(Got::Links(Ok(vec![LinkScheme { scheme: "mailto".into(), apps: vec![c("a", false), c("b", true)], yours: true }])));
        assert_eq!(link_status(&p.schemes()[0]), "Opens in B");
        p.set_link("mailto", Some("b"), Some([4; 16]));
        assert!(p.take_wants().is_empty(), "already");
        p.set_link("mailto", None, Some([5; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetLink { scheme: "mailto".into(), app: None, activation: [5; 16] }]);
    }
}
