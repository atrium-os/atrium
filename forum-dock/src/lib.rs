//! forum-dock — the app catalog (`docs/spec/forum.md` §6).
//!
//! The dock lists the installed apps and, on the human's activate, asks the TCB to
//! launch one (the launch *client* lives in the binary; this is the catalog logic).
//! The dock holds `app-launch`: it ASKS portcullisd for the catalog and for a
//! launch. A bug here still can't launch anything the user didn't approve (the
//! daemon's policy gate is unchanged), can't see any app the daemon declines to
//! list, and can't touch another app's windows (it has no `window-management`).

use std::path::Path;

pub mod apps;
pub mod apps_view;
pub mod bundle_icon;
pub mod dock_view;
pub mod icons;
pub mod launch;
mod phosphor_names;
pub mod pins;

/// The conventional install root: one subdirectory per app, each with its
/// `atrium.toml` manifest (mirrors portcullisd's launch tree).
pub const APPS_DIR: &str = "/var/lib/atrium/apps";

/// One launchable app, as shown in the dock.
#[derive(Debug, Clone, PartialEq)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Icon name/path the app declared in its manifest (`[app] icon = ...`).
    pub icon: Option<String>,
    /// The uid it runs as for this person, from portcullisd (`None` until it
    /// first ran, or when the catalog came off the filesystem): how the dock
    /// matches windows to apps (forum.md §6.1).
    pub uid: Option<u32>,
    /// The app's own icon from its bundle, parsed (`bundle_icon`); `None`
    /// when it has none or it was refused.
    pub vector: Option<std::sync::Arc<bundle_icon::VectorIcon>>,
    /// It may have a console (homes.md §4): its tile offers Terminal.
    pub console: bool,
}

/// A workspace's entry id (homes.md §5): `workspace/<name>`, the console
/// target portcullisd knows it by. Its tile opens its terminal.
pub const WORKSPACE: &str = "workspace/";
/// The "New workspace" tile the filter offers for a name not in use.
pub const NEW_WORKSPACE: &str = "workspace+new/";

pub fn is_workspace(id: &str) -> bool { id.starts_with(WORKSPACE) || id.starts_with(NEW_WORKSPACE) }

/// portcullisd's rule for a workspace name (up to 32 lowercase letters,
/// digits and dashes, not starting with a dash).
pub fn valid_workspace_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A workspace, as a tile.
pub fn workspace_entry(w: &portcullis_ipc::WorkspaceInfo) -> AppEntry {
    let tool = match w.toolchain.as_str() { "c" => "C", "shell" => "shell", t => t };
    let net = if w.network == "none" { "no network" } else { "outbound network" };
    let uses = if w.uses.is_empty() { String::new() } else { format!(", uses {}", w.uses.join(", ")) };
    AppEntry {
        id: format!("{WORKSPACE}{}", w.name),
        name: if w.name == "scratch" { "Scratch".into() } else { w.name.clone() },
        description: Some(format!("Workspace: {tool}, {net}{uses}")),
        icon: Some("code".into()),
        uid: None,
        vector: None,
        console: false,
    }
}

/// The "New workspace" tile for what was typed, if it can name one and no
/// workspace has that name.
pub fn new_workspace_entry(apps: &[AppEntry], query: &str) -> Option<AppEntry> {
    let q = query.trim();
    if !valid_workspace_name(q) || apps.iter().any(|a| a.id == format!("{WORKSPACE}{q}")) {
        return None;
    }
    Some(AppEntry {
        id: format!("{NEW_WORKSPACE}{q}"),
        name: format!("New workspace \u{201c}{q}\u{201d}"),
        description: Some("A new workspace (C and the shell), with outbound network, and its terminal".into()),
        icon: Some("plus".into()),
        uid: None,
        vector: None,
        console: false,
    })
}

/// Parse a bundle icon's SVG, saying why one is not drawn.
pub fn vector_icon(id: &str, svg: Option<&str>) -> Option<std::sync::Arc<bundle_icon::VectorIcon>> {
    match bundle_icon::parse(svg?) {
        Ok(v) => Some(std::sync::Arc::new(v)),
        Err(why) => {
            eprintln!("forum-dock: {id}: its icon is not drawn ({why})");
            None
        }
    }
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
        uid: None,
        vector: None,
        console: false,
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
                    vector: vector_icon(&a.id, a.icon_svg.as_deref()),
                    id: a.id,
                    name: a.name,
                    description: a.description,
                    icon: a.icon,
                    uid: a.uid,
                    console: a.console,
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
            if let Some(mut entry) = entry_from_manifest(&text) {
                let svg = entry.icon.as_deref().and_then(|i| portcullis_toml::icon::read_bundle_svg(&e.path(), i));
                entry.vector = vector_icon(&entry.id, svg.as_deref());
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

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn new_workspace_tile_only_for_a_free_valid_name() {
        let w = portcullis_ipc::WorkspaceInfo { name: "scratch".into(), toolchain: "c".into(), persona: "personal".into(),
                                                network: "outbound".into(), uses: vec![], running: false };
        let apps = vec![workspace_entry(&w)];
        assert_eq!(apps[0].id, "workspace/scratch");
        assert_eq!(apps[0].name, "Scratch");
        assert!(new_workspace_entry(&apps, "scratch").is_none());
        assert!(new_workspace_entry(&apps, "Has Caps").is_none());
        assert!(new_workspace_entry(&apps, "").is_none());
        assert_eq!(new_workspace_entry(&apps, "lib-a").unwrap().id, "workspace+new/lib-a");
        assert!(is_workspace("workspace/x") && is_workspace("workspace+new/x") && !is_workspace("org.x"));
    }
}
