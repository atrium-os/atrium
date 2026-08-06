//! forum-dock — the app catalog (`docs/spec/forum.md` §6).
//!
//! The dock lists the installed apps and, on the human's activate, asks the TCB to
//! launch one (the launch *client* lives in the binary; this is the catalog logic).
//! The dock holds `app-launch`: it ASKS portcullisd for the catalog and for a
//! launch. A bug here still can't launch anything the user didn't approve (the
//! daemon's policy gate is unchanged), can't see any app the daemon declines to
//! list, and can't touch another app's windows (it has no `window-management`).

use std::path::Path;

/// The conventional install root: one subdirectory per app, each with its
/// `atrium.toml` manifest (mirrors portcullisd's launch tree).
pub const APPS_DIR: &str = "/var/lib/atrium/apps";

/// One launchable app, as shown in the dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Icon name/path the app declared in its manifest (`[app] icon = ...`).
    pub icon: Option<String>,
}

/// Parse one manifest's text into a catalog entry. `None` if it doesn't parse —
/// an unreadable/invalid manifest is simply not offered (fail safe, not loud).
pub fn entry_from_manifest(text: &str) -> Option<AppEntry> {
    let m = portcullis_toml::Manifest::from_str(text).ok()?;
    Some(AppEntry {
        id: m.app.id,
        name: m.app.name,
        description: m.app.description,
        icon: m.app.icon,
    })
}

/// Where the catalog came from — worth reporting, because "no apps" and "I could
/// not see any apps" look identical on screen and mean very different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    /// portcullisd answered over the `app-launch` socket. The in-jail path.
    Daemon,
    /// Read straight off the app tree — only possible unjailed (dev / CLI).
    Filesystem,
    /// Neither worked: no daemon socket reachable AND no readable app tree.
    Unavailable,
}

/// The installed-app catalog.
///
/// Asks portcullisd first and only falls back to reading the app tree. That
/// order is the fix for a jailed dock: `/var/lib/atrium/apps` is deliberately
/// NOT mounted into an app jail, so the filesystem scan there silently returns
/// nothing — and the dock used to quietly draw placeholder apps on top of that
/// emptiness. The daemon path needs no filesystem exposure at all; it needs the
/// `app-launch` capability, which is also what the launch request needs.
///
/// Returns the source alongside the entries so the caller can tell "nothing is
/// installed" from "I am blind", instead of rendering both as the same dock.
pub fn catalog_resolved(apps_dir: &Path) -> (Vec<AppEntry>, CatalogSource) {
    match catalog_from_daemon() {
        Some(apps) => (apps, CatalogSource::Daemon),
        None => {
            if apps_dir.is_dir() {
                (catalog(apps_dir), CatalogSource::Filesystem)
            } else {
                (Vec::new(), CatalogSource::Unavailable)
            }
        }
    }
}

/// Ask portcullisd for the catalog over the socket the `app-launch` capability
/// mounts. `None` if the daemon isn't reachable or refuses — the caller decides
/// what to do about it; this layer never invents entries.
pub fn catalog_from_daemon() -> Option<Vec<AppEntry>> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    use std::os::unix::net::UnixStream;

    let path = portcullis_ipc::resolve_socket_path()?;
    let mut s = UnixStream::connect(path).ok()?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }).ok()? {
        Response::Hello { version } if version == PROTO_VERSION => {}
        _ => return None,
    }
    match round_trip(&mut s, &Request::Catalog).ok()? {
        Response::CatalogList { apps } => Some(
            apps.into_iter()
                .map(|a| AppEntry {
                    id: a.id,
                    name: a.name,
                    description: a.description,
                    icon: a.icon,
                })
                .collect(),
        ),
        // A refusal is NOT an empty catalog. Returning None lets the caller say
        // so, rather than drawing an empty (or worse, fake) dock.
        _ => None,
    }
}

/// Scan an apps directory → the installed-app catalog, sorted by id for a stable
/// dock order. Missing dir → empty (no apps installed is not an error).
pub fn catalog(apps_dir: &Path) -> Vec<AppEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(apps_dir) else { return out };
    for e in entries.flatten() {
        let manifest = e.path().join("atrium.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            if let Some(entry) = entry_from_manifest(&text) {
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[app]
id = "org.atrium.edit"
name = "Atrium Edit"
version = "1.0.0"
entry = "/usr/local/bin/atrium-edit"
description = "A text editor"

[capabilities]
graphics = "fresco"
"#;

    #[test]
    fn parses_a_manifest_into_a_catalog_entry() {
        let e = entry_from_manifest(SAMPLE).expect("parse");
        assert_eq!(e.id, "org.atrium.edit");
        assert_eq!(e.name, "Atrium Edit");
        assert_eq!(e.description.as_deref(), Some("A text editor"));
    }

    #[test]
    fn invalid_manifest_is_skipped_not_fatal() {
        assert!(entry_from_manifest("this is not toml = = =").is_none());
        assert!(entry_from_manifest("").is_none());
    }

    #[test]
    fn missing_apps_dir_yields_empty_catalog() {
        assert!(catalog(Path::new("/no/such/atrium/apps")).is_empty());
    }
}
