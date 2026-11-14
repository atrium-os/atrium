//! The bar's network icon and its menu (docs/spec/atrium-net.md §7.1).
//!
//! The icon follows atrium-netconf's status (the `network-wifi` socket, so
//! the menu can name the network). Opening the menu scans (`wifi-nearby`);
//! picking a remembered or open network joins it, and Disconnect leaves.
//! A network that needs a passphrase is joined from Settings, which can take
//! one; the bar's menu only picks.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use atrium_netconf::client::Client;
use atrium_netconf::{Grant, Join, Link, Nearby, Online, Status};
use pergola::widgets::phosphor;
use pergola::widgets::popup::MenuItem;

fn dir() -> PathBuf {
    std::env::var_os("FORUM_BAR_NET_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/atrium/sockets"))
}

fn connect(g: Grant) -> Result<Client, String> {
    Client::connect(&Client::path(&dir(), g)).map_err(|e| e.to_string())
}

/// What the bar shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetShown {
    /// None: atrium-netconf is not reachable.
    pub status: Option<Status>,
    pub nearby: Vec<Nearby>,
    pub scanning: bool,
    /// The last join that did not work.
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NetAction {
    Join { ssid: Vec<u8>, open: bool },
    Leave,
}

pub enum NetMsg {
    Status(Option<Status>),
    Nearby(Result<Vec<Nearby>, String>),
    Done(Result<(), String>),
}

fn name(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

/// The icon: a cable, Wi-Fi by its bars, crossed out with no internet, or
/// slashed with no network.
pub fn glyph(s: Option<&Status>) -> char {
    let Some(s) = s else { return phosphor::WIFI_SLASH };
    let cut = matches!(s.online, Online::No | Online::Captive);
    match s.link {
        Link::None => phosphor::WIFI_SLASH,
        Link::Wired if cut => phosphor::GLOBE_X,
        Link::Wired => phosphor::PLUGS_CONNECTED,
        Link::Wifi if cut => phosphor::WIFI_X,
        Link::Wifi => match s.bars.unwrap_or(4) {
            4 | 3 => phosphor::WIFI_HIGH,
            2 => phosphor::WIFI_MEDIUM,
            1 => phosphor::WIFI_LOW,
            _ => phosphor::WIFI_NONE,
        },
    }
}

/// The menu and what each item does.
pub fn menu(n: &NetShown) -> (Vec<MenuItem>, Vec<Option<NetAction>>) {
    let mut items = Vec::new();
    let mut acts = Vec::new();
    let mut head = match &n.status {
        None => "Network settings unavailable".to_string(),
        Some(s) => match s.link {
            Link::None => "Not connected".to_string(),
            Link::Wired => "Ethernet".to_string(),
            Link::Wifi => format!("Wi-Fi: {}", s.ssid.as_deref().map(name).unwrap_or_default()),
        },
    };
    match n.status.as_ref().map(|s| s.online) {
        Some(Online::No) if n.status.as_ref().is_some_and(|s| s.link != Link::None) => head.push_str(" — no internet"),
        Some(Online::Captive) => head.push_str(" — sign-in needed"),
        _ => {}
    }
    items.push(MenuItem::new(head).enabled(false));
    acts.push(None);
    if let Some(note) = &n.note {
        items.push(MenuItem::new(note.clone()).enabled(false));
        acts.push(None);
    }
    let current = n.status.as_ref().filter(|s| s.link == Link::Wifi).and_then(|s| s.ssid.clone());
    if current.is_some() {
        items.push(MenuItem::new("Disconnect"));
        acts.push(Some(NetAction::Leave));
    }
    let others: Vec<&Nearby> = n.nearby.iter().filter(|x| Some(&x.ssid) != current.as_ref()).take(8).collect();
    if others.is_empty() && n.scanning {
        items.push(MenuItem::new("Looking for networks…").enabled(false).separated());
        acts.push(None);
    }
    for (i, x) in others.into_iter().enumerate() {
        let open = x.security == "open";
        let (label, act) = if x.known {
            (name(&x.ssid), Some(NetAction::Join { ssid: x.ssid.clone(), open }))
        } else if open {
            (format!("{} (open)", name(&x.ssid)), Some(NetAction::Join { ssid: x.ssid.clone(), open: true }))
        } else {
            (format!("{} — passphrase in Settings", name(&x.ssid)), None)
        };
        let it = MenuItem::new(label).enabled(act.is_some());
        items.push(if i == 0 { it.separated() } else { it });
        acts.push(act);
    }
    (items, acts)
}

/// Follow the status on a thread of its own; reconnect when the front
/// restarts. None while it is unreachable.
pub fn follow() -> (Receiver<NetMsg>, Sender<NetMsg>) {
    let (tx, rx) = channel();
    let t = tx.clone();
    std::thread::spawn(move || loop {
        let sub = connect(Grant::Wifi).or_else(|_| connect(Grant::Read)).and_then(|c| c.subscribe().map_err(|f| f.1));
        if let Ok(mut ch) = sub {
            while let Ok(s) = ch.next() {
                if t.send(NetMsg::Status(Some(s))).is_err() {
                    return;
                }
            }
        }
        if t.send(NetMsg::Status(None)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
    });
    (rx, tx)
}

/// Scan (the menu opened).
pub fn scan(tx: Sender<NetMsg>) {
    std::thread::spawn(move || {
        let r = connect(Grant::Nearby).and_then(|mut c| c.nearby().map_err(|f| f.1));
        let _ = tx.send(NetMsg::Nearby(r));
    });
}

pub fn act(tx: Sender<NetMsg>, a: NetAction) {
    std::thread::spawn(move || {
        let r = connect(Grant::Wifi).and_then(|mut c| {
            match &a {
                NetAction::Join { ssid, open } => c.join(&Join { ssid: ssid.clone(), passphrase: None, open: *open }),
                NetAction::Leave => c.leave(),
            }
            .map_err(|f| f.1)
        });
        let _ = tx.send(NetMsg::Done(r));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(link: Link, online: Online, bars: Option<u8>, ssid: Option<&str>) -> Status {
        Status { link, online, metered: false, bars, ssid: ssid.map(|s| s.as_bytes().to_vec()), portal: None }
    }

    #[test]
    fn glyph_follows_the_link() {
        assert_eq!(glyph(None), phosphor::WIFI_SLASH);
        assert_eq!(glyph(Some(&Status::offline())), phosphor::WIFI_SLASH);
        assert_eq!(glyph(Some(&st(Link::Wired, Online::Unknown, None, None))), phosphor::PLUGS_CONNECTED);
        assert_eq!(glyph(Some(&st(Link::Wired, Online::No, None, None))), phosphor::GLOBE_X);
        assert_eq!(glyph(Some(&st(Link::Wifi, Online::Yes, Some(2), None))), phosphor::WIFI_MEDIUM);
        assert_eq!(glyph(Some(&st(Link::Wifi, Online::Captive, Some(4), None))), phosphor::WIFI_X);
    }

    #[test]
    fn menu_joins_known_and_open_networks_only() {
        let near = |s: &str, sec: &str, known| Nearby { ssid: s.as_bytes().to_vec(), security: sec.into(), bars: 3, known };
        let n = NetShown {
            status: Some(st(Link::Wifi, Online::Unknown, Some(3), Some("home"))),
            nearby: vec![near("home", "wpa2", true), near("cafe", "open", false), near("office", "wpa3", true), near("neighbour", "wpa2", false)],
            scanning: false,
            note: None,
        };
        let (items, acts) = menu(&n);
        assert_eq!(items.len(), acts.len());
        assert_eq!(acts[0], None, "the heading");
        assert_eq!(acts[1], Some(NetAction::Leave));
        assert_eq!(acts[2], Some(NetAction::Join { ssid: b"cafe".to_vec(), open: true }));
        assert_eq!(acts[3], Some(NetAction::Join { ssid: b"office".to_vec(), open: false }));
        assert_eq!(acts[4], None, "a new secured network needs Settings");
        assert_eq!(acts.len(), 5, "the current network is not offered again");
    }
}
