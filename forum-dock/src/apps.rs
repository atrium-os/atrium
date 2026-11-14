//! The Applications view's model (docs/spec/forum.md §6.1): which apps it
//! lists, how the filter ranks them, how many fit a page, and how the
//! keyboard moves the selection. No drawing here; `apps_view` draws it.

use crate::AppEntry;

/// Forum's own components: started by the session, not by the person, so
/// never listed.
pub fn is_session_component(id: &str) -> bool {
    id.starts_with("org.atrium.forum-") || id == "org.atrium.vestibulum"
}

/// The apps the view lists, by name (case-insensitive, then id).
pub fn listed(apps: &[AppEntry]) -> Vec<AppEntry> {
    let mut v: Vec<AppEntry> = apps.iter().filter(|a| !is_session_component(&a.id)).cloned().collect();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
    v
}

/// How well `app` matches the typed `query`: `None` if it doesn't. Lower is
/// better: 0 the name starts with what was typed; 1 every word typed starts
/// a word of the name; 2 every word is in the name; 3 a word is only in the
/// id or description.
pub fn rank(app: &AppEntry, query: &str) -> Option<u8> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let name = app.name.to_lowercase();
    let hay = format!("{name} {} {}", app.id.to_lowercase(), app.description.as_deref().unwrap_or("").to_lowercase());
    let words: Vec<&str> = q.split_whitespace().collect();
    if !words.iter().all(|w| hay.contains(w)) {
        return None;
    }
    let name_words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    Some(if name.starts_with(&q) {
        0
    } else if words.iter().all(|w| name_words.iter().any(|n| n.starts_with(w))) {
        1
    } else if words.iter().all(|w| name.contains(w)) {
        2
    } else {
        3
    })
}

/// The matches for `query`, best first; within a rank in `apps`' order
/// (which [`listed`] made name order). Indices into `apps`.
pub fn filter(apps: &[AppEntry], query: &str) -> Vec<usize> {
    let mut m: Vec<(u8, usize)> = apps.iter().enumerate().filter_map(|(i, a)| rank(a, query).map(|r| (r, i))).collect();
    m.sort();
    m.into_iter().map(|(_, i)| i).collect()
}

/// How the grid fits its area: columns and rows per page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
}

impl Grid {
    /// As many `cell_w` x `cell_h` cells as fit `w` x `h` (at least one),
    /// at most `max_cols` across (a very wide screen keeps a readable grid).
    pub fn fit(w: f32, h: f32, cell_w: f32, cell_h: f32, max_cols: usize) -> Grid {
        let n = |room: f32, cell: f32| if cell > 0.0 && room.is_finite() { (room / cell).floor().max(1.0) as usize } else { 1 };
        Grid { cols: n(w, cell_w).min(max_cols.max(1)), rows: n(h, cell_h) }
    }

    pub fn per_page(&self) -> usize {
        self.cols * self.rows
    }

    /// Pages for `n` apps (at least one, so an empty list still has a page).
    pub fn pages(&self, n: usize) -> usize {
        n.div_ceil(self.per_page()).max(1)
    }

    /// The page showing item `i`.
    pub fn page_of(&self, i: usize) -> usize {
        i / self.per_page()
    }
}

/// A keyboard move of the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
}

/// The selection after `m`, for `n` items in `g`. Rows run on across pages:
/// Down from a page's last row goes to the next page's first. Down onto a
/// short last row lands on its last item; past the end stays put.
pub fn moved(sel: usize, n: usize, g: Grid, m: Move) -> usize {
    if n == 0 {
        return 0;
    }
    let sel = sel.min(n - 1);
    match m {
        Move::Left => sel.saturating_sub(1),
        Move::Right => (sel + 1).min(n - 1),
        Move::Up => sel.checked_sub(g.cols).unwrap_or(sel),
        Move::Down => {
            let last_row = (n - 1) / g.cols;
            if sel / g.cols >= last_row { sel } else { (sel + g.cols).min(n - 1) }
        }
        // A whole page, onto that page's first item, so the view turns.
        Move::PageUp => g.page_of(sel).saturating_sub(1) * g.per_page(),
        Move::PageDown => {
            let next = (g.page_of(sel) + 1) * g.per_page();
            if next < n { next } else { sel }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, desc: Option<&str>) -> AppEntry {
        AppEntry { id: id.into(), name: name.into(), description: desc.map(Into::into), icon: None, uid: None }
    }

    fn sample() -> Vec<AppEntry> {
        listed(&[
            app("org.atrium.write", "Atrium Write", Some("Word processor")),
            app("org.atrium.navigator", "Navigator", Some("The web")),
            app("org.atrium.forum-bar", "Forum Bar", None),
            app("org.atrium.scrinium", "Scrinium", Some("Files and folders")),
            app("org.atrium.sheets", "Atrium Sheets", Some("Spreadsheets")),
            app("org.atrium.vestibulum", "Vestibulum", None),
            app("org.atrium.player", "Player", Some("Music and video, with a navigator of tracks")),
        ])
    }

    fn names(apps: &[AppEntry], idx: &[usize]) -> Vec<String> {
        idx.iter().map(|&i| apps[i].name.clone()).collect()
    }

    #[test]
    fn session_components_are_not_listed_and_the_rest_are_by_name() {
        let a = sample();
        let n: Vec<&str> = a.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(n, ["Atrium Sheets", "Atrium Write", "Navigator", "Player", "Scrinium"]);
    }

    #[test]
    fn the_name_start_beats_a_word_start_beats_the_description() {
        let a = sample();
        assert_eq!(names(&a, &filter(&a, "nav")), ["Navigator", "Player"]);
        assert_eq!(names(&a, &filter(&a, "w")), ["Atrium Write", "Navigator", "Player"]);
        assert_eq!(names(&a, &filter(&a, "write")), ["Atrium Write"]);
        // Every word must match somewhere.
        assert_eq!(names(&a, &filter(&a, "atrium sh")), ["Atrium Sheets"]);
        assert_eq!(names(&a, &filter(&a, "files")), ["Scrinium"]);
        assert!(filter(&a, "zzz").is_empty());
        // Nothing typed: everything, in name order.
        assert_eq!(filter(&a, "  ").len(), a.len());
    }

    #[test]
    fn the_grid_fits_and_pages() {
        let g = Grid::fit(1000.0, 500.0, 120.0, 140.0, 7);
        assert_eq!(g, Grid { cols: 7, rows: 3 });
        assert_eq!(g.pages(0), 1);
        assert_eq!(g.pages(21), 1);
        assert_eq!(g.pages(22), 2);
        assert_eq!(Grid::fit(10.0, 10.0, 120.0, 140.0, 7), Grid { cols: 1, rows: 1 });
    }

    #[test]
    fn keyboard_moves() {
        let g = Grid { cols: 4, rows: 2 };
        let n = 10; // two pages: 0..8 and 8..10
        assert_eq!(moved(0, n, g, Move::Left), 0);
        assert_eq!(moved(3, n, g, Move::Right), 4);
        assert_eq!(moved(9, n, g, Move::Right), 9);
        assert_eq!(moved(5, n, g, Move::Up), 1);
        assert_eq!(moved(1, n, g, Move::Up), 1);
        // Down from page one's last row runs on to page two.
        assert_eq!(moved(6, n, g, Move::Down), 9);
        assert_eq!(moved(4, n, g, Move::Down), 8);
        assert_eq!(moved(8, n, g, Move::Down), 8);
        assert_eq!(moved(2, n, g, Move::PageDown), 8);
        assert_eq!(moved(9, n, g, Move::PageDown), 9);
        assert_eq!(moved(9, n, g, Move::PageUp), 0);
        assert_eq!(moved(0, 0, g, Move::Down), 0);
    }
}
