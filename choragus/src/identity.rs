//! Verified app identity from Portcullis's launch registry.
//!
//! Portcullis launches each app in its own jail under a **dedicated uid** (from a
//! reserved app-uid range — *not* shared with the human or with other apps). A
//! dedicated uid is the right boundary twice over: it isolates the app (same-uid
//! processes could otherwise `ptrace`/signal/read each other), and it *is* the
//! app's unforgeable identity. At launch Portcullis records `uid → (owning user,
//! app-id)`; Choragus reads `getpeereid`'s uid and resolves it here. So the app's
//! identity is the kernel's answer via this binding — never the app's own word.
//! (This is what makes the §9 grant enforcement sound: an app cannot get another
//! app's capabilities by *claiming* its id, because the id comes from the uid.)

use std::collections::HashMap;

/// `uid → (owning user, app-id)` — the binding Portcullis writes when it launches
/// an app at a dedicated uid.
#[derive(Default)]
pub struct AppRegistry {
    map: HashMap<u32, (String, String)>,
}

impl AppRegistry {
    /// Parse the registry text. One app per line: `<uid> <user> <app-id>`;
    /// `#` comments and blanks ignored.
    pub fn parse(text: &str) -> AppRegistry {
        let mut map = HashMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(uid), Some(user), Some(app)) = (it.next(), it.next(), it.next()) {
                if let Ok(uid) = uid.parse::<u32>() {
                    map.insert(uid, (user.to_string(), app.to_string()));
                }
            }
        }
        AppRegistry { map }
    }

    pub fn load(path: &str) -> std::io::Result<AppRegistry> {
        Ok(AppRegistry::parse(&std::fs::read_to_string(path)?))
    }

    /// The verified `(owning user, app-id)` for a uid, if Portcullis launched an
    /// app there. `None` = not a Portcullis-launched app (→ default-deny).
    pub fn resolve(&self, uid: u32) -> Option<(&str, &str)> {
        self.map.get(&uid).map(|(u, a)| (u.as_str(), a.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = "
        # uid  user  app-id  (Portcullis writes this at launch)
        100001  alice  org.atrium.recorder
        100002  alice  org.atrium.player
    ";

    #[test]
    fn resolves_uid_to_verified_app() {
        let r = AppRegistry::parse(REG);
        assert_eq!(r.resolve(100001), Some(("alice", "org.atrium.recorder")));
        assert_eq!(r.resolve(100002), Some(("alice", "org.atrium.player")));
    }

    #[test]
    fn unregistered_uid_is_unknown() {
        let r = AppRegistry::parse(REG);
        assert_eq!(r.resolve(0), None, "a non-Portcullis-launched uid is unknown");
    }
}
