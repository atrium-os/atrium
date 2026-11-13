//! The trail store on disk (§4.5).
//!
//! Small records, never page content: per trail its pages' addresses,
//! titles and tree links, which page is current, its mute and activity;
//! the trails put away with when; the kept pages; which sites know you
//! in the Personal jar. Unrecorded trails (and their jars) are never
//! written. The file lives in the user's home, which Tessera encrypts
//! with the session (`~/.local/share/atrium/navigator/trails.json`, or
//! `NAVIGATOR_TRAILS`).
//!
//! What is read back is checked before it is used: the file is the
//! user's, but a bug or a truncated write must not crash the window or
//! build a tree with a cycle. A page's parent must come before it (pages
//! are only ever appended), children are rebuilt from parents, and every
//! count and string is bounded. A trail that fails the check is dropped,
//! not the whole store.
//!
//! The light and tier of each page are not stored: they are the broker's
//! word and come back when the page is opened again.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::trail::{Facts, Kept, Media, Page, Persona, PutAway, Trail, TrailStore};

pub const FORMAT: &str = "atrium-navigator-trails/1";
pub const MAX_TRAILS: usize = 512;
pub const MAX_EARLIER: usize = 4096;
pub const MAX_PAGES: usize = 4096;
pub const MAX_KEPT: usize = 4096;
pub const MAX_URL: usize = 16 * 1024;
pub const MAX_TITLE: usize = 256;
/// The largest file read back.
pub const MAX_FILE: u64 = 64 << 20;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct File {
    format: String,
    persona: String,
    /// Index into `trails` of the trail on screen.
    current: usize,
    trails: Vec<TrailRec>,
    earlier: Vec<EarlierRec>,
    kept: Vec<KeptRec>,
    /// Sites that know you in the Personal jar (§6.2).
    knows: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct TrailRec {
    pages: Vec<PageRec>,
    current: Option<usize>,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    active: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct PageRec {
    url: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<usize>,
    #[serde(default = "yes")]
    seen: bool,
}

fn yes() -> bool { true }

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct EarlierRec {
    at_s: u64,
    trail: TrailRec,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct KeptRec {
    url: String,
    title: String,
}

/// Where the store lives.
pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("NAVIGATOR_TRAILS") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/nonexistent"));
    home.join(".local/share/atrium/navigator/trails.json")
}

fn clean(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

fn cut(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn trail_rec(t: &Trail) -> TrailRec {
    TrailRec {
        pages: t.pages.iter().map(|p| PageRec { url: p.url.clone(), title: cut(&p.title, MAX_TITLE), search: p.search.clone(), parent: p.parent, seen: p.seen }).collect(),
        current: t.current,
        muted: t.muted,
        active: t.active,
    }
}

/// A trail record as a trail, or why it isn't one.
fn trail_of(r: TrailRec, facts: &dyn Fn(&str) -> Facts) -> Result<Trail, String> {
    if r.pages.len() > MAX_PAGES {
        return Err(format!("{} pages", r.pages.len()));
    }
    let mut t = Trail::new(0, false);
    for (i, p) in r.pages.into_iter().enumerate() {
        if !clean(&p.url, MAX_URL) || p.url.is_empty() {
            return Err(format!("page {i}: a bad address"));
        }
        if p.parent.is_some_and(|q| q >= i) {
            return Err(format!("page {i}: its parent does not come before it"));
        }
        if p.search.as_ref().is_some_and(|q| !clean(q, MAX_URL)) {
            return Err(format!("page {i}: a bad search"));
        }
        t.pages.push(Page {
            facts: facts(&p.url),
            url: p.url,
            title: cut(&p.title, MAX_TITLE),
            search: p.search,
            parent: p.parent,
            children: vec![],
            seen: p.seen,
            media: Media::None,
            asked: None,
        });
        if let Some(q) = p.parent {
            t.pages[q].children.push(i);
        }
    }
    if r.current.is_some_and(|c| c >= t.pages.len()) {
        return Err("the current page is not in the trail".into());
    }
    t.current = r.current;
    t.muted = r.muted;
    t.active = r.active;
    Ok(t)
}

/// The store as its file's text. Unrecorded trails are left out.
pub fn encode(s: &TrailStore) -> String {
    let recorded: Vec<usize> = (0..s.trails.len()).filter(|&i| !s.trails[i].unrecorded).collect();
    let current = recorded.iter().position(|&i| i == s.current).unwrap_or(0);
    let f = File {
        format: FORMAT.into(),
        persona: match s.persona {
            Persona::Personal => "personal".into(),
            Persona::Unrecorded => "unrecorded".into(),
        },
        current,
        trails: recorded.iter().map(|&i| trail_rec(&s.trails[i])).collect(),
        earlier: s.earlier.iter().filter(|e| !e.trail.unrecorded).map(|e| EarlierRec { at_s: e.at_s, trail: trail_rec(&e.trail) }).collect(),
        kept: s.kept.iter().map(|k| KeptRec { url: k.url.clone(), title: cut(&k.title, MAX_TITLE) }).collect(),
        knows: s.known.iter().filter(|(j, _)| j == "personal").map(|(_, site)| site.clone()).collect(),
    };
    serde_json::to_string_pretty(&f).expect("encodes")
}

/// A store from a file's text. `facts` gives each page its light until
/// the broker says otherwise. Returns the store and what was dropped.
pub fn decode(text: &str, facts: &dyn Fn(&str) -> Facts) -> Result<(TrailStore, Vec<String>), String> {
    let f: File = serde_json::from_str(text).map_err(|e| format!("not a trail store: {e}"))?;
    if f.format != FORMAT {
        return Err(format!("format {:?}, not {FORMAT}", f.format));
    }
    let mut dropped = Vec::new();
    let mut s = TrailStore::empty();
    s.persona = if f.persona == "unrecorded" { Persona::Unrecorded } else { Persona::Personal };
    let n = f.trails.len();
    let mut current = None;
    for (i, r) in f.trails.into_iter().take(MAX_TRAILS).enumerate() {
        match trail_of(r, facts) {
            Ok(mut t) => {
                t.id = s.next_id;
                s.next_id += 1;
                s.clock = s.clock.max(t.active);
                s.trails.push(t);
                if i == f.current {
                    current = Some(s.trails.len() - 1);
                }
            }
            Err(e) => dropped.push(format!("trail {i}: {e}")),
        }
    }
    if n > MAX_TRAILS {
        dropped.push(format!("{} trails past {MAX_TRAILS}", n - MAX_TRAILS));
    }
    for (i, e) in f.earlier.into_iter().take(MAX_EARLIER).enumerate() {
        match trail_of(e.trail, facts) {
            Ok(mut t) => {
                t.id = s.next_id;
                s.next_id += 1;
                s.earlier.push(PutAway { trail: t, at_s: e.at_s });
            }
            Err(err) => dropped.push(format!("put away {i}: {err}")),
        }
    }
    for k in f.kept.into_iter().take(MAX_KEPT) {
        if clean(&k.url, MAX_URL) && !k.url.is_empty() {
            s.kept.push(Kept { url: k.url, title: cut(&k.title, MAX_TITLE) });
        }
    }
    for site in f.knows.into_iter().take(MAX_KEPT) {
        if clean(&site, 253) && !site.is_empty() {
            s.known.insert(("personal".into(), site));
        }
    }
    if s.trails.is_empty() {
        s.new_trail(false);
    } else {
        s.current = current.unwrap_or(0);
    }
    Ok((s, dropped))
}

/// Read the store at `p`; `Ok(None)` when there is none yet.
pub fn load(p: &Path, facts: &dyn Fn(&str) -> Facts) -> Result<Option<(TrailStore, Vec<String>)>, String> {
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    if meta.len() > MAX_FILE {
        return Err(format!("{}: {} bytes, over {MAX_FILE}", p.display(), meta.len()));
    }
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    decode(&text, facts).map(Some)
}

/// Write `text` to `p` whole or not at all (a temporary file renamed over
/// it), readable by the user alone.
pub fn save(p: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = p.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trail::PageSpec;

    fn spec(u: &str) -> PageSpec {
        PageSpec { url: u.into(), title: u.trim_start_matches("https://").into(), search: None, facts: Facts::green(""), media: Media::None }
    }

    fn facts(_: &str) -> Facts { Facts::green("") }

    #[test]
    fn a_store_round_trips_without_its_unrecorded_trails() {
        let mut s = TrailStore::default();
        s.follow(spec("https://atlas.example/wiki/Atrium"));
        s.follow(spec("https://atlas.example/wiki/Impluvium"));
        s.trail_mut().back();
        s.trail_mut().open_beside(spec("https://atlas.example/wiki/Compluvium"));
        s.set_knows("atlas.example", true);
        s.toggle_keep();
        s.new_trail(false);
        s.follow(spec("https://radio.example/"));
        s.trail_mut().muted = true;
        s.put_away(1_000);
        s.new_trail(true);
        s.follow(spec("https://secret.example/"));
        s.set_knows("secret.example", true);
        s.new_trail(false);
        s.follow(spec("https://met.example/"));
        let text = encode(&s);
        assert!(!text.contains("secret.example"), "nothing Unrecorded is written");
        let (back, dropped) = decode(&text, &facts).unwrap();
        assert!(dropped.is_empty(), "{dropped:?}");
        assert_eq!(back.trails.len(), 2);
        assert_eq!(back.trail().page().unwrap().url, "https://met.example/", "the trail on screen");
        let atrium = back.trails.iter().find(|t| t.pages.len() == 3).unwrap();
        assert_eq!(atrium.pages[0].children, vec![1, 2], "the tree, children from parents");
        assert_eq!(atrium.current, Some(0));
        assert!(!atrium.pages[2].seen, "opened beside, not looked at");
        assert_eq!(back.earlier.len(), 1);
        assert_eq!(back.earlier[0].at_s, 1_000);
        assert!(back.earlier[0].trail.muted);
        assert_eq!(back.kept.len(), 1);
        let mut b = back.clone();
        b.select(b.trails.iter().position(|t| t.pages.len() == 3).unwrap());
        assert!(b.knows("atlas.example"));
        assert_eq!(encode(&back), text, "stable: a save after a load writes the same file");
    }

    #[test]
    fn a_bad_trail_is_dropped_not_the_store() {
        let mut s = TrailStore::default();
        s.follow(spec("https://a.example/"));
        s.new_trail(false);
        s.follow(spec("https://b.example/"));
        let text = encode(&s).replacen("\"current\": 0", "\"current\": 7", 1);
        let (back, dropped) = decode(&text, &facts).unwrap();
        assert_eq!(dropped.len(), 1, "{dropped:?}");
        assert_eq!(back.trails.len(), 1);
        // A cycle: a page whose parent is itself.
        let cyc = r#"{"format":"atrium-navigator-trails/1","persona":"personal","current":0,"trails":[{"pages":[{"url":"https://a.example/","title":"a","parent":0}],"current":0}],"earlier":[],"kept":[],"knows":[]}"#;
        let (back, dropped) = decode(cyc, &facts).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(back.trails.len(), 1, "a fresh trail on Start instead");
        assert!(back.trail().current.is_none());
        assert!(decode("{\"format\":\"other\"}", &facts).is_err());
        assert!(decode("not json", &facts).is_err());
    }

    #[test]
    fn saves_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("nav-trails-{}", std::process::id()));
        let p = d.join("sub/trails.json");
        assert!(load(&p, &facts).unwrap().is_none());
        let mut s = TrailStore::default();
        s.follow(spec("https://a.example/"));
        save(&p, &encode(&s)).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let (back, _) = load(&p, &facts).unwrap().unwrap();
        assert_eq!(back.trail().page().unwrap().url, "https://a.example/");
        let _ = std::fs::remove_dir_all(d);
    }
}
