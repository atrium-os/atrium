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
            t.pages.iter().enumerate().map(move |(id, p)| Row { title: p.title.clone(), url: p.url.clone(), trail: trail.clone(), at_s: p.at_s, place, page: id })
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
}
