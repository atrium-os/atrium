//! Who is connected (arca.md §4.1, amended 2026-11-06).
//!
//! The kernel's peer uid, resolved through the launch registry that
//! portcullisd writes, never anything the peer says. The registry carries
//! the manifest's verified publisher id, so an app's label is (owning user,
//! publisher, app id).

use std::path::PathBuf;

/// What Arca knows about a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    /// A launched app.
    App { user: String, publisher: [u8; 32], app_id: String },
    /// Vestibulum: unlocks and locks stores; never sees item values.
    Vestibulum,
    /// Arca's own trusted UI (grants, reveal); a separate process of Arca's.
    TrustedUi,
    /// The web broker (arca.md §8.2): a launched app the configuration
    /// names by publisher and app id. It issues tickets; otherwise it is
    /// that app.
    WebBroker { user: String, publisher: [u8; 32], app_id: String },
    /// An Atrium system service with a fixed uid (`ARCA_SERVICES`): its own
    /// items in the machine store (arca.md §6.4), e.g. vinculumd's bonds.
    Service(String),
    /// A courier (arca.md §5): carries one persona's items to one endpoint.
    Courier(crate::courier::CourierLabel),
    /// A share server (network-shares.md §5.1): one share's items, after
    /// its handshake proved the pinned server identity.
    Share(crate::share::ShareLabel),
    /// The system file manager (`ARCA_FILE_MANAGER`, network-shares.md
    /// §5.2): a launched app that may also save shares' credentials from
    /// its own credential field and mint shares' SSH keys. Otherwise it is
    /// that app.
    FileManager { user: String, publisher: [u8; 32], app_id: String },
    /// Anything else: refused every op.
    Unknown,
}

/// Configuration for resolving peers.
#[derive(Debug, Clone)]
pub struct Resolver {
    pub registry: PathBuf,
    /// The fixed system uid Vestibulum runs as.
    pub vestibulum_uid: Option<u32>,
    pub ui_uid: Option<u32>,
    /// Development only: serve apps launched without a verified publisher,
    /// under the all-zero publisher id. Off in every shipped configuration:
    /// with it on, any unsigned app with the same app id is the same app.
    pub allow_unsigned: bool,
    /// The courier registry; `None` serves no couriers.
    pub courier_registry: Option<PathBuf>,
    /// The web broker's (publisher, app id); `None`: no web profile.
    pub web_broker: Option<([u8; 32], String)>,
    /// The share registry; `None` serves no share servers.
    pub share_registry: Option<PathBuf>,
    /// The file manager's (publisher, app id), `ARCA_FILE_MANAGER` in the
    /// web broker's format; `None`: nobody saves shares.
    pub file_manager: Option<([u8; 32], String)>,
    /// System services by uid (`ARCA_SERVICES`); each reaches only its own
    /// items in the machine store.
    pub services: Vec<(u32, String)>,
}

/// `ARCA_SERVICES`: `name=uid[,name=uid...]`, e.g. `vinculumd=65023`. A
/// malformed entry is dropped; a uid or name given twice drops both.
pub fn parse_services(s: &str) -> Vec<(u32, String)> {
    let all: Vec<(u32, String)> = s
        .split(',')
        .filter_map(|e| {
            let (n, u) = e.trim().split_once('=')?;
            let uid: u32 = u.trim().parse().ok()?;
            (arca_store::model::valid_service(n.trim()) && uid != 0).then(|| (uid, n.trim().to_string()))
        })
        .collect();
    all.iter()
        .filter(|(u, n)| all.iter().filter(|(u2, n2)| u2 == u || n2 == n).count() == 1)
        .cloned()
        .collect()
}

/// `ARCA_WEB_BROKER`: `app_id publisher-hex` (64 hex). Unsigned brokers
/// are never configured.
pub fn parse_web_broker(s: &str) -> Option<([u8; 32], String)> {
    let (app, hex) = s.trim().split_once(' ')?;
    let hex = hex.trim();
    if hex.len() != 64 || app.is_empty() {
        return None;
    }
    let mut p = [0u8; 32];
    for (i, b) in p.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    (p != UNSIGNED_PUBLISHER).then(|| (p, app.to_string()))
}

/// The all-zero publisher id used for unsigned apps when allowed.
pub const UNSIGNED_PUBLISHER: [u8; 32] = [0; 32];

impl Resolver {
    pub fn resolve(&self, uid: u32) -> Label {
        if (crate::courier::UID_FIRST..=crate::courier::UID_LAST).contains(&uid) {
            // Courier uids are never apps or system services.
            return self
                .courier_registry
                .as_ref()
                .and_then(|p| crate::courier::lookup(p.to_str()?, uid))
                .map_or(Label::Unknown, Label::Courier);
        }
        if (crate::share::UID_FIRST..=crate::share::UID_LAST).contains(&uid) {
            return self
                .share_registry
                .as_ref()
                .and_then(|p| crate::share::lookup(p.to_str()?, uid))
                .map_or(Label::Unknown, Label::Share);
        }
        // Read fresh each time: portcullisd appends at every launch. A read
        // failure is "unknown": fail closed.
        let Ok(reg) = portcullis_peer::AppRegistry::load(self.registry.to_str().unwrap_or("")) else {
            return Label::Unknown;
        };
        let service = self.services.iter().find(|(u, _)| *u == uid).map(|(_, n)| n.clone());
        let system = Some(uid) == self.vestibulum_uid || Some(uid) == self.ui_uid || service.is_some();
        let Some((user, app_id)) = reg.resolve(uid) else {
            return match () {
                _ if Some(uid) == self.vestibulum_uid => Label::Vestibulum,
                _ if Some(uid) == self.ui_uid => Label::TrustedUi,
                _ if service.is_some() => Label::Service(service.unwrap()),
                _ => Label::Unknown,
            };
        };
        if system {
            // A system uid that the launcher also gave to an app: trust neither.
            return Label::Unknown;
        }
        let publisher = match reg.publisher(uid) {
            Some(Some(p)) => p,
            Some(None) if self.allow_unsigned => UNSIGNED_PUBLISHER,
            _ => return Label::Unknown,
        };
        if publisher == UNSIGNED_PUBLISHER && !self.allow_unsigned {
            return Label::Unknown;
        }
        if self.web_broker.as_ref().is_some_and(|(p, a)| *p == publisher && a == app_id) {
            return Label::WebBroker { user: user.into(), publisher, app_id: app_id.into() };
        }
        if self.file_manager.as_ref().is_some_and(|(p, a)| *p == publisher && a == app_id) {
            return Label::FileManager { user: user.into(), publisher, app_id: app_id.into() };
        }
        Label::App { user: user.into(), publisher, app_id: app_id.into() }
    }
}
