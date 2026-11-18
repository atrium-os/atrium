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

fn manifest(dir: &Path, app: &str) -> Option<portcullis_toml::Manifest> {
    if app.is_empty() || app.contains(['/', '\0']) || app.starts_with('.') {
        return None;
    }
    let text = std::fs::read_to_string(dir.join(app).join("atrium.toml")).ok()?;
    portcullis_toml::Manifest::from_str(&text).ok()
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
        std::fs::remove_dir_all(&d).unwrap();
    }
}
