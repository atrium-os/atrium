//! The stand-in trail store and sites for build step 1 (navigator-ui.md
//! §14): made-up trails and pages, the facts the broker would report for
//! them, and the links the placeholder page offers so the trail model can
//! be walked before a real page is bound (step 2). Nothing here is fetched
//! and nothing is real; every address is under `.example`.

use crate::trail::{Facts, Kept, Light, Media, PageSpec, Prompt, PutAway, Tier, Trail, TrailStore};

/// A link on a stand-in page.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub url: String,
    pub title: String,
    /// A link to a file: following it asks in the prompt row instead of
    /// adding a page (§6.4).
    pub download: Option<(String, String)>,
}

fn link(url: &str, title: &str) -> Link {
    Link { url: url.into(), title: title.into(), download: None }
}

const LE: &str = "Private connection (TLS 1.3) · issued by Let's Encrypt, root ISRG Root X1, shipped with Atrium";

fn atlas(path: &str) -> String {
    format!("https://atlas.example/wiki/{path}")
}

/// The search provider's result page for `q` (the provider is a setting,
/// asked on first use, §5.3; this one is a stand-in).
pub fn search_url(q: &str) -> String {
    format!("https://search.example/?q={}", q.trim().replace(' ', "+"))
}

fn titled(url: &str) -> String {
    let path = url.split_once("://").map_or(url, |(_, r)| r);
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    last.replace(['_', '-'], " ")
}

/// What the broker would report for `url`, and the page's links.
pub fn lookup(url: &str) -> (PageSpec, Vec<Link>) {
    let plain = |title: &str| PageSpec { url: url.into(), title: title.into(), search: None, facts: Facts::green(LE), media: Media::None };
    let atlas_links = || vec![
        link(&atlas("Atrium_(architecture)"), "Atrium (architecture)"),
        link(&atlas("Impluvium"), "Impluvium"),
        link(&atlas("Compluvium_roofs"), "Compluvium roofs"),
        link(&atlas("Tablinum"), "Tablinum"),
        link(&atlas("Peristyle"), "Peristyle"),
    ];
    if let Some(q) = url.strip_prefix("https://search.example/?q=") {
        let q = q.replace('+', " ");
        let spec = PageSpec { search: Some(q.clone()), ..plain(&format!("Search: {q}")) };
        let cap: String = q.split(' ').map(|w| {
            let mut c = w.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
        }).collect::<Vec<_>>().join("_");
        return (spec, vec![
            link(&atlas(&cap), &format!("{} — atlas.example", q)),
            link(&atlas("Domus_(house)"), "Domus (house) — atlas.example"),
            link(&format!("https://blog.rust.example/{}", q.replace(' ', "-")), &format!("{q} — blog.rust.example")),
        ]);
    }
    match url {
        u if u == atlas("Domus_(house)") => (plain("Domus (house)"), atlas_links()),
        u if u == atlas("Atrium_(architecture)") => {
            let mut l = atlas_links();
            l.remove(0);
            l.push(Link { download: Some(("impluvium-survey.pdf".into(), "2.4 MB".into())), ..link("https://atlas.example/files/impluvium-survey.pdf", "Survey drawings (PDF)") });
            l.push(link("https://atlas.example/films/atrium-walkthrough", "Film: a walk through an atrium"));
            l.push(link("http://old-atlas.example/atrium", "The old atlas mirror (http)"));
            (plain("Atrium (architecture)"), l)
        }
        "https://atlas.example/films/atrium-walkthrough" => {
            let facts = Facts { tier: Tier::Replayed, live_parts: vec!["media.example".into()], ..Facts::green(LE) }.settle();
            (PageSpec { facts, media: Media::Silent, ..plain("A walk through an atrium") }, atlas_links())
        }
        "http://old-atlas.example/atrium" => {
            let facts = Facts {
                insecure: true,
                reasons: vec!["Not private: anyone on this network can read or change this page.".into()],
                connection: "Not private (http)".into(),
                ..Facts::green("")
            }.settle();
            (PageSpec { facts, ..plain("Atrium (old mirror)") }, vec![link(&atlas("Atrium_(architecture)"), "The current atlas")])
        }
        "https://mail.example/inbox" => {
            let facts = Facts { tier: Tier::Live, reasons: vec!["mail.example's JavaScript runs now, in a jail of its own.".into()], ..Facts::green(LE) }.settle();
            (PageSpec { facts, ..plain("Inbox (3)") }, vec![
                link("https://mail.example/m/survey-drawings", "Lena Ortiz — Survey drawings for the north wing"),
                link("https://mail.example/m/lime-delivery", "Builders' guild — Lime delivery moved to Tuesday"),
                link("https://n0rthbank.example/login", "Northbank — Your statement is ready"),
            ])
        }
        u if u.starts_with("https://mail.example/m/") => {
            let facts = Facts { tier: Tier::Live, ..Facts::green(LE) }.settle();
            (PageSpec { facts, ..plain(&titled(u)) }, vec![link("https://mail.example/inbox", "Back to the inbox")])
        }
        "https://n0rthbank.example/login" => {
            let facts = Facts {
                light: Light::Red,
                reasons: vec!["n0rthbank.example looks like northbank.example, where you have a login.".into()],
                ..Facts::green("Private connection (TLS 1.3) · issued by ZeroSSL, root USERTrust, shipped with Atrium")
            };
            (PageSpec { facts, ..plain("Sign in") }, vec![])
        }
        "https://radio.example/late-night" => {
            let facts = Facts { tier: Tier::Replayed, live_parts: vec!["player.radio.example".into()], ..Facts::green(LE) }.settle();
            (PageSpec { facts, media: Media::Sound, ..plain("Late-night radio") }, vec![])
        }
        "https://met.example/chennai" => {
            let facts = Facts { tier: Tier::Replayed, ..Facts::green(LE) };
            (PageSpec { facts, ..plain("Weather, Chennai") }, vec![link("https://met.example/madurai", "Madurai")])
        }
        "https://doc.rust.example/releases/1.92" => (plain("Rust 1.92 release notes"), vec![link("https://doc.rust.example/std", "The standard library")]),
        "https://freebsd.example/handbook/jails" => (plain("Handbook: Jails"), vec![link("https://freebsd.example/handbook/zfs", "Handbook: ZFS")]),
        u if u.starts_with("https://atlas.example/") => (plain(&titled(u).replace("(", " (").replace("  ", " ")), atlas_links()),
        u if u.starts_with("http://") => {
            let facts = Facts { insecure: true, connection: "Not private (http)".into(), reasons: vec!["Not private: anyone on this network can read or change this page.".into()], ..Facts::green("") }.settle();
            (PageSpec { facts, ..plain(crate::trail::host(u)) }, vec![])
        }
        u => {
            let t = titled(u);
            let title = if t.is_empty() || t == crate::trail::host(u) { crate::trail::host(u).to_string() } else { t };
            (PageSpec { facts: Facts::green("Stand-in page: nothing was fetched"), ..plain(&title) }, vec![])
        }
    }
}

/// The page a link leads to.
pub fn spec(url: &str) -> PageSpec {
    lookup(url).0
}

fn trail(urls: &[&str]) -> Trail {
    let mut t = Trail::new(0, false);
    for u in urls {
        t.follow(spec(u));
    }
    t
}

/// The stand-in store: the mockups' trails (§5.8 frame 1).
pub fn store() -> TrailStore {
    let mut s = TrailStore::empty();

    // Put away earlier, oldest first so the newest ends up on top.
    let now = crate::clock::now_s();
    for (u, ago_h) in [
        ("https://tessera.example/notes/cas", 74),
        ("https://atlas.example/wiki/Pantheon_dome_concrete", 27),
        ("https://bread.example/hydration-tables", 25),
    ] {
        let mut t = trail(&[u]);
        if u.contains("bread") {
            t.pages[0].title = "Bread hydration tables".into();
        } else if u.contains("tessera") {
            t.pages[0].title = "Tessera CAS notes".into();
        }
        s.earlier.insert(0, PutAway { trail: t, at_s: now - ago_h * 3600 });
    }

    // Open trails, oldest activity first.
    s.add(trail(&["https://mail.example/inbox"]));
    s.add(trail(&["https://met.example/chennai"]));
    s.add(trail(&["https://radio.example/late-night"]));
    s.add(trail(&["https://freebsd.example/handbook/jails"]));
    s.add(trail(&["https://doc.rust.example/releases/1.92"]));

    // The current trail: a search, two pages, then the current page with
    // two branches (one unseen), and one side branch folded.
    let mut t = trail(&[&search_url("roman atrium"), &atlas("Domus_(house)")]);
    t.open_beside(spec(&atlas("Peristyle")));
    t.pages.last_mut().unwrap().seen = true;
    t.follow(spec(&atlas("Atrium_(architecture)")));
    t.open_beside(spec(&atlas("Compluvium_roofs")));
    t.pages.last_mut().unwrap().seen = true;
    t.open_beside(spec(&atlas("Impluvium")));
    s.current = s.add(t);

    s.kept = vec![
        Kept { url: "https://freebsd.example/handbook".into(), title: "Handbook".into() },
        Kept { url: "https://doc.rust.example/std/iter".into(), title: "Implementing Iterator".into() },
        Kept { url: "https://met.example/chennai".into(), title: "Weather, Chennai".into() },
        Kept { url: atlas("Pantheon"), title: "Pantheon".into() },
    ];
    s
}

/// Which made-up moment to open on (for the VM screenshots and demos).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scene {
    /// §5.8 frame 1: the current trail expanded, status bar on.
    Main,
    /// Frame 2: the site knows you and a download is asked.
    Prompt,
    /// Frame 3: a new trail on Start, rail collapsed, typing "impl".
    Start,
    /// Frame 4: a web app running whole as Live code.
    Live,
    /// A red page: the stop page in place of the page (§6.5).
    Stop,
}

impl Scene {
    pub fn parse(s: &str) -> Option<Scene> {
        Some(match s {
            "main" => Scene::Main,
            "prompt" => Scene::Prompt,
            "start" => Scene::Start,
            "live" => Scene::Live,
            "stop" => Scene::Stop,
            _ => return None,
        })
    }
}

/// The store for a scene, and the prompt it opens with, if any.
pub fn scene(sc: Scene) -> (TrailStore, Option<Prompt>) {
    let mut s = store();
    match sc {
        Scene::Main | Scene::Start => (s, None),
        Scene::Prompt => {
            s.set_knows("atlas.example", true);
            (s, Some(Prompt::Download { file: "impluvium-survey.pdf".into(), size: "2.4 MB".into(), origin: "atlas.example".into() }))
        }
        Scene::Live => {
            let i = s.trails.iter().position(|t| t.title() == "Inbox (3)").unwrap();
            s.select(i);
            s.touch();
            s.set_knows("mail.example", true);
            (s, None)
        }
        Scene::Stop => {
            let i = s.trails.iter().position(|t| t.title() == "Inbox (3)").unwrap();
            s.select(i);
            s.set_knows("mail.example", true);
            s.follow(spec("https://n0rthbank.example/login"));
            (s, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_store_matches_the_mockup() {
        let s = store();
        let t = s.trail();
        assert_eq!(t.title(), "Atrium (architecture)");
        let r = t.path_rows();
        assert_eq!(r.path.len(), 3);
        assert_eq!(r.branches.len(), 2);
        assert_eq!(r.folded, 1);
        assert!(t.has_unseen());
        assert_eq!(s.rail_order()[0], s.current);
        assert_eq!(s.trails.len(), 6);
        assert_eq!(s.earlier[0].trail.title(), "Bread hydration tables");
    }

    #[test]
    fn facts_by_kind() {
        assert_eq!(spec("https://mail.example/inbox").facts.tier, Tier::Live);
        assert_eq!(spec("https://mail.example/inbox").facts.light, Light::Amber);
        assert_eq!(spec("https://n0rthbank.example/login").facts.light, Light::Red);
        assert!(spec("http://old-atlas.example/atrium").facts.insecure);
        assert_eq!(spec("https://atlas.example/wiki/Impluvium").facts.light, Light::Green);
        let f = spec("https://atlas.example/films/atrium-walkthrough").facts;
        assert_eq!((f.tier, f.live_parts.len(), f.light), (Tier::Replayed, 1, Light::Amber));
        assert_eq!(spec(&search_url("roman atrium")).search.as_deref(), Some("roman atrium"));
        assert_eq!(spec("https://unknown.example/a_b").title, "a b");
    }

    #[test]
    fn scenes_build() {
        for sc in ["main", "prompt", "start", "live", "stop"] {
            let (s, _) = scene(Scene::parse(sc).unwrap());
            assert!(!s.trails.is_empty());
        }
        let (s, _) = scene(Scene::Stop);
        assert_eq!(s.trail().page().unwrap().facts.light, Light::Red);
    }
}
