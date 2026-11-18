//! opifexd's wire: one JSON request per line, one JSON answer per line, on
//! `/atrium/sockets/opifex/opifex.sock`. Who may ask what is decided by the
//! caller's uid (opifex.md §6.6, §7.1): anyone may ask the read-only
//! questions; installs, removals, rollbacks and syncs are root's, except
//! that the install sheet's uid may install a catalog app a person approved
//! on it (`InstallApproved`, C3).

use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/atrium/sockets/opifex/opifex.sock";
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// What is installed.
    Installed,
    /// What the catalog says about an id (no network).
    Describe { id: String },
    /// Ids whose id, name or description contains `text`.
    Search { text: String },
    /// Install the newest release of `id` from the local catalog, downloading
    /// its archive through the fetcher unless it is in the download folder.
    Install { id: String },
    /// Install a local bundle folder or `.insula` file (side-load).
    InstallLocal { path: String },
    /// From the install sheet only (`opifex-sheet`, its own uid): install
    /// `id` for the person `for_uid`, who approved the release whose
    /// manifest hashes to `manifest_sha256` (what the sheet showed). A newer
    /// release synced meanwhile is refused, not installed (C3).
    InstallApproved { id: String, manifest_sha256: String, for_uid: u32 },
    Uninstall { id: String },
    Rollback { id: String },
    /// Installed apps with a newer release, and what will happen to each.
    Updates,
    /// A person's care for one app's updates: `"notify"` (wait for me) or
    /// `"auto"` (the publisher's class). Root, or the person who installed it.
    SetUpdates { id: String, care: String },
    /// Sync over the network, install what may go in the background, and
    /// announce the rest (what the daily timer does). Root.
    Tick,
    /// Check the catalog into the local copy: from the network (`from` empty:
    /// where the catalog says it lives; or an `https://` base) through the
    /// fetcher, or from a folder.
    Sync { from: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Described {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: String,
    pub latest: String,
    pub publisher: String,
    pub catalog: String,
    pub capabilities: Vec<String>,
    pub archive_size: u64,
    pub installed: Option<String>,
    /// sha256 of the release's `atrium.toml`: what the sheet approves.
    #[serde(default)]
    pub manifest_sha256: String,
    /// On an update: the capabilities the new release asks for that the
    /// installed one doesn't (what the sheet shows).
    #[serde(default)]
    pub added: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Response {
    Installed { apps: Vec<crate::installed::Record> },
    Described { app: Described },
    Found { ids: Vec<String> },
    Done { id: String, version: String },
    Synced { entries: u64, releases: usize },
    Updates { items: Vec<crate::updates::Pending>, last_sync: u64, last_error: String },
    Ticked { installed: Vec<String>, waiting: usize },
    Ok,
    Error { message: String },
}
