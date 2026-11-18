//! What installed manifests say about roles (app-composition.md §5.1,
//! dev-tools-plan.md M6): which roles an app may embed (`[capabilities]
//! embeds`) and which apps provide a role (`[roles] implements`).
//!
//! Read from the installed tree (`/var/lib/atrium/apps/<id>/atrium.toml`),
//! which portcullisd verified at install; never from anything the app sends.

use std::path::{Path, PathBuf};

pub const APPS_DIR: &str = "/var/lib/atrium/apps";

/// The platform's own id prefix: a third-party publisher is held to its
/// verified prefix (opifex.md §6.5), so only platform bundles are here.
pub const PLATFORM_PREFIX: &str = "org.atrium.";

/// An app id that can name a folder in the app tree: not empty, no
/// slash, no NUL, not hidden, at most 255 bytes.
pub fn plausible_id(app: &str) -> bool {
    !app.is_empty() && app.len() <= 255 && !app.contains(['/', '\0']) && !app.starts_with('.')
}

fn manifest(dir: &Path, app: &str) -> Option<portcullis_toml::Manifest> {
    if !plausible_id(app) {
        return None;
    }
    let text = std::fs::read_to_string(dir.join(app).join("atrium.toml")).ok()?;
    portcullis_toml::Manifest::from_str(&text).ok()
}

/// A `share-target` the share sheet can offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub app: String,
    /// `[app] name`.
    pub name: String,
    /// Which of the item's types it takes, in the item's order.
    pub takes: Vec<String>,
    /// `[app] icon`: a system icon name, or a path to an SVG in the bundle.
    pub icon: Option<String>,
    /// That SVG's text, read safely from the bundle
    /// (`portcullis_toml::icon::read_bundle_svg`); the drawer still checks it.
    pub icon_svg: Option<String>,
}

/// Installed manifests under one folder.
#[derive(Clone, Debug)]
pub struct Installed {
    pub dir: PathBuf,
}

impl Default for Installed {
    fn default() -> Self { Installed { dir: APPS_DIR.into() } }
}

impl Installed {
    pub fn at(dir: impl Into<PathBuf>) -> Self { Installed { dir: dir.into() } }

    /// Whether `app` may embed `role`. Its manifest's `embeds` decides
    /// when it has one. A platform bundle without one keeps what it had
    /// before `embeds` existed (every role); any other app (a third
    /// party's, a dev build) must name the role.
    pub fn may_embed(&self, app: &str, role: &str) -> bool {
        let Some(m) = manifest(&self.dir, app) else { return false };
        match &m.capabilities.embeds {
            Some(roles) => roles.iter().any(|r| r == role),
            None => app.starts_with(PLATFORM_PREFIX),
        }
    }

    /// Whether `app`'s manifest implements `role`.
    pub fn implements(&self, app: &str, role: &str) -> bool {
        manifest(&self.dir, app)
            .and_then(|m| m.roles)
            .is_some_and(|r| r.implements.iter().any(|x| x == role))
    }

    /// Whether `app` is a `share-target` the share sheet may offer `person`:
    /// its installed manifest implements the role, and a dev build only
    /// for the person whose workspace made it.
    pub fn may_target(&self, person: &str, app: &str) -> bool {
        (!app.starts_with("dev.") || app.starts_with(&format!("dev.{person}."))) && self.implements(app, "share-target")
    }

    /// The `share-target`s for `person` that take one of `mimes`, by app id,
    /// with the types each declared (`[roles.share-target] mime`; none:
    /// anything) and its name from the manifest.
    pub fn share_targets(&self, person: &str, mimes: &[String]) -> Vec<Target> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut ids: Vec<String> = rd.flatten().filter_map(|e| e.file_name().to_str().map(String::from)).collect();
        ids.sort();
        ids.into_iter()
            .filter(|id| self.may_target(person, id))
            .filter_map(|id| {
                let m = manifest(&self.dir, &id)?;
                let accepts = m.roles.as_ref().and_then(|r| r.share_target.as_ref()).map(|s| s.mime.clone()).unwrap_or_default();
                let takes: Vec<String> = mimes.iter().filter(|t| crate::share_target::accepts(&accepts, t)).cloned().collect();
                if takes.is_empty() {
                    return None;
                }
                let icon_svg = m.app.icon.as_deref().and_then(|i| portcullis_toml::icon::read_bundle_svg(&self.dir.join(&id), i));
                Some(Target { app: id, name: m.app.name.clone(), takes, icon: m.app.icon.clone(), icon_svg })
            })
            .collect()
    }

    /// The provider of `role` for `person` when no default is set (until
    /// Curia holds the person's choice, limen.md §3.2): a platform bundle
    /// that implements it, else the one other app that does. A dev build
    /// counts only for the person whose workspace made it. Two or more
    /// third-party providers and no platform one: none, so nothing is
    /// chosen for the person behind their back.
    pub fn provider(&self, person: &str, role: &str) -> Option<String> {
        let mut ids: Vec<String> = std::fs::read_dir(&self.dir).ok()?
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|id| !id.starts_with('.'))
            .collect();
        ids.sort();
        let mine = format!("dev.{person}.");
        let ok = |id: &String| !id.starts_with("dev.") || id.starts_with(&mine);
        let found: Vec<String> = ids.into_iter().filter(|id| ok(id) && self.implements(id, role)).collect();
        if let Some(p) = found.iter().find(|id| id.starts_with(PLATFORM_PREFIX)) {
            return Some(p.clone());
        }
        (found.len() == 1).then(|| found[0].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(dir: &Path, id: &str, extra: &str) {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        std::fs::write(dir.join(id).join("atrium.toml"),
            format!("[app]\nid = \"{id}\"\nname = \"X\"\nversion = \"1\"\nentry = \"bin/x\"\n{extra}")).unwrap();
    }

    #[test]
    fn embeds_and_providers() {
        let d = std::env::temp_dir().join(format!("limen-manifests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        app(&d, "org.atrium.mail", "");
        app(&d, "com.example.viewer", "\n[capabilities]\nembeds = [\"doc-viewer\"]\n");
        app(&d, "com.example.bare", "");
        app(&d, "dev.alice.scratch.org.example.share", "\n[roles]\nimplements = [\"share-target\"]\n");
        app(&d, "com.example.share", "\n[roles]\nimplements = [\"share-target\", \"map\"]\n");
        app(&d, "org.atrium.maps", "\n[roles]\nimplements = [\"map\"]\n");
        let i = Installed::at(&d);
        assert!(i.may_embed("org.atrium.mail", "picker"));
        assert!(i.may_embed("com.example.viewer", "doc-viewer"));
        assert!(!i.may_embed("com.example.viewer", "picker"));
        assert!(!i.may_embed("com.example.bare", "doc-viewer"));
        assert!(!i.may_embed("not.installed", "doc-viewer"));
        assert!(!i.may_embed("../etc", "doc-viewer"));
        // A platform provider first.
        assert_eq!(i.provider("alice", "map").as_deref(), Some("org.atrium.maps"));
        // Two third-party ones for alice (hers and an installed one): none.
        assert_eq!(i.provider("alice", "share-target"), None);
        // Bob doesn't see alice's dev build: the one installed provider.
        assert_eq!(i.provider("bob", "share-target").as_deref(), Some("com.example.share"));
        assert_eq!(i.provider("bob", "payment"), None);
        // Share targets: alice's own dev build and the installed one; bob
        // only the installed one; a declared type list narrows.
        assert!(i.may_target("alice", "dev.alice.scratch.org.example.share") && !i.may_target("bob", "dev.alice.scratch.org.example.share"));
        assert!(!i.may_target("bob", "org.atrium.maps") && !i.may_target("bob", "../x"));
        app(&d, "com.example.photos", "icon = \"icons/app.svg\"\n\n[roles]\nimplements = [\"share-target\"]\n\n[roles.share-target]\nmime = [\"image/*\"]\n");
        std::fs::create_dir_all(d.join("com.example.photos/icons")).unwrap();
        std::fs::write(d.join("com.example.photos/icons/app.svg"), "<svg/>").unwrap();
        let pdf = vec!["application/pdf".to_string()];
        let png = vec!["image/png".to_string(), "text/plain".to_string()];
        let ids = |v: Vec<Target>| v.into_iter().map(|t| t.app).collect::<Vec<_>>();
        assert_eq!(ids(i.share_targets("bob", &pdf)), ["com.example.share"]);
        assert_eq!(ids(i.share_targets("bob", &png)), ["com.example.photos", "com.example.share"]);
        assert_eq!(i.share_targets("bob", &png)[0].takes, ["image/png"]);
        // The icon: its own SVG from the bundle; none declared, none.
        let t = i.share_targets("bob", &png);
        assert_eq!((t[0].icon.as_deref(), t[0].icon_svg.as_deref()), (Some("icons/app.svg"), Some("<svg/>")));
        assert_eq!((t[1].icon.as_deref(), t[1].icon_svg.as_deref()), (None, None));
        assert_eq!(ids(i.share_targets("alice", &pdf)), ["com.example.share", "dev.alice.scratch.org.example.share"]);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
