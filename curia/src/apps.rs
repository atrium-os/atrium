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
//!
//! Its Share section (share-sheet.md §4.4): the apps the share sheet
//! offers, in its order; Hide takes one off the sheet, Move up puts it
//! higher. Each change goes with a press, which portcullisd spends.
//!
//! An app with a settings pane of its own (curia.md §15) gets Settings on
//! its row: the app's pane, which limend starts for Curia in the persona
//! the person picks, shows in the page in place of the list. Curia sees
//! only its pixels.
//!
//! An app that asks to run in the background (background-apps.md §3.2)
//! gets a switch per persona in its details: "Open Clipboard History when
//! you sign in". Off by default; turning one on goes with a press, which
//! portcullisd spends; turning one off needs none.
//!
//! Its Between personas section (personas.md §6): a switch per persona,
//! direction and kind ("Work may send links to Personal"). Off by default;
//! turning one on goes with a press and then the person confirming it's
//! them on Sigillum's sheet, which portcullisd asks for.

use portcullis_ipc::{AccessBy, AccessEntry, CrossingSwitch, LinkScheme, ShareTargetRow, CROSSING_KINDS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Refresh,
    Allow { app_id: String, activation: [u8; 16] },
    Remove { app_id: String },
    /// Turn `app_id`'s background switch in `persona` on or off. On carries
    /// the press; off needs none.
    SetBackground { app_id: String, persona: String, on: bool, activation: Option<[u8; 16]> },
    SetListen { app_id: String, persona: String, network: bool, on: bool, activation: Option<[u8; 16]> },
    /// Make `app` the default for `scheme` links; `None`: ask each time.
    SetLink { scheme: String, app: Option<String>, activation: [u8; 16] },
    /// The share sheet's order and hidden apps, all of them.
    SetShare { order: Vec<String>, hidden: Vec<String>, activation: [u8; 16] },
    /// Turn one crossing switch on or off.
    SetCrossing { switch: CrossingSwitch, on: bool, activation: [u8; 16] },
    /// The Personas page (personas.md §2): add one, recolour one, remove one.
    AddPersona { name: String, colour: String, activation: [u8; 16] },
    SetColour { name: String, colour: String, activation: [u8; 16] },
    RemovePersona { name: String, activation: [u8; 16] },
}

/// The person's named personas (name, colour) and the switches that are on.
pub type Crossings = (Vec<(String, String)>, Vec<CrossingSwitch>);

#[derive(Clone, Debug)]
pub enum Got {
    List(Result<Vec<AccessEntry>, String>),
    /// An Allow or Remove came back.
    Done { app_id: String, result: Result<(), String> },
    Links(Result<Vec<LinkScheme>, String>),
    /// A SetLink came back.
    LinkDone { scheme: String, result: Result<(), String> },
    Share(Result<Vec<ShareTargetRow>, String>),
    /// A SetShare came back.
    ShareDone(Result<(), String>),
    Crossings(Result<Crossings, String>),
    /// A SetCrossing came back.
    CrossingDone { switch: CrossingSwitch, result: Result<(), String> },
    /// An AddPersona, SetColour or RemovePersona came back, for `name`.
    PersonaDone { name: String, result: Result<(), String> },
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
    /// The Share section: None until portcullisd answers.
    pub share: Option<Result<Vec<ShareTargetRow>, String>>,
    /// A SetShare on its way.
    pub share_busy: bool,
    /// What went wrong, for the section.
    pub share_note: Option<String>,
    /// The Between personas section: None until portcullisd answers.
    pub crossings: Option<Result<Crossings, String>>,
    /// A SetCrossing on its way (turning on waits for the person's
    /// verification).
    pub crossing_busy: Option<(CrossingSwitch, bool)>,
    /// What went wrong for a switch.
    pub crossing_note: Option<(CrossingSwitch, String)>,
    /// The Personas page: a change on its way, for this persona.
    pub persona_busy: Option<String>,
    /// What went wrong for a persona (or `""`: for Add).
    pub persona_note: Option<(String, String)>,
    /// Remove asks once more, saying what goes.
    pub persona_confirm: Option<String>,
    /// The colour chosen for a new persona.
    pub new_colour: Option<String>,
    /// An app's own settings pane, open in the page.
    pub pane: Option<Pane>,
    /// The app whose Settings asks which persona first.
    pub pane_pick: Option<String>,
    /// Why an app's pane closed or didn't open, under its row.
    pub pane_note: Option<(String, String)>,
    pub wants: Vec<Want>,
}

/// An app's settings pane: whose, in which persona, and whether its
/// window is in the slot yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub app: String,
    pub name: String,
    /// "personal" or a named persona.
    pub persona: String,
    pub up: bool,
}

/// The pane's slot, and its size: the pane draws a window this big.
pub const PANE_SLOT: pergola::slot::SlotKey = 0x5345_5431;
pub const PANE_W: f32 = 560.0;
pub const PANE_H: f32 = 420.0;

/// The colours a persona may take (portcullis-policy's `PERSONA_COLOURS`).
pub const COLOURS: &[&str] = &["blue", "green", "orange", "purple", "red", "teal", "pink", "yellow"];

/// A persona name from what the person typed: "Work" is `work`, "Side
/// project" is `side-project`; `None` when it can't be one.
pub fn persona_key(typed: &str) -> Option<String> {
    let k: String = typed.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-");
    let ok = !k.is_empty() && k.len() <= 32 && k != "personal"
        && k.starts_with(|c: char| c.is_ascii_lowercase())
        && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then_some(k)
}

/// "Opens in Mail", "Asks each time".
pub fn link_status(l: &LinkScheme) -> String {
    match l.apps.iter().find(|c| c.default) {
        Some(c) if l.yours => format!("Opens in {}", c.name),
        Some(c) => format!("Opens in {} (the system's choice)", c.name),
        None => "Asks which app each time".into(),
    }
}

/// "Personal", "Work": a persona's name as the chip shows it.
pub fn persona_name(p: &str) -> String {
    let mut c = p.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// "Work may send links to Personal"; for `share`, "Work may share to
/// Personal's apps" (the share sheet lists them).
pub fn crossing_words(s: &CrossingSwitch) -> String {
    match s.kind.as_str() {
        "printers" => format!("{} may print to {} printers", persona_name(&s.from), persona_name(&s.to)),
        "share" => format!("{} may share to {}'s apps", persona_name(&s.from), persona_name(&s.to)),
        k => format!("{} may send {k} to {}", persona_name(&s.from), persona_name(&s.to)),
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

/// The background switch's words for `e` in `persona`; `in_persona`
/// when the person has named personas, so each switch says which.
pub fn background_words(e: &AccessEntry, persona: &str, in_persona: bool) -> Option<String> {
    let b = e.background.as_ref()?;
    let w = if b.at_sign_in {
        format!("Open {} when you sign in", e.name)
    } else {
        format!("Keep {} running in the background once opened", e.name)
    };
    Some(if in_persona { format!("{w}, in {}", persona_name(persona)) } else { w })
}

/// `e` may run in the background in `persona` now.
pub fn background_on(e: &AccessEntry, persona: &str) -> bool {
    e.background.as_ref().is_some_and(|b| b.allowed_in.iter().any(|p| p == persona))
}

/// One of an app's port switches (background-apps.md §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenRow {
    /// The network switch (devices on the network the computer is on now);
    /// otherwise the app's own switch.
    pub network: bool,
    pub words: String,
    pub on: bool,
    /// Why it can't be turned on here, when it can't.
    pub blocked: Option<String>,
}

fn ports(v: &[u16]) -> String {
    let p: Vec<String> = v.iter().map(u16::to_string).collect();
    match p.len() {
        1 => format!("port {}", p[0]),
        _ => format!("ports {}", p.join(", ")),
    }
}

/// The port switches for `e`: its own (this computer, and the leave the
/// network switch needs), then the network one when a port is for the
/// local network.
pub fn listen_rows(e: &AccessEntry) -> Vec<ListenRow> {
    let Some(l) = &e.listen else { return Vec::new() };
    let mine: Vec<u16> = l.ports.iter().filter(|p| !p.local_network).map(|p| p.port).collect();
    let lan: Vec<u16> = l.ports.iter().filter(|p| p.local_network).map(|p| p.port).collect();
    let allowed = l.persona.is_some();
    let mut v = vec![ListenRow {
        network: false,
        words: if mine.is_empty() {
            format!("Let {} take connections", e.name)
        } else {
            format!("Let your own programs on this computer connect to {} on {}", e.name, ports(&mine))
        },
        on: allowed,
        blocked: None,
    }];
    if !lan.is_empty() {
        let here_on = l.here.as_ref().is_some_and(|h| l.networks.contains(h));
        v.push(ListenRow {
            network: true,
            words: format!("Let devices on this network connect to {} on {}", e.name, ports(&lan)),
            on: here_on,
            blocked: if l.here.is_none() { Some(l.here_why.clone().unwrap_or_else(|| "This network can't be allowed".into())) } else { None },
        });
    }
    v
}

/// The persona whose instance gets `e`'s ports: the one already allowed,
/// else Personal.
pub fn listen_persona(e: &AccessEntry) -> String {
    e.listen.as_ref().and_then(|l| l.persona.clone()).unwrap_or_else(|| "personal".into())
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
            Got::Share(r) => self.share = Some(r),
            Got::ShareDone(result) => {
                self.share_busy = false;
                self.share_note = result.err();
                self.refresh();
            }
            Got::Crossings(r) => self.crossings = Some(r),
            Got::PersonaDone { name, result } => {
                if self.persona_busy.as_deref() == Some(&name) {
                    self.persona_busy = None;
                }
                match result {
                    Ok(()) => {
                        self.persona_note = None;
                        if name.is_empty() {
                            self.new_colour = None;
                        }
                    }
                    Err(why) => self.persona_note = Some((name, why)),
                }
                self.refresh();
            }
            Got::CrossingDone { switch, result } => {
                if self.crossing_busy.as_ref().is_some_and(|(s, _)| *s == switch) {
                    self.crossing_busy = None;
                }
                match result {
                    Ok(()) => self.crossing_note = None,
                    Err(why) => self.crossing_note = Some((switch, why)),
                }
                self.refresh();
            }
        }
    }

    /// Each direction between two personas, Personal first: none until
    /// the person has a named persona.
    pub fn directions(&self) -> Vec<(String, String)> {
        let Some(Ok((named, _))) = &self.crossings else { return Vec::new() };
        let all: Vec<&str> = std::iter::once("personal").chain(named.iter().map(|(n, _)| n.as_str())).collect();
        let mut v = Vec::new();
        for a in &all {
            for b in &all {
                if a != b {
                    v.push((a.to_string(), b.to_string()));
                }
            }
        }
        v
    }

    /// The person's named personas, (name, colour).
    pub fn named(&self) -> &[(String, String)] {
        match &self.crossings {
            Some(Ok((n, _))) => n,
            _ => &[],
        }
    }

    /// Colours no persona has yet.
    pub fn free_colours(&self) -> Vec<&'static str> {
        COLOURS.iter().copied().filter(|c| !self.named().iter().any(|(_, k)| k == c)).collect()
    }

    /// The colour a new persona gets: the one chosen, else the first free.
    pub fn new_colour(&self) -> Option<String> {
        let free = self.free_colours();
        self.new_colour.clone().filter(|c| free.contains(&c.as_str())).or_else(|| free.first().map(|c| c.to_string()))
    }

    pub fn choose_new_colour(&mut self, c: &str) {
        if self.free_colours().contains(&c) {
            self.new_colour = Some(c.into());
        }
    }

    /// Add: the name as typed, with the press that made it. The note for
    /// Add is under the empty name.
    pub fn add_persona(&mut self, typed: &str, activation: Option<[u8; 16]>) {
        if self.persona_busy.is_some() || self.crossings.as_ref().is_none_or(|r| r.is_err()) {
            return;
        }
        let Some(name) = persona_key(typed) else {
            let why = if typed.trim().eq_ignore_ascii_case("personal") { "Personal is always there" } else { "Use letters, digits and spaces, starting with a letter (up to 32)" };
            self.persona_note = Some((String::new(), why.into()));
            return;
        };
        if self.named().iter().any(|(n, _)| *n == name) {
            self.persona_note = Some((String::new(), format!("You have {} already", persona_name(&name))));
            return;
        }
        let Some(colour) = self.new_colour() else {
            self.persona_note = Some((String::new(), "Every colour is taken: eight personas at most".into()));
            return;
        };
        let Some(activation) = activation else {
            self.persona_note = Some((String::new(), "Press Add with the pointer or Enter".into()));
            return;
        };
        self.persona_note = None;
        self.persona_busy = Some(String::new());
        self.wants.push(Want::AddPersona { name, colour, activation });
    }

    pub fn set_colour(&mut self, name: &str, colour: &str, activation: Option<[u8; 16]>) {
        let has = self.named().iter().any(|(n, k)| n == name && k != colour);
        if self.persona_busy.is_some() || !has || !self.free_colours().contains(&colour) {
            return;
        }
        let Some(activation) = activation else {
            self.persona_note = Some((name.into(), "Press with the pointer or Enter".into()));
            return;
        };
        self.persona_note = None;
        self.persona_busy = Some(name.into());
        self.wants.push(Want::SetColour { name: name.into(), colour: colour.into(), activation });
    }

    /// Remove: the first press asks, saying what goes; the second, with
    /// its press, asks portcullisd (and Sigillum's sheet).
    pub fn remove_persona(&mut self, name: &str, activation: Option<[u8; 16]>) {
        if self.persona_busy.is_some() || !self.named().iter().any(|(n, _)| n == name) {
            return;
        }
        if self.persona_confirm.as_deref() != Some(name) {
            self.persona_confirm = Some(name.into());
            self.persona_note = None;
            return;
        }
        let Some(activation) = activation else {
            self.persona_note = Some((name.into(), "Press with the pointer or Enter".into()));
            return;
        };
        self.persona_confirm = None;
        self.persona_note = None;
        self.persona_busy = Some(name.into());
        self.wants.push(Want::RemovePersona { name: name.into(), activation });
    }

    pub fn cancel_remove_persona(&mut self) {
        self.persona_confirm = None;
    }

    pub fn crossing_on(&self, s: &CrossingSwitch) -> bool {
        matches!(&self.crossings, Some(Ok((_, on))) if on.contains(s))
    }

    /// Turn a switch on or off, with the press that made it; one at a
    /// time, and only a switch the section shows.
    pub fn set_crossing(&mut self, switch: CrossingSwitch, on: bool, activation: Option<[u8; 16]>) {
        let shown = CROSSING_KINDS.contains(&switch.kind.as_str()) && self.directions().iter().any(|(f, t)| *f == switch.from && *t == switch.to);
        if self.crossing_busy.is_some() || !shown || self.crossing_on(&switch) == on {
            return;
        }
        let Some(activation) = activation else {
            self.crossing_note = Some((switch, "Press with the pointer or Enter".into()));
            return;
        };
        self.crossing_note = None;
        self.crossing_busy = Some((switch.clone(), on));
        self.wants.push(Want::SetCrossing { switch, on, activation });
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

    /// The share sheet's apps, in its order.
    pub fn share_targets(&self) -> &[ShareTargetRow] {
        match &self.share {
            Some(Ok(v)) => v,
            _ => &[],
        }
    }

    /// Hide `app` from the share sheet, or show it again.
    pub fn share_hide(&mut self, app: &str, hide: bool, activation: Option<[u8; 16]>) {
        let mut rows = self.share_targets().to_vec();
        let Some(r) = rows.iter_mut().find(|r| r.id == app) else { return };
        if r.hidden == hide {
            return;
        }
        r.hidden = hide;
        self.set_share(rows, activation);
    }

    /// Put `app` one place higher on the share sheet.
    pub fn share_up(&mut self, app: &str, activation: Option<[u8; 16]>) {
        let mut rows = self.share_targets().to_vec();
        let Some(i) = rows.iter().position(|r| r.id == app) else { return };
        if i == 0 {
            return;
        }
        rows.swap(i - 1, i);
        self.set_share(rows, activation);
    }

    fn set_share(&mut self, rows: Vec<ShareTargetRow>, activation: Option<[u8; 16]>) {
        if self.share_busy {
            return;
        }
        let Some(activation) = activation else {
            self.share_note = Some("Press with the pointer or Enter".into());
            return;
        };
        self.share_busy = true;
        self.share_note = None;
        let order = rows.iter().map(|r| r.id.clone()).collect();
        let hidden = rows.iter().filter(|r| r.hidden).map(|r| r.id.clone()).collect();
        self.wants.push(Want::SetShare { order, hidden, activation });
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

    /// Turn `id`'s background switch in `persona` to `on`. On needs the
    /// press (`None`: not from one, so nothing is asked), and an app that
    /// runs as it is.
    pub fn set_background(&mut self, id: &str, persona: &str, on: bool, activation: Option<[u8; 16]>) {
        let Some(e) = self.find(id) else { return };
        if self.busy.is_some() || e.background.is_none() || background_on(e, persona) == on {
            return;
        }
        if on && e.by.is_none() {
            self.note = Some((id.into(), format!("Allow {} first", e.name)));
            return;
        }
        if on && activation.is_none() {
            self.note = Some((id.into(), "Press Turn on with the pointer or Enter".into()));
            return;
        }
        self.busy = Some(id.into());
        self.note = None;
        self.wants.push(Want::SetBackground { app_id: id.into(), persona: persona.into(), on, activation: activation.filter(|_| on) });
    }

    /// Turn one of `id`'s port switches to `on` (see [`listen_rows`]).
    pub fn set_listen(&mut self, id: &str, network: bool, on: bool, activation: Option<[u8; 16]>) {
        let Some(e) = self.find(id) else { return };
        let Some(row) = listen_rows(e).into_iter().find(|r| r.network == network) else { return };
        if self.busy.is_some() || row.on == on {
            return;
        }
        if on && e.by.is_none() {
            self.note = Some((id.into(), format!("Allow {} first", e.name)));
            return;
        }
        if on {
            if let Some(why) = row.blocked {
                self.note = Some((id.into(), why));
                return;
            }
            if activation.is_none() {
                self.note = Some((id.into(), "Press Turn on with the pointer or Enter".into()));
                return;
            }
        }
        let persona = listen_persona(e);
        self.busy = Some(id.into());
        self.note = None;
        self.wants.push(Want::SetListen { app_id: id.into(), persona, network, on, activation: activation.filter(|_| on) });
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

    /// Settings on an app's row: its pane, after the persona when the
    /// person has named ones.
    pub fn open_settings(&mut self, id: &str) {
        if !self.find(id).is_some_and(|e| e.settings) {
            return;
        }
        self.pane_note = None;
        if self.named().is_empty() {
            self.settings_in(id, "personal");
        } else {
            self.pane_pick = if self.pane_pick.as_deref() == Some(id) { None } else { Some(id.into()) };
        }
    }

    /// The pane of `id` in `persona`: Personal or one of the person's.
    pub fn settings_in(&mut self, id: &str, persona: &str) {
        let Some(e) = self.find(id).filter(|e| e.settings) else { return };
        if persona != "personal" && !self.named().iter().any(|(n, _)| n == persona) {
            return;
        }
        let name = e.name.clone();
        self.pane_pick = None;
        self.pane = Some(Pane { app: id.into(), name, persona: persona.into(), up: false });
    }

    /// Back to the list.
    pub fn close_settings(&mut self) {
        self.pane = None;
    }

    /// The pane's window is in its slot.
    pub fn pane_up(&mut self) {
        if let Some(p) = self.pane.as_mut() { p.up = true }
    }

    /// The pane went, or never came: back to the list, saying so.
    pub fn pane_gone(&mut self, why: &str) {
        if let Some(p) = self.pane.take() {
            self.pane_note = Some((p.app, why.into()));
        }
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
            settings: false,
            background: None,
            listen: None,
        }
    }

    #[test]
    fn background_switches_go_with_a_press_on_and_none_off() {
        use portcullis_ipc::BackgroundAccess;
        let bg = |by, allowed: &[&str]| AccessEntry {
            name: "Clipboard History".into(),
            background: Some(BackgroundAccess { at_sign_in: true, allowed_in: allowed.iter().map(|s| s.to_string()).collect(), stale: false }),
            ..e("org.x.bar", by, &[], true, &[])
        };
        let mut p = AppsPage::default();
        p.apply(Got::List(Ok(vec![bg(Some(AccessBy::You), &[]), e("plain", Some(AccessBy::You), &[], true, &[])])));
        let row = p.find("org.x.bar").unwrap().clone();
        assert_eq!(background_words(&row, "personal", false).as_deref(), Some("Open Clipboard History when you sign in"));
        assert_eq!(background_words(&row, "work", true).as_deref(), Some("Open Clipboard History when you sign in, in Work"));
        assert_eq!(background_words(p.find("plain").unwrap(), "personal", false), None, "it doesn't ask");
        p.set_background("org.x.bar", "personal", true, None);
        assert!(p.take_wants().is_empty(), "on needs the press");
        assert!(p.note.as_ref().unwrap().1.contains("Press"));
        p.set_background("plain", "personal", true, Some([1; 16]));
        assert!(p.take_wants().is_empty(), "no switch to turn");
        p.set_background("org.x.bar", "personal", true, Some([1; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetBackground { app_id: "org.x.bar".into(), persona: "personal".into(), on: true, activation: Some([1; 16]) }]);
        p.apply(Got::Done { app_id: "org.x.bar".into(), result: Ok(()) });
        p.take_wants();
        p.apply(Got::List(Ok(vec![bg(Some(AccessBy::You), &["personal"])])));
        assert!(background_on(p.find("org.x.bar").unwrap(), "personal"));
        p.set_background("org.x.bar", "personal", false, Some([2; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetBackground { app_id: "org.x.bar".into(), persona: "personal".into(), on: false, activation: None }], "off spends no press");
        // An app not allowed to run can't be turned on.
        let mut q = AppsPage::default();
        q.apply(Got::List(Ok(vec![bg(None, &[])])));
        q.set_background("org.x.bar", "personal", true, Some([1; 16]));
        assert!(q.take_wants().is_empty());
        assert_eq!(q.note.as_ref().unwrap().1, "Allow Clipboard History first");
    }

    #[test]
    fn settings_opens_an_apps_own_pane_in_the_persona_picked() {
        let mut p = AppsPage::default();
        let web = AccessEntry { settings: true, name: "Navigator".into(), ..e("org.atrium.navigator", None, &[], false, &[]) };
        p.apply(Got::List(Ok(vec![web, e("org.example.notes", None, &[], false, &[])])));
        p.apply(Got::Crossings(Ok((vec![], vec![]))));
        p.open_settings("org.example.notes");
        assert_eq!(p.pane, None, "no pane of its own");
        p.open_settings("org.atrium.navigator");
        assert_eq!(p.pane, Some(Pane { app: "org.atrium.navigator".into(), name: "Navigator".into(), persona: "personal".into(), up: false }), "Personal only: no question");
        p.pane_up();
        p.pane_gone("Its settings closed");
        assert_eq!((p.pane.clone(), p.pane_note.clone()), (None, Some(("org.atrium.navigator".into(), "Its settings closed".into()))));

        p.apply(Got::Crossings(Ok((vec![("work".into(), "blue".into())], vec![]))));
        p.open_settings("org.atrium.navigator");
        assert_eq!((p.pane.clone(), p.pane_pick.as_deref()), (None, Some("org.atrium.navigator")), "which persona first");
        p.settings_in("org.atrium.navigator", "school");
        assert_eq!(p.pane, None, "not one of theirs");
        p.settings_in("org.atrium.navigator", "work");
        assert_eq!(p.pane.as_ref().map(|x| x.persona.as_str()), Some("work"));
        assert_eq!(p.pane_pick, None);
        p.close_settings();
        assert_eq!(p.pane, None);
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
    fn share_targets_hide_and_move_up_with_a_press() {
        let row = |id: &str, hidden: bool| ShareTargetRow { id: id.into(), name: id.to_uppercase(), hidden };
        let mut p = AppsPage::default();
        p.apply(Got::Share(Ok(vec![row("a", false), row("b", false), row("c", true)])));
        // No press, nothing asked.
        p.share_up("b", None);
        assert!(p.take_wants().is_empty());
        assert!(p.share_note.is_some());
        p.share_up("b", Some([1; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetShare { order: vec!["b".into(), "a".into(), "c".into()], hidden: vec!["c".into()], activation: [1; 16] }]);
        // One at a time.
        p.share_hide("a", true, Some([2; 16]));
        assert!(p.take_wants().is_empty());
        p.apply(Got::ShareDone(Ok(())));
        let _ = p.take_wants(); // the refresh
        p.apply(Got::Share(Ok(vec![row("b", false), row("a", false), row("c", true)])));
        p.share_hide("c", false, Some([3; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetShare { order: vec!["b".into(), "a".into(), "c".into()], hidden: vec![], activation: [3; 16] }]);
        p.apply(Got::ShareDone(Err("no".into())));
        // The top one doesn't move up; an unknown app changes nothing.
        let _ = p.take_wants();
        p.share_up("b", Some([4; 16]));
        p.share_hide("zzz", true, Some([4; 16]));
        assert!(p.take_wants().is_empty());
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

    #[test]
    fn crossing_switches_go_one_way_one_kind_with_a_press() {
        let sw = |f: &str, t: &str, k: &str| CrossingSwitch { from: f.into(), to: t.into(), kind: k.into() };
        let mut p = AppsPage::default();
        p.apply(Got::Crossings(Ok((vec![], vec![]))));
        assert!(p.directions().is_empty(), "only Personal: nothing crosses");
        p.apply(Got::Crossings(Ok((vec![("work".into(), "blue".into())], vec![sw("work", "personal", "links")]))));
        assert_eq!(p.directions(), [("personal".to_string(), "work".to_string()), ("work".into(), "personal".into())]);
        assert!(p.crossing_on(&sw("work", "personal", "links")));
        assert_eq!(crossing_words(&sw("work", "personal", "links")), "Work may send links to Personal");
        assert_eq!(crossing_words(&sw("personal", "work", "share")), "Personal may share to Work's apps");
        p.set_crossing(sw("personal", "work", "clipboard"), true, None);
        assert!(p.take_wants().is_empty() && p.crossing_note.is_some(), "no press");
        p.set_crossing(sw("work", "personal", "links"), true, Some([1; 16]));
        p.set_crossing(sw("personal", "school", "links"), true, Some([1; 16]));
        p.set_crossing(sw("personal", "work", "workspace-links"), true, Some([1; 16]));
        assert!(p.take_wants().is_empty(), "already on, no such persona, not a kind shown");
        p.set_crossing(sw("personal", "work", "clipboard"), true, Some([2; 16]));
        p.set_crossing(sw("work", "personal", "links"), false, Some([3; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetCrossing { switch: sw("personal", "work", "clipboard"), on: true, activation: [2; 16] }], "one at a time");
        p.apply(Got::CrossingDone { switch: sw("personal", "work", "clipboard"), result: Err("not confirmed (Cancelled); nothing changed".into()) });
        assert!(p.crossing_busy.is_none() && p.crossing_note.is_some());
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        p.set_crossing(sw("work", "personal", "links"), false, Some([3; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetCrossing { switch: sw("work", "personal", "links"), on: false, activation: [3; 16] }]);
    }

    fn with_work() -> AppsPage {
        let mut p = AppsPage::default();
        p.apply(Got::Crossings(Ok((vec![("work".into(), "blue".into())], vec![]))));
        p.wants.clear();
        p
    }

    #[test]
    fn names_as_typed_become_persona_keys() {
        assert_eq!(persona_key(" Side  project ").as_deref(), Some("side-project"));
        assert_eq!(persona_key("Work").as_deref(), Some("work"));
        assert_eq!(persona_key("personal"), None);
        assert_eq!(persona_key("2nd job"), None);
        assert_eq!(persona_key("Ünïcode"), None);
        assert_eq!(persona_key(""), None);
    }

    #[test]
    fn add_takes_a_press_a_new_name_and_a_free_colour() {
        let mut p = with_work();
        assert_eq!(p.free_colours().len(), 7);
        assert_eq!(p.new_colour().as_deref(), Some("green"));
        p.choose_new_colour("blue");
        assert_eq!(p.new_colour().as_deref(), Some("green"), "work's");
        p.choose_new_colour("teal");
        p.add_persona("School", None);
        assert!(p.wants.is_empty() && p.persona_note.is_some());
        p.add_persona("Work", Some([1; 16]));
        assert!(p.wants.is_empty() && p.persona_note.as_ref().unwrap().1.contains("already"));
        p.add_persona("School", Some([1; 16]));
        assert_eq!(p.wants, vec![Want::AddPersona { name: "school".into(), colour: "teal".into(), activation: [1; 16] }]);
        p.add_persona("Other", Some([1; 16]));
        assert_eq!(p.wants.len(), 1, "one at a time");
        p.apply(Got::PersonaDone { name: String::new(), result: Ok(()) });
        assert_eq!(p.persona_busy, None);
        assert!(p.wants.contains(&Want::Refresh));
    }

    #[test]
    fn remove_asks_twice_and_recolour_needs_a_free_colour() {
        let mut p = with_work();
        p.remove_persona("work", Some([2; 16]));
        assert!(p.wants.is_empty());
        assert_eq!(p.persona_confirm.as_deref(), Some("work"));
        p.remove_persona("work", Some([2; 16]));
        assert_eq!(p.wants, vec![Want::RemovePersona { name: "work".into(), activation: [2; 16] }]);
        p.apply(Got::PersonaDone { name: "work".into(), result: Err("Mail is still open in work: close it first".into()) });
        assert!(p.persona_note.as_ref().unwrap().1.contains("close it first"));
        p.wants.clear();
        p.set_colour("work", "blue", Some([3; 16]));
        assert!(p.wants.is_empty(), "its own colour already");
        p.set_colour("work", "red", Some([3; 16]));
        assert_eq!(p.wants, vec![Want::SetColour { name: "work".into(), colour: "red".into(), activation: [3; 16] }]);
        p.remove_persona("school", Some([2; 16]));
        assert_eq!(p.persona_confirm, None, "not one of hers");
    }

    #[test]
    fn port_switches_say_where_from_and_need_the_press() {
        use portcullis_ipc::{ListenAccess, ListenPort};
        let mut row = e("org.x.srv", Some(AccessBy::You), &[], true, &[]);
        row.name = "Photo Server".into();
        row.listen = Some(ListenAccess {
            ports: vec![ListenPort { port: 8080, udp: false, local_network: true }, ListenPort { port: 9000, udp: false, local_network: false }],
            persona: None, networks: vec![], stale: false, here: Some("wifi-486f6d65".into()), here_why: None,
        });
        let rows = listen_rows(&row);
        assert_eq!(rows[0].words, "Let your own programs on this computer connect to Photo Server on port 9000");
        assert_eq!(rows[1].words, "Let devices on this network connect to Photo Server on port 8080");
        assert!(!rows[0].on && !rows[1].on);
        let mut p = AppsPage::default();
        p.apply(Got::List(Ok(vec![row.clone()])));
        p.set_listen("org.x.srv", true, true, None);
        assert!(p.take_wants().is_empty(), "no press, nothing asked");
        p.set_listen("org.x.srv", true, true, Some([2; 16]));
        assert_eq!(p.take_wants(), vec![Want::SetListen { app_id: "org.x.srv".into(), persona: "personal".into(), network: true, on: true, activation: Some([2; 16]) }]);
        let mut open = row;
        if let Some(l) = open.listen.as_mut() { l.here = None; l.here_why = Some("open Wi-Fi networks can't be allowed".into()); }
        assert_eq!(listen_rows(&open)[1].blocked.as_deref(), Some("open Wi-Fi networks can't be allowed"));
    }
}
