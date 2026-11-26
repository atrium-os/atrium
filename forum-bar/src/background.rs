//! The bar's "Running in the background" item (background-apps.md §3.2).
//!
//! Every background app the person has running is listed here, named by
//! portcullisd from the installed manifest, never by the app: Quit stops it
//! for this session, and "Don't open at sign-in" turns its switch off. A bar
//! app is listed too, so quitting it is always here even if its own menu
//! has no Quit. Shown only while something runs in the background.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use portcullis_ipc::{round_trip, BackgroundApp, Request, Response, PROTO_VERSION};
use pergola::widgets::popup::MenuItem;

/// How often the list is asked for while the bar runs.
const EVERY: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BgShown {
    pub apps: Vec<BackgroundApp>,
    /// The last Quit or switch that did not work.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BgAction {
    Quit { app_id: String, persona: Option<String> },
    NotAtSignIn { app_id: String, persona: Option<String> },
}

pub enum BgMsg {
    Apps(Vec<BackgroundApp>),
    Done(Result<(), String>),
}

impl BgShown {
    pub fn shown(&self) -> bool {
        !self.apps.is_empty()
    }
}

/// "Clipboard History", or "Clipboard History (Work)".
fn label(a: &BackgroundApp) -> String {
    match &a.persona {
        Some(p) => {
            let mut c = p.chars();
            let p: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
            format!("{} ({p})", a.name)
        }
        None => a.name.clone(),
    }
}

/// The menu and what each item does.
pub fn menu(n: &BgShown) -> (Vec<MenuItem>, Vec<Option<BgAction>>) {
    let mut items = vec![MenuItem::new("Running in the background").enabled(false)];
    let mut acts = vec![None];
    if let Some(note) = &n.note {
        items.push(MenuItem::new(note.clone()).enabled(false));
        acts.push(None);
    }
    for a in &n.apps {
        let name = label(a);
        let state = if a.paused { format!("{name}: stopped after crashing, retrying soon") } else { name.clone() };
        items.push(MenuItem::new(state).enabled(false).separated());
        acts.push(None);
        // Its ports (background-apps.md §4.3): how busy, and where not.
        let mut about = Vec::new();
        match a.connections {
            Some(1) => about.push("1 connection".to_string()),
            Some(n) if n > 0 => about.push(format!("{n} connections")),
            _ => {}
        }
        if a.not_on_this_network {
            about.push("Not reachable on this network".into());
        }
        if !about.is_empty() {
            items.push(MenuItem::new(about.join(" · ")).enabled(false));
            acts.push(None);
        }
        items.push(MenuItem::new(format!("Quit {name}")));
        acts.push(Some(BgAction::Quit { app_id: a.app_id.clone(), persona: a.persona.clone() }));
        if a.at_sign_in {
            items.push(MenuItem::new(format!("Don't open {name} at sign-in")));
            acts.push(Some(BgAction::NotAtSignIn { app_id: a.app_id.clone(), persona: a.persona.clone() }));
        }
    }
    (items, acts)
}

fn ask(req: &Request) -> Result<Response, String> {
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        Ok(other) => return Err(format!("portcullisd: {other:?}")),
        Err(e) => return Err(format!("portcullisd is not answering ({e})")),
    }
    round_trip(&mut s, req).map_err(|e| format!("portcullisd is not answering ({e})"))
}

/// Ask for the list on a thread of its own, every few seconds. An empty
/// list while portcullisd is away: the item hides.
pub fn follow() -> (Receiver<BgMsg>, Sender<BgMsg>) {
    let (tx, rx) = channel();
    let t = tx.clone();
    std::thread::spawn(move || loop {
        let apps = match ask(&Request::BackgroundApps) {
            Ok(Response::BackgroundApps { apps }) => apps,
            _ => Vec::new(),
        };
        if t.send(BgMsg::Apps(apps)).is_err() {
            return;
        }
        std::thread::sleep(EVERY);
    });
    (rx, tx)
}

pub fn act(tx: Sender<BgMsg>, a: BgAction) {
    std::thread::spawn(move || {
        let req = match &a {
            BgAction::Quit { app_id, persona } => Request::BackgroundQuit { app_id: app_id.clone(), persona: persona.clone() },
            BgAction::NotAtSignIn { app_id, persona } => Request::BackgroundOff { app_id: app_id.clone(), persona: persona.clone() },
        };
        let r = match ask(&req) {
            Ok(Response::Ok) => Ok(()),
            Ok(Response::Error { message }) => Err(message),
            Ok(other) => Err(format!("{other:?}")),
            Err(e) => Err(e),
        };
        match &r {
            Ok(()) => eprintln!("forum-bar: background: {a:?}"),
            Err(e) => eprintln!("forum-bar: background: {a:?} failed: {e}"),
        }
        let _ = tx.send(BgMsg::Done(r));
        if let Ok(Response::BackgroundApps { apps }) = ask(&Request::BackgroundApps) {
            let _ = tx.send(BgMsg::Apps(apps));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, persona: Option<&str>, paused: bool, at_sign_in: bool) -> BackgroundApp {
        BackgroundApp { app_id: id.into(), name: name.into(), persona: persona.map(String::from), paused, at_sign_in,
                        connections: None, not_on_this_network: false }
    }

    #[test]
    fn hidden_while_nothing_runs_and_each_app_gets_quit() {
        assert!(!BgShown::default().shown());
        let n = BgShown { apps: vec![
            app("org.x.clip", "Clipboard History", None, false, true),
            app("org.x.srv", "Photo Server", Some("work"), true, false),
        ], note: None };
        assert!(n.shown());
        let (items, acts) = menu(&n);
        assert_eq!(items.len(), acts.len());
        let quits: Vec<_> = acts.iter().flatten().collect();
        assert_eq!(quits, vec![
            &BgAction::Quit { app_id: "org.x.clip".into(), persona: None },
            &BgAction::NotAtSignIn { app_id: "org.x.clip".into(), persona: None },
            &BgAction::Quit { app_id: "org.x.srv".into(), persona: Some("work".into()) },
        ], "a manual app has no sign-in switch to turn off");
        assert_eq!(acts[0], None, "the heading does nothing");
    }

    #[test]
    fn a_server_says_how_busy_it_is_and_where_it_isnt_reachable() {
        let mut srv = app("org.x.srv", "Photo Server", None, false, true);
        srv.connections = Some(3);
        srv.not_on_this_network = true;
        let (items, acts) = menu(&BgShown { apps: vec![srv], note: None });
        assert_eq!(items.len(), acts.len());
        let words: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
        assert!(words.contains(&"3 connections · Not reachable on this network".to_string()), "{words:?}");
        let quiet = app("org.x.clip", "Clipboard History", None, false, true);
        let (items, _) = menu(&BgShown { apps: vec![quiet], note: None });
        assert_eq!(items.len(), 4, "no line for an app with no ports");
    }

    #[test]
    fn names_say_the_persona() {
        assert_eq!(label(&app("a.b", "Photo Server", Some("work"), false, false)), "Photo Server (Work)");
        assert_eq!(label(&app("a.b", "Photo Server", None, false, false)), "Photo Server");
    }
}
