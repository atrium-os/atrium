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
}

/// The all-zero publisher id used for unsigned apps when allowed.
pub const UNSIGNED_PUBLISHER: [u8; 32] = [0; 32];

impl Resolver {
    pub fn resolve(&self, uid: u32) -> Label {
        if Some(uid) == self.vestibulum_uid {
            return Label::Vestibulum;
        }
        if Some(uid) == self.ui_uid {
            return Label::TrustedUi;
        }
        // Read fresh each time: portcullisd appends at every launch. A read
        // failure is "unknown": fail closed.
        let Ok(reg) = portcullis_peer::AppRegistry::load(self.registry.to_str().unwrap_or("")) else {
            return Label::Unknown;
        };
        let Some((user, app_id)) = reg.resolve(uid) else { return Label::Unknown };
        let publisher = match reg.publisher(uid) {
            Some(Some(p)) => p,
            Some(None) if self.allow_unsigned => UNSIGNED_PUBLISHER,
            _ => return Label::Unknown,
        };
        if publisher == UNSIGNED_PUBLISHER && !self.allow_unsigned {
            return Label::Unknown;
        }
        Label::App { user: user.into(), publisher, app_id: app_id.into() }
    }
}
