//! Settings' Apps page (curia.md §13): what each installed app asks for,
//! who allowed it, and the checks before Settings may allow one. Pure: the
//! daemon reads the manifests and policies and does the writing.

use std::io;

use portcullis_ipc::{AccessBy, AccessEntry};
use portcullis_policy::{compute_delta, decide, GrantSource, Policy};
use portcullis_toml::Manifest;

/// The page's row for `m` (installed, its text hashing to `hash`), for
/// the person whose policy is `user`.
pub fn entry(m: &Manifest, hash: &str, user: &Policy, system: Result<&Policy, &io::Error>) -> AccessEntry {
    let id = m.app.id.clone();
    let name = if m.app.name.is_empty() { id.clone() } else { m.app.name.clone() };
    // What it opens is part of what it asks (opening-files.md §5.1).
    let mut asks = compute_delta(&m.capabilities, None, None, hash).describe();
    asks.extend(m.opens_lines());
    asks.extend(m.links_line());
    let d = decide(&id, &m.capabilities, hash, user, system);
    let missing = if d.delta.is_empty() {
        Vec::new()
    } else {
        let mut v = d.delta.describe();
        v.extend(m.opens_lines());
        v.extend(m.links_line());
        v
    };
    let by = match (d.delta.is_empty(), d.by) {
        (true, Some(GrantSource::System)) => Some(AccessBy::System),
        (true, Some(GrantSource::User)) => Some(AccessBy::You),
        _ => None,
    };
    AccessEntry {
        yours: user.grants.get(&id).map(|g| g.granted_at.clone()),
        restricted: m.capabilities.restricted().into_iter().map(String::from).collect(),
        id,
        name,
        by,
        asks,
        missing,
    }
}

/// Why Settings may not allow `e`'s app, or `None` when it may.
pub fn may_approve(e: &AccessEntry) -> Option<String> {
    if !e.restricted.is_empty() {
        return Some(format!("{} asks for what only the system can allow ({})", e.name, e.restricted.join(", ")));
    }
    if e.missing.is_empty() {
        return Some(format!("{} already has what it asks for", e.name));
    }
    None
}

/// Why the person's grant for `e`'s app can't be removed, or `None`.
pub fn may_remove(e: &AccessEntry) -> Option<String> {
    if e.yours.is_none() {
        return Some(match e.by {
            Some(AccessBy::System) => format!("{} is allowed by the system, not by you", e.name),
            _ => format!("you haven't allowed {} anything", e.name),
        });
    }
    None
}

/// The rows by name, then id.
pub fn sort(v: &mut [AccessEntry]) {
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_policy::{hash_manifest, Grant};

    const PLAYER: &str = r#"
[app]
id = "org.atrium.player"
name = "Player"
version = "1.0"
entry = "bin/player"

[capabilities]
graphics = "fresco"
audio = true
"#;

    fn m(t: &str) -> Manifest {
        Manifest::from_str(t).unwrap()
    }

    #[test]
    fn a_new_app_asks_and_settings_may_allow_it() {
        let man = m(PLAYER);
        let h = hash_manifest(PLAYER.as_bytes());
        let none = Policy::default();
        let e = entry(&man, &h, &none, Ok(&none));
        assert_eq!(e.by, None);
        assert!(e.asks.iter().any(|l| l == "Play audio"), "{:?}", e.asks);
        assert_eq!(e.asks, e.missing);
        assert!(e.yours.is_none() && e.restricted.is_empty());
        assert_eq!(may_approve(&e), None);
        assert!(may_remove(&e).unwrap().contains("haven't allowed"));
    }

    #[test]
    fn allowed_by_you_then_changed_by_an_update() {
        let man = m(PLAYER);
        let h = hash_manifest(PLAYER.as_bytes());
        let mut you = Policy::default();
        you.grants.insert("org.atrium.player".into(), Grant { manifest_hash: h.clone(), granted_at: "2026-11-15T10:00:00Z".into(), capabilities: man.capabilities.clone() });
        let e = entry(&man, &h, &you, Ok(&Policy::default()));
        assert_eq!(e.by, Some(AccessBy::You));
        assert!(e.missing.is_empty());
        assert_eq!(may_remove(&e), None);
        assert!(may_approve(&e).is_some(), "nothing to allow");
        // An update asks again; the old grant is still the person's to remove.
        let newer = PLAYER.replace("audio = true", "audio = true\nmicrophone = true");
        let e = entry(&m(&newer), &hash_manifest(newer.as_bytes()), &you, Ok(&Policy::default()));
        assert_eq!(e.by, None);
        assert_eq!(e.missing[0], "Manifest has changed since last grant");
        assert!(e.missing.iter().any(|l| l.contains("microphone")));
        assert_eq!(may_approve(&e), None);
        assert_eq!(may_remove(&e), None);
    }

    #[test]
    fn restricted_and_system_apps_are_not_the_persons() {
        let t = PLAYER.replace("audio = true", "audio-control = true\napp-grants = true");
        let man = m(&t);
        let h = hash_manifest(t.as_bytes());
        let mut sys = Policy::default();
        sys.grants.insert("org.atrium.player".into(), Grant { manifest_hash: h.clone(), granted_at: "x".into(), capabilities: man.capabilities.clone() });
        let e = entry(&man, &h, &Policy::default(), Ok(&sys));
        assert_eq!(e.by, Some(AccessBy::System));
        assert_eq!(e.restricted, vec!["audio-control", "app-grants"]);
        assert!(may_approve(&e).unwrap().contains("only the system"));
        assert!(may_remove(&e).unwrap().contains("by the system"));
        // Not installed by the system: still not the person's to allow.
        let e = entry(&man, &h, &Policy::default(), Ok(&Policy::default()));
        assert!(may_approve(&e).unwrap().contains("only the system"));
    }

    #[test]
    fn rows_sort_by_name() {
        let none = Policy::default();
        let a = entry(&m(PLAYER), "h", &none, Ok(&none));
        let mut b = a.clone();
        b.name = "atlas".into();
        let mut v = vec![a, b];
        sort(&mut v);
        assert_eq!(v[0].name, "atlas");
    }
}
