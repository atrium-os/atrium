//! The share sheet (docs/spec/share-sheet.md): Limen's `share` provider.
//!
//! An app that wants to share embeds the `share` role in its window with
//! the press that asked for it, a title and the media types it has
//! ([`limen::share::Open`]). The sheet:
//!
//! 1. spends that press with frescod (purpose `SHARE`; only `_share` may),
//!    so no app opens a sheet without the person asking, and a press from
//!    another app's window is useless;
//! 2. lists the installed apps that implement `share-target` and take one
//!    of the item's types, for the app's person (her dev builds, nobody
//!    else's), never the sharing app itself;
//! 3. when the person presses one, tells limend (`CHOOSE`) and the app
//!    ([`limen::share::ToParent::Chosen`]). The app then embeds
//!    `share-target`, which limend starts from that choice and nothing
//!    else, and sends the item to it.
//!
//! A person with personas (personas.md) also sees, below those, the apps
//! of each other persona their `share` switch from the sharing app's
//! persona to it is on (Settings, Between personas), each marked with the
//! persona it opens in (§7). portcullisd checks that switch again when
//! limend asks it to start the target there.
//!
//! The app never sees the list, nor which app was chosen; the sheet never
//! sees the content, only its title and types.
//!
//! [`Sheet`] is the service without its windows, so it runs in tests; the
//! `share-sheet` binary draws each sheet with Pergola.

use std::collections::HashMap;

use std::time::Duration;

use fresco_protocol::activation::{purpose, Consume, ConsumeResult};
use limen::client::{Child, Error};
use limen::manifests::{Installed, Target};
use limen::proto::{ChildEvent, EmbedId, WindowToken};
use limen::share::{ToChild, ToParent};
use portcullis_policy::share::SharePrefs;
use portcullis_policy::{Policy, PERSONAL};
use tabula::gate::Gate;

/// uid → the person whose app it is (the launch registry's user).
pub type Person = Box<dyn Fn(u32) -> Option<String> + Send>;

/// uid → the persona its instance runs in (None: Personal).
pub type PersonaOf = Box<dyn Fn(u32) -> Option<String> + Send>;

/// What the windows should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Open the sheet for `embed` (hidden until limend binds it), then call
    /// [`Sheet::ready`] with its embed token. `targets` may be empty: the
    /// sheet says no app takes this.
    /// `from_persona`: the sharing app's persona and its colour, when
    /// the person has personas.
    Show { embed: EmbedId, title: String, from_app: String, from_persona: Option<(String, Option<String>)>, targets: Vec<Target> },
    /// The app detached the sheet or went: close its window.
    Close { embed: EmbedId },
}

struct Pending {
    parent_uid: u32,
    parent_app: String,
    opened: bool,
    targets: Vec<Target>,
    /// The app has had its answer.
    done: bool,
}

pub struct Sheet {
    child: Child,
    gate: Box<dyn Gate>,
    installed: Installed,
    person: Person,
    persona_of: PersonaOf,
    embeds: HashMap<EmbedId, Pending>,
    /// Where each person's `share.toml` is (`<dir>/<person>/share.toml`):
    /// the apps they hid and their order, from Settings.
    prefs: std::path::PathBuf,
}

impl Sheet {
    /// `child` is the connection that provides `share` (see
    /// [`Child::provider`]).
    pub fn new(child: Child, gate: Box<dyn Gate>, installed: Installed, person: Person) -> Self {
        Sheet { child, gate, installed, person, persona_of: Box::new(|_| None), embeds: HashMap::new(), prefs: "/var/db/atrium".into() }
    }

    /// Read people's choices under `dir` instead of /var/db/atrium.
    pub fn with_prefs(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.prefs = dir.into();
        self
    }

    /// Tell which persona each app runs in (by default, all Personal).
    pub fn with_personas(mut self, persona_of: PersonaOf) -> Self {
        self.persona_of = persona_of;
        self
    }

    /// Serve limend for up to `wait`.
    pub fn poll(&mut self, wait: Duration) -> Result<Option<Event>, Error> {
        match self.child.poll(wait)? {
            Some(ChildEvent::Attach { embed, role, parent_app, parent_uid }) if role == "share" => {
                eprintln!("share-sheet: {embed} for {parent_app} (uid {parent_uid})");
                self.embeds.insert(embed, Pending { parent_uid, parent_app, opened: false, targets: Vec::new(), done: false });
                Ok(None)
            }
            Some(ChildEvent::Message { embed, bytes }) => Ok(self.open(embed, &bytes)),
            Some(ChildEvent::Detaching { embed }) => Ok(self.embeds.remove(&embed).map(|_| Event::Close { embed })),
            _ => Ok(None),
        }
    }

    fn fail(&mut self, embed: EmbedId, why: &str) {
        eprintln!("share-sheet: {embed}: {why}");
        if let Some(p) = self.embeds.get_mut(&embed) {
            p.done = true;
        }
        let _ = self.child.emit(embed, ToParent::Failed { why: why.into() }.encode());
    }

    fn open(&mut self, embed: EmbedId, bytes: &[u8]) -> Option<Event> {
        let p = self.embeds.get_mut(&embed)?;
        // One open per sheet; a second is ignored.
        if p.opened {
            return None;
        }
        p.opened = true;
        let Some(ToChild::Open(o)) = ToChild::decode(bytes) else {
            self.fail(embed, "an odd request");
            return None;
        };
        let parent_uid = p.parent_uid;
        // The press must be the app's own, fresh, in its focused window.
        match self.gate.consume(Consume { token: o.activation, presenter_uid: parent_uid, purpose: purpose::SHARE }) {
            ConsumeResult::Ok { .. } => {}
            ConsumeResult::Refused { reason } => {
                self.fail(embed, &format!("no fresh press in the app's window (frescod: {reason})"));
                return None;
            }
        }
        let Some(person) = (self.person)(parent_uid) else {
            self.fail(embed, "not an app");
            return None;
        };
        let p = self.embeds.get_mut(&embed)?;
        let from = (self.persona_of)(parent_uid);
        let mut all = self.installed.share_targets(&person, &o.mimes);
        let mut from_persona = None;
        let mut crossed = Vec::new();
        if !person.is_empty() && !person.contains(['/', '\0']) && !person.starts_with('.') {
            // The person's order, without the apps they hid (Settings'
            // Share section). A hidden app isn't shown, so it can't be chosen.
            let prefs = SharePrefs::load(&self.prefs.join(&person).join("share.toml")).unwrap_or_default();
            all = prefs.shown(all, |t| t.app.as_str());
            // Each other persona the person lets this one share to, in
            // the order Settings lists them: Personal, then by name.
            let policy = Policy::load(&self.prefs.join(&person).join("policy.toml")).unwrap_or_default();
            if !policy.personas.is_empty() && policy.has_persona(from.as_deref()) {
                let named = |n: Option<&str>| n.unwrap_or(PERSONAL).to_string();
                let colour = |n: Option<&str>| n.and_then(|n| policy.personas.get(n)).map(|d| d.colour.clone());
                from_persona = Some((named(from.as_deref()), colour(from.as_deref())));
                let others = std::iter::once(None).chain(policy.personas.keys().map(|k| Some(k.as_str())));
                for q in others.filter(|q| *q != from.as_deref() && policy.may_cross(from.as_deref(), *q, "share")) {
                    // The sharing app's own other instance is a fine target.
                    crossed.extend(all.iter().cloned().map(|mut t| {
                        t.persona = Some(named(q));
                        t.colour = colour(q);
                        t
                    }));
                }
            }
        }
        all.retain(|t| t.app != p.parent_app);
        let crossings = crossed.len();
        all.extend(crossed);
        let targets = all;
        eprintln!("share-sheet: {embed}: {} types, {} targets for {person} ({crossings} in other personas)", o.mimes.len(), targets.len());
        p.targets = targets.clone();
        Some(Event::Show { embed, title: o.title, from_app: p.parent_app.clone(), from_persona, targets })
    }

    /// The sheet's window for `embed` is open: `window` is its embed token.
    pub fn ready(&mut self, embed: EmbedId, window: WindowToken) -> Result<(), Error> {
        self.child.ready(embed, window)
    }

    /// The person pressed `app` in the sheet, in `persona` for a row of
    /// another persona's. Only a row the sheet showed.
    pub fn chose(&mut self, embed: EmbedId, app: &str, persona: Option<&str>) -> Result<(), String> {
        let p = self.embeds.get_mut(&embed).ok_or("no such sheet")?;
        if p.done {
            return Err("already answered".into());
        }
        if !p.targets.iter().any(|t| t.app == app && t.persona.as_deref() == persona) {
            return Err(format!("{app} in {persona:?} was not offered"));
        }
        let chosen = match persona {
            Some(q) => self.child.choose_in(embed, app, q),
            None => self.child.choose(embed, app),
        };
        chosen.map_err(|e| format!("limend refused the choice ({e:?})"))?;
        p.done = true;
        self.child.emit(embed, ToParent::Chosen.encode()).map_err(|e| format!("{e:?}"))?;
        match persona {
            Some(q) => eprintln!("share-sheet: {embed}: chose {app} in {q}"),
            None => eprintln!("share-sheet: {embed}: chose {app}"),
        }
        Ok(())
    }

    /// The person closed the sheet (or its window went).
    pub fn cancelled(&mut self, embed: EmbedId) {
        if let Some(p) = self.embeds.get_mut(&embed) {
            if !p.done {
                p.done = true;
                let _ = self.child.emit(embed, ToParent::Cancelled.encode());
                eprintln!("share-sheet: {embed}: cancelled");
            }
        }
    }
}

/// The person behind a uid, from the launch registry at `path`.
pub fn registry_person(path: String) -> Person {
    Box::new(move |uid| {
        let reg = portcullis_peer::AppRegistry::load(&path).ok()?;
        reg.resolve(uid).map(|(u, _)| u.to_string())
    })
}

/// The persona of a uid's instance, from the launch registry at `path`.
pub fn registry_persona(path: String) -> PersonaOf {
    Box::new(move |uid| {
        let reg = portcullis_peer::AppRegistry::load(&path).ok()?;
        reg.persona(uid).flatten().map(str::to_string)
    })
}
