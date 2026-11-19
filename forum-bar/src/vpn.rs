//! The bar's VPN item and its menu (vpn.md §7.4).
//!
//! The icon follows crypta-ctl's tunnel list (`vpn-read`): green while a
//! tunnel is connected, amber while one is connecting (with the block switch
//! on, its traffic waits meanwhile), grey when all are down. The menu
//! connects and disconnects the saved tunnels (`vpn-use`); everything else
//! is on Settings' VPN page. Shown only while there is a tunnel to show.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use crypta_ctl::client::Client;
use crypta_ctl::{Grant, Scope, State, Tunnel};
use pergola::widgets::popup::MenuItem;

fn dir() -> PathBuf {
    std::env::var_os("FORUM_BAR_NET_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/atrium/sockets"))
}

fn connect(g: Grant) -> Result<Client, String> {
    Client::connect(&Client::path(&dir(), g)).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VpnShown {
    /// None: crypta-ctl is not reachable.
    pub tunnels: Option<Vec<Tunnel>>,
    /// The last connect or disconnect that did not work.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VpnAction {
    Connect(String),
    Disconnect(String),
}

pub enum VpnMsg {
    Tunnels(Option<Vec<Tunnel>>),
    Done(Result<(), String>),
}

/// How the icon looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Up,
    Connecting,
    Down,
}

impl VpnShown {
    /// Whether the item is on the bar at all.
    pub fn shown(&self) -> bool {
        self.tunnels.as_ref().is_some_and(|v| !v.is_empty())
    }

    pub fn look(&self) -> Look {
        let v = self.tunnels.as_deref().unwrap_or(&[]);
        if v.iter().any(|t| matches!(t.state, State::Connecting | State::SignIn)) {
            Look::Connecting
        } else if v.iter().any(|t| t.state == State::Up) {
            Look::Up
        } else {
            Look::Down
        }
    }
}

/// The menu and what each item does.
pub fn menu(n: &VpnShown) -> (Vec<MenuItem>, Vec<Option<VpnAction>>) {
    let mut items = vec![MenuItem::new("VPN").enabled(false)];
    let mut acts = vec![None];
    if let Some(note) = &n.note {
        items.push(MenuItem::new(note.clone()).enabled(false));
        acts.push(None);
    }
    for (i, t) in n.tunnels.as_deref().unwrap_or(&[]).iter().enumerate() {
        let whom = if t.scope == Scope::Personas { ", chosen personas" } else { "" };
        let (label, act) = match t.state {
            State::Up => (format!("Disconnect {} (connected{whom})", t.name), VpnAction::Disconnect(t.name.clone())),
            State::Connecting => (format!("Disconnect {} (connecting{whom})", t.name), VpnAction::Disconnect(t.name.clone())),
            // vpn.md §7.4: the sign-in is on Settings' VPN page.
            State::SignIn => (format!("Disconnect {} (sign in on Settings' VPN page{whom})", t.name), VpnAction::Disconnect(t.name.clone())),
            State::Down => (format!("Connect {}", t.name), VpnAction::Connect(t.name.clone())),
        };
        let it = MenuItem::new(label);
        items.push(if i == 0 { it.separated() } else { it });
        acts.push(Some(act));
    }
    (items, acts)
}

/// Follow the tunnels on a thread of its own; reconnect when the front
/// restarts. None while it is unreachable.
pub fn follow() -> (Receiver<VpnMsg>, Sender<VpnMsg>) {
    let (tx, rx) = channel();
    let t = tx.clone();
    let mut backoff = atrium_listen::Backoff::new(Duration::from_secs(3));
    std::thread::spawn(move || loop {
        let began = std::time::Instant::now();
        let sub = connect(Grant::Read).and_then(|c| c.subscribe().map_err(|f| f.1));
        if let Ok(mut ch) = sub {
            while let Ok(v) = ch.next() {
                if t.send(VpnMsg::Tunnels(Some(v))).is_err() {
                    return;
                }
            }
        }
        if t.send(VpnMsg::Tunnels(None)).is_err() {
            return;
        }
        backoff.after(Client::path(&dir(), Grant::Read), began.elapsed());
    });
    (rx, tx)
}

pub fn act(tx: Sender<VpnMsg>, a: VpnAction) {
    std::thread::spawn(move || {
        let r = connect(Grant::Use).and_then(|mut c| {
            match &a {
                VpnAction::Connect(n) => c.connect_tunnel(n),
                VpnAction::Disconnect(n) => c.disconnect_tunnel(n),
            }
            .map_err(|f| f.1)
        });
        match (&a, &r) {
            (_, Ok(())) => eprintln!("forum-bar: vpn: {a:?}"),
            (_, Err(e)) => eprintln!("forum-bar: vpn: {a:?} failed: {e}"),
        }
        let _ = tx.send(VpnMsg::Done(r));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypta_ctl::Kind;

    fn t(name: &str, state: State, scope: Scope) -> Tunnel {
        Tunnel { name: name.into(), kind: Kind::WireGuard, state, scope }
    }

    #[test]
    fn hidden_without_tunnels_and_looks_by_state() {
        assert!(!VpnShown::default().shown());
        assert!(!VpnShown { tunnels: Some(vec![]), note: None }.shown());
        let n = VpnShown { tunnels: Some(vec![t("acme", State::Up, Scope::Personas), t("home", State::Down, Scope::Machine)]), note: None };
        assert!(n.shown());
        assert_eq!(n.look(), Look::Up);
        let c = VpnShown { tunnels: Some(vec![t("acme", State::Up, Scope::Machine), t("home", State::Connecting, Scope::Machine)]), note: None };
        assert_eq!(c.look(), Look::Connecting, "a connecting one is what matters");
        let (items, acts) = menu(&n);
        assert_eq!(items.len(), 3);
        assert_eq!(acts[1], Some(VpnAction::Disconnect("acme".into())));
        assert_eq!(acts[2], Some(VpnAction::Connect("home".into())));
    }
}
