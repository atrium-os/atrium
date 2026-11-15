//! Who may use the control socket on Atrium (bluetooth.md §13): an app the
//! launcher started (its uid is in the launch registry, `uid user app-id`
//! per line) whose installed manifest (`APPS_DIR/<app-id>/atrium.toml`)
//! holds `bluetooth-control = true` under `[capabilities]`. Both files are
//! the system's; they are read on each connection, as apps come and go. No
//! Atrium crate is used, so vinculumd still builds on stock FreeBSD, where
//! `--control-uid` is the rule.

/// The app id the registry gives `uid`, if it names an app (not a session
/// component, not a void line).
pub fn app_of(registry: &str, uid: u32) -> Option<String> {
    let mut found = None;
    for line in registry.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f.len() > 4 || f[0].parse::<u32>().ok() != Some(uid) {
            continue;
        }
        // A fourth field is a publisher key (64 hex digits) or `session`.
        let app = match f.get(3) {
            None => Some(f[2]),
            Some(&"session") => None,
            Some(p) if p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()) => Some(f[2]),
            Some(_) => None,
        };
        // A later line for the same uid replaces an earlier one.
        found = app.map(str::to_string);
    }
    found.filter(|a| !a.is_empty() && !a.contains('/') && !a.starts_with('.'))
}

/// Does this manifest text hold `bluetooth-control = true` in its
/// `[capabilities]` table? Written the way manifests are: one `key = value`
/// per line; anything else (a dotted or quoted key, an inline table) does
/// not count.
pub fn manifest_allows(text: &str) -> bool {
    let mut in_caps = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_caps = line == "[capabilities]";
            continue;
        }
        if !in_caps {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "bluetooth-control" {
                return v.trim() == "true";
            }
        }
    }
    false
}

/// Whether `uid` may use the control socket, by the registry and the
/// installed manifests.
pub fn admits(registry_path: &str, apps_dir: &str, uid: u32) -> bool {
    let Ok(reg) = std::fs::read_to_string(registry_path) else { return false };
    let Some(app) = app_of(&reg, uid) else { return false };
    let path = std::path::Path::new(apps_dir).join(&app).join("atrium.toml");
    std::fs::read_to_string(path).is_ok_and(|t| manifest_allows(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = "\
# uid user app
50004 alice org.atrium.curia
50005 alice org.example.game
50006 alice ostiarius-ui session
50007 alice org.example.bad nothex
50008 bob org.atrium.curia 0000000000000000000000000000000000000000000000000000000000000000
50005 alice org.example.other
";

    #[test]
    fn the_registry_names_apps_only() {
        assert_eq!(app_of(REG, 50004).as_deref(), Some("org.atrium.curia"));
        assert_eq!(app_of(REG, 50008).as_deref(), Some("org.atrium.curia"));
        assert_eq!(app_of(REG, 50005).as_deref(), Some("org.example.other"), "the later line wins");
        assert_eq!(app_of(REG, 50006), None, "a session component");
        assert_eq!(app_of(REG, 50007), None, "a void line");
        assert_eq!(app_of(REG, 1), None);
        assert_eq!(app_of("50009 alice ../etc", 50009), None);
    }

    #[test]
    fn only_the_capability_in_its_table_counts() {
        let ok = "[app]\nid = \"org.atrium.curia\"\n\n[capabilities]\ngraphics = \"fresco\"\nbluetooth-control = true   # pair\n";
        assert!(manifest_allows(ok));
        assert!(!manifest_allows(&ok.replace("= true", "= false")));
        assert!(!manifest_allows("[app]\nbluetooth-control = true\n[capabilities]\naudio = true\n"), "not under [capabilities]");
        assert!(!manifest_allows("[capabilities]\n# bluetooth-control = true\n"));
        assert!(!manifest_allows("[capabilities]\naudio = true\n[supervision]\nbluetooth-control = true\n"));
    }

    #[test]
    fn admits_reads_both_files() {
        let d = std::env::temp_dir().join(format!("vinculum-apps-{}", std::process::id()));
        std::fs::create_dir_all(d.join("apps/org.atrium.curia")).unwrap();
        std::fs::create_dir_all(d.join("apps/org.example.game")).unwrap();
        std::fs::write(d.join("apps/org.atrium.curia/atrium.toml"), "[capabilities]\nbluetooth-control = true\n").unwrap();
        std::fs::write(d.join("apps/org.example.game/atrium.toml"), "[capabilities]\naudio = true\n").unwrap();
        std::fs::write(d.join("registry"), "50004 alice org.atrium.curia\n50005 alice org.example.game\n").unwrap();
        let (r, a) = (d.join("registry"), d.join("apps"));
        let (r, a) = (r.to_str().unwrap(), a.to_str().unwrap());
        assert!(admits(r, a, 50004));
        assert!(!admits(r, a, 50005));
        assert!(!admits(r, a, 50006));
        assert!(!admits("/nonexistent", a, 50004));
        let _ = std::fs::remove_dir_all(&d);
    }
}
