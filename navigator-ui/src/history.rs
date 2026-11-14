//! The history view (docs/spec/navigator-ui.md §5.5): the pages of the
//! open trails and of Earlier, newest first, grouped by day, a page the UI
//! draws itself. Opening one goes to it in its trail (bringing an Earlier
//! trail back), so it opens as the version the store kept (§4.4).
//! Unrecorded trails are not history and are left out.

use crate::trail::{PageId, TrailStore};

/// The most rows the view lists.
pub const ROWS_MAX: usize = 200;

/// Where a row's page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// An open trail, by its index in `trails`.
    Open(usize),
    /// A trail put away, by its index in `earlier`.
    Earlier(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub title: String,
    pub url: String,
    /// The trail's title, to tell rows from different trails apart.
    pub trail: String,
    pub at_s: u64,
    pub place: Place,
    pub page: PageId,
}

/// The rows matching `filter` (case folded, in the title or the address),
/// newest first; a page seen in more than one place is listed once, where
/// it was seen last.
pub fn rows(store: &TrailStore, filter: &str) -> Vec<Row> {
    let f = filter.trim().to_lowercase();
    let open = store.trails.iter().enumerate().map(|(i, t)| (Place::Open(i), t));
    let earlier = store.earlier.iter().enumerate().map(|(i, e)| (Place::Earlier(i), &e.trail));
    let mut v: Vec<Row> = open
        .chain(earlier)
        .filter(|(_, t)| !t.unrecorded)
        .flat_map(|(place, t)| {
            let trail = t.title().to_string();
            t.pages.iter().enumerate().filter(|(_, p)| !p.cleared()).map(move |(id, p)| Row { title: p.title.clone(), url: p.url.clone(), trail: trail.clone(), at_s: p.at_s, place, page: id })
        })
        .filter(|r| f.is_empty() || r.title.to_lowercase().contains(&f) || r.url.to_lowercase().contains(&f))
        .collect();
    v.sort_by(|a, b| b.at_s.cmp(&a.at_s));
    let mut seen = std::collections::HashSet::new();
    v.retain(|r| seen.insert(r.url.clone()));
    v.truncate(ROWS_MAX);
    v
}

/// Go to a row's page: its trail shown, the page current.
pub fn open(store: &mut TrailStore, r: &Row) -> bool {
    match r.place {
        Place::Open(i) => {
            if !store.select(i) {
                return false;
            }
        }
        Place::Earlier(i) => {
            if !store.bring_back(i) {
                return false;
            }
        }
    }
    store.trail_mut().go(r.page)
}

/// The day a row is listed under: "Earlier" for a page with no time.
pub fn day_of(at_s: u64, now_s: u64) -> String {
    if at_s == 0 { "Earlier".to_string() } else { crate::clock::day_label(at_s, now_s) }
}

/// What clearing history takes (§5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clear {
    /// The pages listed under a day label.
    Day(String),
    /// A site's pages (registrable domain, as `trail::site`).
    Site(String),
    All,
}

impl Clear {
    fn takes(&self, url: &str, at_s: u64, now_s: u64) -> bool {
        match self {
            Clear::Day(d) => day_of(at_s, now_s) == *d,
            Clear::Site(s) => crate::trail::site(url) == s,
            Clear::All => true,
        }
    }

    /// As the confirmation names it.
    pub fn label(&self) -> String {
        match self {
            Clear::Day(d) => format!("from {d}"),
            Clear::Site(s) => format!("from {s}"),
            Clear::All => "from all history".into(),
        }
    }
}

/// What a clear took.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cleared {
    /// How many page records left the trail store.
    pub pages: usize,
    /// Pages that went (any page child of theirs closes).
    pub keys: Vec<(u64, PageId)>,
    /// The stored versions navigatord is told to take out: those no page
    /// left names, and not a kept page's.
    pub records: Vec<String>,
}

/// Clear history (§5.5): the pages `what` takes leave the recorded trails,
/// open and put away, and their stored versions go with them. Each open
/// trail keeps the page it is on (it is on screen, or a click away); a
/// trail put away that is left with nothing goes. Unrecorded trails are
/// not history. Kept pages stay listed as kept, with their stored version.
pub fn clear(store: &mut TrailStore, what: &Clear, now_s: u64) -> Cleared {
    let mut out = Cleared::default();
    let mut saved = Vec::new();
    for (open, t) in store
        .trails
        .iter_mut()
        .map(|t| (true, t))
        .chain(store.earlier.iter_mut().map(|e| (false, &mut e.trail)))
        .filter(|(_, t)| !t.unrecorded)
    {
        let ids: Vec<PageId> = (0..t.pages.len())
            .filter(|&i| !(open && t.current == Some(i)))
            .filter(|&i| !t.pages[i].cleared() && what.takes(&t.pages[i].url, t.pages[i].at_s, now_s))
            .collect();
        for i in ids {
            let url = t.pages[i].url.clone();
            if let Some(a) = t.clear_page(i) {
                saved.push((a, url));
            }
            out.pages += 1;
            out.keys.push((t.id, i));
        }
    }
    store.earlier.retain(|e| !e.trail.is_empty());
    let still: std::collections::HashSet<&str> = store
        .trails
        .iter()
        .chain(store.earlier.iter().map(|e| &e.trail))
        .flat_map(|t| t.pages.iter().filter_map(|p| p.saved.as_deref()))
        .collect();
    for (a, url) in saved {
        if !still.contains(a.as_str()) && !store.is_kept(&url) && !out.records.contains(&a) {
            out.records.push(a);
        }
    }
    out
}

/// How many pages a clear would take, for its confirmation.
pub fn count(store: &TrailStore, what: &Clear, now_s: u64) -> usize {
    clear(&mut store.clone(), what, now_s).pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trail::{Facts, Media, PageSpec};

    fn spec(url: &str, title: &str) -> PageSpec {
        PageSpec { url: url.into(), title: title.into(), search: None, facts: Facts::green("Private: https"), media: Media::None }
    }

    #[test]
    fn rows_are_newest_first_filtered_and_open_their_page() {
        let mut s = TrailStore::empty();
        s.new_trail(false);
        s.follow(spec("https://atlas.example/wiki/Atrium", "Atrium"));
        s.follow(spec("https://atlas.example/wiki/Impluvium", "Impluvium"));
        s.trail_mut().pages[0].at_s = 100;
        s.trail_mut().pages[1].at_s = 200;
        s.put_away(300);
        s.new_trail(false);
        s.follow(spec("https://n0rthbank.example/", "North Bank"));
        s.trail_mut().pages[0].at_s = 400;
        s.new_trail(true);
        s.follow(spec("https://secret.example/", "Secret"));

        let all = rows(&s, "");
        assert_eq!(all.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["North Bank", "Impluvium", "Atrium"], "no Unrecorded pages");
        let atlas = rows(&s, "ATLAS");
        assert_eq!(atlas.len(), 2);
        let atrium = rows(&s, "atrium").into_iter().next().unwrap();
        assert_eq!(atrium.place, Place::Earlier(0));
        assert!(open(&mut s, &atrium));
        assert_eq!(s.trail().page().unwrap().title, "Atrium", "brought back, at that page");
        assert!(s.earlier.is_empty());
    }

    #[test]
    fn clearing_takes_a_day_a_site_or_all_and_keeps_each_open_trails_page() {
        let day = 86_400;
        let now = 100 * day + 3600;
        let mut s = TrailStore::empty();
        s.new_trail(false);
        for (u, at) in [("https://atlas.example/a", now - 2 * day), ("https://atlas.example/b", now - 60), ("https://n0rthbank.example/", now - 30)] {
            s.follow(spec(u, u));
            let t = s.trail_mut();
            let c = t.current.unwrap();
            t.pages[c].at_s = at;
            t.pages[c].saved = Some(format!("{:0>64}", c));
        }
        s.toggle_keep();
        // Put away a trail with one atlas page, and keep an Unrecorded one.
        s.new_trail(false);
        s.follow(spec("https://atlas.example/c", "c"));
        s.trail_mut().pages[0].at_s = now - 2 * day;
        s.trail_mut().pages[0].saved = Some("c".repeat(64));
        s.put_away(now);
        s.new_trail(true);
        s.follow(spec("https://atlas.example/u", "u"));
        let today = day_of(now - 60, now);
        let older = day_of(now - 2 * day, now);
        assert_eq!(count(&s, &Clear::Day(older.clone()), now), 2);
        let c = clear(&mut s, &Clear::Day(older), now);
        assert_eq!(c.pages, 2);
        assert_eq!(c.keys.len(), 2, "an open trail's page and the put-away one's");
        assert_eq!(c.records, vec!["0".repeat(64), "c".repeat(64)]);
        assert!(s.earlier.is_empty(), "the trail put away had nothing left");
        let first = s.trails.iter().find(|t| !t.unrecorded).unwrap();
        assert_eq!(first.pages[1].parent, None, "b hangs where a was");
        assert_eq!(rows(&s, "").len(), 2);
        // A site: the trail keeps the page it is on.
        let c = clear(&mut s, &Clear::Site("n0rthbank.example".into()), now);
        assert_eq!(c.pages, 0, "the page the trail is on stays");
        let c = clear(&mut s, &Clear::Site("atlas.example".into()), now);
        assert_eq!((c.pages, c.records.len()), (1, 1));
        assert!(rows(&s, "").iter().all(|r| r.url.contains("n0rthbank")));
        // All: a kept page's stored version stays.
        let rec = s.trails.iter().position(|t| !t.unrecorded).unwrap();
        s.select(rec);
        s.trail_mut().follow(spec("https://atlas.example/d", "d"));
        let all = clear(&mut s, &Clear::All, now);
        assert!(all.records.is_empty(), "the kept page's version stays, d had none");
        assert_eq!(day_of(0, now), "Earlier");
        assert!(today != "Earlier");
        // Unrecorded trails are untouched.
        assert!(s.trails.iter().any(|t| t.unrecorded && t.pages.len() == 1 && !t.pages[0].cleared()));
        // The store on disk leaves cleared pages out.
        let t = s.trails.iter().find(|t| !t.unrecorded).unwrap().compacted();
        assert!(t.pages.iter().all(|p| !p.cleared()) && t.current.is_some_and(|c| c < t.pages.len()));
    }
}
