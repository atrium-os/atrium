//! Opening links in apps (docs/spec/opening-links.md §5): the grants
//! portcullisd gives Monstrator, one per press, and the one look it takes
//! at a link (its scheme, its length, no control bytes). Monstrator does
//! the parsing in its jail; portcullisd never does (§2).

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The link opener's system user: the only caller of `LinkGrant` and
/// `DeliverLink`.
pub const MONSTRATOR_USER: &str = "_monstrator";
/// How long a grant waits for the person to pick an app.
pub const GRANT_FOR: Duration = Duration::from_secs(120);
/// Grants outstanding at once, at most.
pub const MAX_GRANTS: usize = 64;
/// Grants per asking app per minute, at most (§5: ten a minute).
pub const PER_MINUTE: usize = 10;

/// One press's worth of link: for that person, from that app, of that
/// scheme, to one of `apps`.
#[derive(Debug, Clone)]
pub struct Grant {
    pub at: Instant,
    pub person: String,
    pub asker_app: String,
    pub scheme: String,
    pub apps: Vec<String>,
}

#[derive(Default)]
pub struct Grants {
    live: Mutex<HashMap<[u8; 16], Grant>>,
    /// Each asking app's recent grants, for the rate.
    recent: Mutex<HashMap<String, Vec<Instant>>>,
}

fn token() -> [u8; 16] {
    let mut t = [0u8; 16];
    // A grant is only as good as its secrecy: no fallback.
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut t)).expect("/dev/urandom");
    t
}

impl Grants {
    /// portcullisd's one table.
    pub fn global() -> &'static Grants {
        static G: OnceLock<Grants> = OnceLock::new();
        G.get_or_init(Grants::default)
    }

    /// Whether `asker_app` may have another grant now (checked before the
    /// press is spent, so a refused one doesn't burn it).
    pub fn may_ask(&self, asker_app: &str, now: Instant) -> Result<(), String> {
        let mut r = self.recent.lock().unwrap();
        let v = r.entry(asker_app.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if v.len() >= PER_MINUTE {
            return Err(format!("{asker_app} opened {PER_MINUTE} links in a minute; wait a little"));
        }
        Ok(())
    }

    /// A grant for `g`.
    pub fn issue(&self, g: Grant) -> Result<[u8; 16], String> {
        let mut live = self.live.lock().unwrap();
        live.retain(|_, x| g.at.duration_since(x.at) < GRANT_FOR);
        if live.len() >= MAX_GRANTS {
            return Err("too many links waiting to be opened".into());
        }
        self.recent.lock().unwrap().entry(g.asker_app.clone()).or_default().push(g.at);
        let t = token();
        live.insert(t, g);
        Ok(t)
    }

    /// Spend `grant` on `url` to `app`: once, in time, one of its apps, its
    /// scheme. A refusal for a wrong app or link keeps it (the sheet may
    /// try again); an expired one goes.
    pub fn take(&self, grant: &[u8; 16], app: &str, url: &str, now: Instant) -> Result<Grant, String> {
        let mut live = self.live.lock().unwrap();
        let Some(g) = live.get(grant) else { return Err("no such grant (used, or never given)".into()) };
        if now.duration_since(g.at) >= GRANT_FOR {
            live.remove(grant);
            return Err("the link waited too long; open it again".into());
        }
        if !g.apps.iter().any(|a| a == app) {
            return Err(format!("{app} wasn't one of the apps for this link"));
        }
        url_ok(url, &g.scheme)?;
        Ok(live.remove(grant).unwrap())
    }
}

/// The look portcullisd takes at a link: bounded, printable ASCII only
/// (Monstrator hands on the parser's encoded form), and of `scheme`.
pub fn url_ok(url: &str, scheme: &str) -> Result<(), String> {
    if url.is_empty() || url.len() > atrium_links::MAX_URL {
        return Err("a link of no or too great length".into());
    }
    if url.bytes().any(|b| !(0x21..0x7f).contains(&b)) {
        return Err("a link with spaces, control or non-ASCII bytes".into());
    }
    if !atrium_links::starts_with_scheme(url, scheme) {
        return Err(format!("not a {scheme}: link"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(at: Instant) -> Grant {
        Grant { at, person: "ana".into(), asker_app: "org.atrium.navigator".into(), scheme: "mailto".into(), apps: vec!["org.example.mail".into()] }
    }

    #[test]
    fn a_grant_opens_one_link_to_one_of_its_apps() {
        let t = Grants::default();
        let now = Instant::now();
        let k = t.issue(g(now)).unwrap();
        assert!(t.take(&k, "org.example.other", "mailto:a@b", now).is_err(), "not its app");
        assert!(t.take(&k, "org.example.mail", "https://a/", now).is_err(), "not its scheme");
        assert!(t.take(&k, "org.example.mail", "mailto:a@b\nbcc:c", now).is_err(), "control bytes");
        let got = t.take(&k, "org.example.mail", "mailto:a@b", now).unwrap();
        assert_eq!(got.person, "ana");
        assert!(t.take(&k, "org.example.mail", "mailto:a@b", now).is_err(), "once");
        let k = t.issue(g(now)).unwrap();
        assert!(t.take(&k, "org.example.mail", "mailto:a@b", now + GRANT_FOR).unwrap_err().contains("too long"));
    }

    #[test]
    fn ten_a_minute() {
        let t = Grants::default();
        let now = Instant::now();
        for _ in 0..PER_MINUTE {
            t.may_ask("org.atrium.navigator", now).unwrap();
            t.issue(g(now)).unwrap();
        }
        assert!(t.may_ask("org.atrium.navigator", now).is_err());
        assert!(t.may_ask("org.example.other", now).is_ok(), "per app");
        assert!(t.may_ask("org.atrium.navigator", now + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn the_look_at_a_link() {
        assert!(url_ok("MAILTO:a@b", "mailto").is_ok());
        assert!(url_ok("mailto:a b", "mailto").is_err());
        assert!(url_ok("mailto:é", "mailto").is_err());
        assert!(url_ok(&format!("mailto:{}", "a".repeat(atrium_links::MAX_URL)), "mailto").is_err());
    }
}
