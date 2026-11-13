//! Who is asking (sigillum.md §7.1). The label comes from the peer uid the
//! kernel reports and the launch registry, never from a message.

/// An app as the launch registry names it: publisher key and app id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppId {
    pub publisher: [u8; 32],
    pub app_id: String,
}

impl AppId {
    /// The app's private RP ID, which no website can share (§13).
    pub fn private_rp(&self) -> String {
        let hex: String = self.publisher.iter().map(|b| format!("{b:02x}")).collect();
        format!("app:{hex}/{}", self.app_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    /// A launched app, with the RP IDs its manifest's `identity.webauthn`
    /// declared and Opifex verified at install.
    /// `user` is the owning user the launch registry names.
    App { user: String, app: AppId, rp_ids: Vec<String> },
    /// arcad, by its fixed uid.
    Arca,
    /// ostiarius, by its fixed uid.
    Ostiarius,
    /// No registry entry: gets nothing.
    Unknown,
}

/// Resolves a peer uid. The daemon's implementation reads the launch
/// registry (`portcullis-peer`) and the fixed service uids.
pub trait Registry {
    fn label(&self, uid: u32) -> Label;
}
