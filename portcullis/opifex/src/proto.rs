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
    /// What the catalog says about an id (no network). `for_uid`: from the
    /// sheet or root, describe it as that person sees the catalogs (their
    /// own added catalogs too); ignored from anyone else.
    Describe { id: String, #[serde(default)] for_uid: Option<u32> },
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
    /// The catalogs this person sees (C7).
    Catalogs,
    /// What adding the catalog at `url` would add: its name and its
    /// `root.toml`'s hash, for the sheet to show before the press. Anyone.
    PreviewCatalog { url: String },
    /// Follow the catalog at `url`, whose `root.toml` hashes to
    /// `root_sha256` (what the person pressed for). From the sheet for the
    /// person `for_uid`; `machine` (everyone's) only from an administrator.
    AddCatalog { url: String, root_sha256: String, machine: bool, for_uid: u32 },
    /// Stop following a catalog: an administrator, or the person who added it.
    RemoveCatalog { slug: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// The publisher's name and the domains it proved (C7; empty for the
    /// Atrium catalog's platform key).
    #[serde(default)]
    pub publisher_name: String,
    #[serde(default)]
    pub publisher_domains: Vec<String>,
    /// From a catalog someone added, not the Atrium catalog.
    #[serde(default)]
    pub third_party: bool,
    /// Its publisher's key was replaced by a recovery since it was installed.
    #[serde(default)]
    pub recovered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Response {
    Installed { apps: Vec<crate::installed::Record> },
    Described { app: Described },
    Found { ids: Vec<String> },
    Done { id: String, version: String },
    Synced { entries: u64, releases: usize, #[serde(default)] others: Vec<String> },
    Catalogs { items: Vec<crate::catalogs::Listed> },
    CatalogPreview { url: String, name: String, root_sha256: String, warning: String },
    CatalogAdded { slug: String, name: String, summary: String },
    Updates { items: Vec<crate::updates::Pending>, last_sync: u64, last_error: String },
    Ticked { installed: Vec<String>, waiting: usize },
    Ok,
    Error { message: String },
}

/// Ask opifexd (`OPIFEX_SOCKET`, else [`SOCKET`]) one thing.
pub fn ask(req: &Request) -> Result<Response, String> {
    use std::io::{BufRead, Write};
    let sock = std::env::var("OPIFEX_SOCKET").unwrap_or_else(|_| SOCKET.into());
    let mut s = std::os::unix::net::UnixStream::connect(&sock).map_err(|e| format!("opifexd ({sock}): {e}"))?;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    s.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    std::io::BufReader::new(s).read_line(&mut out).map_err(|e| e.to_string())?;
    serde_json::from_str::<Response>(out.trim()).map_err(|e| format!("opifexd: {e}"))
}
