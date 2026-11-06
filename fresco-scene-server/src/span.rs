//! Span geometry (fresco-span.md §3, §4): where the monitors of a span sit
//! inside the window that covers them, with the bezel strips between them.
//!
//! Pure arithmetic over the monitors' places on the desktop, shared by the
//! frontend (which answers `OP_WM_SPAN_GROUP` with the window's rect) and
//! frescod (which moves the monitors apart for the bezels, paces them
//! together, and tells the window its tiles).

/// A monitor as the span sees it: identity and its rect on the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub identity: u64,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// A span laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanLayout {
    /// The window's rect on the desktop, the bezel strips included.
    pub rect: [i32; 4],
    /// Where each monitor goes (identity, x, y) while the span is up: the
    /// members spread apart by the bezels, and every other monitor past the
    /// group moved along with them. Only the ones that move.
    pub moves: Vec<(u64, i32, i32)>,
    /// Each member's part of the window `[x, y, w, h]`, in `members` order.
    pub tiles: Vec<[u32; 4]>,
    /// The bezel strips no monitor shows, in window pixels.
    pub hidden: Vec<[u32; 4]>,
}

/// The largest span surface, per side.
pub const MAX_DIM: u32 = fresco_protocol::SPAN_MAX_DIM;

/// Lay out a span of `members` (identities) over the monitors `all`, with
/// `gaps_x` between columns and `gaps_y` between rows (empty: none). The
/// members must be at least two, all one size, landscape, and fill a grid
/// of touching monitors.
pub fn layout(all: &[Place], members: &[u64], gaps_x: &[u32], gaps_y: &[u32]) -> Result<SpanLayout, String> {
    if members.len() < 2 {
        return Err("a span needs two monitors or more".into());
    }
    if members.len() > fresco_protocol::SPAN_MAX_MONITORS {
        return Err(format!("a span covers {} monitors at most", fresco_protocol::SPAN_MAX_MONITORS));
    }
    let mut ms = Vec::with_capacity(members.len());
    for (i, id) in members.iter().enumerate() {
        if members[..i].contains(id) {
            return Err(format!("monitor {id:#x} named twice"));
        }
        ms.push(*all.iter().find(|p| p.identity == *id).ok_or_else(|| format!("monitor {id:#x} is not on the desktop"))?);
    }
    let (w, h) = (ms[0].w, ms[0].h);
    if ms.iter().any(|m| (m.w, m.h) != (w, h)) {
        return Err("the monitors of a span must all run one mode".into());
    }
    if w < h || w == 0 {
        return Err("a span is landscape only".into());
    }
    let x0 = ms.iter().map(|m| m.x).min().unwrap();
    let y0 = ms.iter().map(|m| m.y).min().unwrap();
    let mut col = Vec::with_capacity(ms.len());
    let mut row = Vec::with_capacity(ms.len());
    for m in &ms {
        let (dx, dy) = (i64::from(m.x) - i64::from(x0), i64::from(m.y) - i64::from(y0));
        if dx % i64::from(w) != 0 || dy % i64::from(h) != 0 {
            return Err("the monitors of a span must touch, in a row or a grid".into());
        }
        col.push((dx / i64::from(w)) as usize);
        row.push((dy / i64::from(h)) as usize);
    }
    let cols = col.iter().max().unwrap() + 1;
    let rows = row.iter().max().unwrap() + 1;
    if cols * rows != ms.len() {
        return Err("the monitors of a span must fill a grid".into());
    }
    for (i, _) in ms.iter().enumerate() {
        if (0..i).any(|j| col[j] == col[i] && row[j] == row[i]) {
            return Err("two monitors of the span overlap".into());
        }
    }
    if !(gaps_x.is_empty() || gaps_x.len() == cols - 1) || !(gaps_y.is_empty() || gaps_y.len() == rows - 1) {
        return Err("one bezel gap between each pair of columns and rows".into());
    }
    let gx = |c: usize| -> u64 { gaps_x.iter().take(c).map(|&g| u64::from(g)).sum() };
    let gy = |r: usize| -> u64 { gaps_y.iter().take(r).map(|&g| u64::from(g)).sum() };
    let width = cols as u64 * u64::from(w) + gx(cols);
    let height = rows as u64 * u64::from(h) + gy(rows);
    if width > u64::from(MAX_DIM) || height > u64::from(MAX_DIM) {
        return Err(format!("a span of {width}x{height} is larger than {MAX_DIM} a side"));
    }
    let tile = |c: usize, r: usize| -> [u32; 4] {
        [(c as u64 * u64::from(w) + gx(c)) as u32, (r as u64 * u64::from(h) + gy(r)) as u32, w, h]
    };
    let tiles: Vec<[u32; 4]> = (0..ms.len()).map(|i| tile(col[i], row[i])).collect();
    let mut hidden = Vec::new();
    for (c, &g) in gaps_x.iter().enumerate() {
        if g != 0 {
            hidden.push([tile(c, 0)[0] + w, 0, g, height as u32]);
        }
    }
    for (r, &g) in gaps_y.iter().enumerate() {
        if g != 0 {
            hidden.push([0, tile(0, r)[1] + h, width as u32, g]);
        }
    }
    /* Members spread; monitors wholly past the group move along. */
    let (right, bottom) = (i64::from(x0) + cols as i64 * i64::from(w), i64::from(y0) + rows as i64 * i64::from(h));
    let mut moves = Vec::new();
    for p in all {
        let (nx, ny) = match ms.iter().position(|m| m.identity == p.identity) {
            Some(i) => (x0 + tiles[i][0] as i32, y0 + tiles[i][1] as i32),
            None => (
                if i64::from(p.x) >= right { p.x + gx(cols) as i32 } else { p.x },
                if i64::from(p.y) >= bottom { p.y + gy(rows) as i32 } else { p.y },
            ),
        };
        if (nx, ny) != (p.x, p.y) {
            moves.push((p.identity, nx, ny));
        }
    }
    Ok(SpanLayout { rect: [x0, y0, width as i32, height as i32], moves, tiles, hidden })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: u64) -> Vec<Place> {
        (0..n).map(|i| Place { identity: 10 + i, x: 1920 * i as i32, y: 0, w: 1920, h: 1080 }).collect()
    }

    #[test]
    fn three_in_a_row() {
        let l = layout(&row(3), &[10, 11, 12], &[], &[]).unwrap();
        assert_eq!(l.rect, [0, 0, 5760, 1080]);
        assert_eq!(l.tiles, vec![[0, 0, 1920, 1080], [1920, 0, 1920, 1080], [3840, 0, 1920, 1080]]);
        assert!(l.hidden.is_empty() && l.moves.is_empty());
    }

    #[test]
    fn bezels_spread_the_members_and_move_what_is_past_them() {
        let mut all = row(3);
        all.push(Place { identity: 99, x: 5760, y: 0, w: 2560, h: 1440 });
        let l = layout(&all, &[10, 11, 12], &[40, 40], &[]).unwrap();
        assert_eq!(l.rect, [0, 0, 5840, 1080]);
        assert_eq!(l.tiles[1], [1960, 0, 1920, 1080]);
        assert_eq!(l.tiles[2], [3920, 0, 1920, 1080]);
        assert_eq!(l.hidden, vec![[1920, 0, 40, 1080], [3880, 0, 40, 1080]]);
        assert_eq!(l.moves, vec![(11, 1960, 0), (12, 3920, 0), (99, 5840, 0)]);
    }

    #[test]
    fn a_grid_in_any_order() {
        let all = vec![
            Place { identity: 1, x: 0, y: 0, w: 1920, h: 1080 },
            Place { identity: 2, x: 1920, y: 0, w: 1920, h: 1080 },
            Place { identity: 3, x: 0, y: 1080, w: 1920, h: 1080 },
            Place { identity: 4, x: 1920, y: 1080, w: 1920, h: 1080 },
        ];
        let l = layout(&all, &[4, 1, 3, 2], &[10], &[20]).unwrap();
        assert_eq!(l.rect, [0, 0, 3850, 2180]);
        assert_eq!(l.tiles, vec![[1930, 1100, 1920, 1080], [0, 0, 1920, 1080], [0, 1100, 1920, 1080], [1930, 0, 1920, 1080]]);
        assert_eq!(l.hidden, vec![[1920, 0, 10, 2180], [0, 1080, 3850, 20]]);
    }

    #[test]
    fn refusals() {
        let mut all = row(3);
        assert!(layout(&all, &[10], &[], &[]).is_err(), "one monitor");
        assert!(layout(&all, &[10, 10], &[], &[]).is_err(), "named twice");
        assert!(layout(&all, &[10, 77], &[], &[]).is_err(), "not on the desktop");
        assert!(layout(&all, &[10, 12], &[], &[]).is_err(), "not touching");
        assert!(layout(&all, &[10, 11], &[1, 2], &[]).is_err(), "gap count");
        all[1].w = 2560;
        assert!(layout(&all, &[10, 11], &[], &[]).is_err(), "mixed sizes");
        let portrait = vec![Place { identity: 1, x: 0, y: 0, w: 1080, h: 1920 }, Place { identity: 2, x: 1080, y: 0, w: 1080, h: 1920 }];
        assert!(layout(&portrait, &[1, 2], &[], &[]).is_err(), "portrait");
        let big: Vec<Place> = (0..5).map(|i| Place { identity: i, x: 3840 * i as i32, y: 0, w: 3840, h: 2160 }).collect();
        assert!(layout(&big, &[0, 1, 2, 3, 4], &[], &[]).is_err(), "past 16384");
        assert!(layout(&big, &[0, 1, 2, 3], &[], &[]).is_ok(), "4x 4K fits");
    }
}
