//! ★ COLLAPSED TABLE BORDERS, compiled into separated ones (CSS 2.1 §17.6.2).
//!
//! The profile's tables use the separated-borders model only: every cell
//! draws its own four borders, `border-spacing` apart. A page written for
//! `border-collapse: collapse` — Wikipedia's `wikitable`, every data table
//! with `td { border: 1px solid }` — then draws each grid line TWICE, with a
//! gap between, and the table's own border a third time around them. So the
//! PRODUCER collapses, the way it collapses margins: for each grid line it
//! decides the one border CSS would draw there, and rewrites the table so
//! the separated model draws exactly that.
//!
//! The rewrite: `border-spacing: 0`; the table's and its rows' own borders
//! removed (they take part in the conflict, then are drawn by the cells);
//! every cell draws the winner of its END and BOTTOM grid lines, and of its
//! START and TOP lines only on the table's edge (or where no cell is on the
//! other side). Each line is drawn once, by one cell.
//!
//! The conflict (§17.6.2.1), per grid line segment:
//! 1. `hidden` anywhere suppresses the line;
//! 2. otherwise `none` loses, and the WIDER border wins;
//! 3. then by style: double, solid, dashed, dotted, ridge, outset, groove,
//!    inset (`double`…`inset` arrive repaired to `solid`, so only solid >
//!    dashed > dotted can be told apart);
//! 4. then by source: cell, row, row group, column, column group, table
//!    (rows, cells and the table take part; column and row group borders
//!    are counted, not used);
//! 5. then the one further to the start (left in an ltr table) and top.
//!
//! What is not exact, stated: CSS centres a collapsed border on its grid
//! line, half in each cell; here the whole width is inside one cell, so
//! content beside a 1 px border moves by half a pixel. A cell spanning rows
//! (or columns) gets ONE winner for its whole side — the strongest of its
//! segments (counted).

use crate::collapse::Resolved;
use crate::{Priority, Report};
use navigator_dom::{Dom, Handle, Kind};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
struct B {
    width: f64,
    style: String,
    w_text: String,
    color: String,
    /// cell 4, row 3, table 0.
    source: u8,
}

impl B {
    fn none() -> B { B { width: 0.0, style: "none".into(), w_text: "0px".into(), color: "currentColor".into(), source: 0 } }
    fn visible(&self) -> bool { self.style != "none" && self.style != "hidden" && self.width > 0.0 }
}

fn style_rank(s: &str) -> u8 {
    match s { "double" => 8, "solid" => 7, "dashed" => 6, "dotted" => 5, "ridge" => 4, "outset" => 3, "groove" => 2, "inset" => 1, _ => 0 }
}

/// A border width in px, as far as it can be read without layout: px, the
/// three keywords, `em` against 16 px (counted).
fn width_px(v: &str, approx: &mut usize) -> f64 {
    let v = v.trim();
    match v {
        "thin" => 1.0,
        "medium" => 3.0,
        "thick" => 5.0,
        "0" => 0.0,
        _ => {
            if let Some(n) = v.strip_suffix("px").and_then(|n| n.trim().parse::<f64>().ok()) { return n }
            for (u, k) in [("rem", 16.0), ("em", 16.0), ("pt", 4.0 / 3.0)] {
                if let Some(n) = v.strip_suffix(u).and_then(|n| n.trim().parse::<f64>().ok()) { *approx += 1; return n * k }
            }
            *approx += 1;
            1.0
        }
    }
}

/// The winner among `cands`, listed start/top first (rule 5: the first of
/// equals wins). `None` when a `hidden` suppresses the line.
fn winner(cands: &[B]) -> B {
    if cands.iter().any(|b| b.style == "hidden") { return B::none() }
    let mut best: Option<&B> = None;
    for b in cands.iter().filter(|b| b.visible()) {
        best = match best {
            None => Some(b),
            Some(a) => {
                let ka = (a.width, style_rank(&a.style), a.source);
                let kb = (b.width, style_rank(&b.style), b.source);
                if kb.partial_cmp(&ka) == Some(std::cmp::Ordering::Greater) { Some(b) } else { Some(a) }
            }
        };
    }
    best.cloned().unwrap_or_else(B::none)
}

struct View<'a> {
    resolved: &'a Resolved,
    dom: &'a Dom,
    media: String,
}

impl View<'_> {
    fn get(&self, h: Handle, p: &str) -> Option<&str> {
        let look = |m: &str| self.resolved.get(&(h, String::new(), m.to_string())).and_then(|pr| pr.get(p)).map(|(_, v)| v.as_str());
        if !self.media.is_empty() { if let Some(v) = look(&self.media) { return Some(v) } }
        look("")
    }
    fn display(&self, h: Handle) -> String {
        if let Some(d) = self.get(h, "display") { return d.to_string() }
        match self.dom.tag(h) { Some("table") => "table", Some("tr") => "table-row", Some("td" | "th") => "table-cell",
                                Some("thead" | "tbody" | "tfoot") => "table-row-group", _ => "block" }.to_string()
    }
    fn collapsed(&self, table: Handle) -> bool {
        let mut cur = Some(table);
        while let Some(h) = cur {
            match self.get(h, "border-collapse-marker") {
                Some("collapse") => return true,
                Some("inherit") | None => {}
                Some(_) => return false,
            }
            cur = self.dom.get(h).and_then(|n| n.parent).filter(|p| matches!(self.dom.get(*p).map(|n| &n.kind), Some(Kind::Element(_))));
        }
        false
    }
    fn border(&self, h: Handle, side: &str, source: u8, approx: &mut usize) -> B {
        let style = self.get(h, &format!("border-{side}-style")).unwrap_or("none").to_string();
        let w_text = self.get(h, &format!("border-{side}-width")).map(str::to_string)
            .unwrap_or_else(|| if style == "none" { "0px".into() } else { "medium".into() });
        let width = width_px(&w_text, approx);
        let color = self.get(h, &format!("border-{side}-color")).unwrap_or("currentColor").to_string();
        B { width, style, w_text, color, source }
    }
    /// Rows, flattened through groups, as the renderer walks them.
    fn rows(&self, table: Handle) -> Vec<Handle> {
        let mut rows = vec![];
        let mut stack: Vec<Handle> = self.dom.element_children(table).into_iter().rev().collect();
        while let Some(c) = stack.pop() {
            match self.display(c).as_str() {
                "table-row" => rows.push(c),
                "table" | "none" => {}
                _ => for g in self.dom.element_children(c).into_iter().rev() { stack.push(g) },
            }
        }
        rows
    }
}

fn span(dom: &Dom, h: Handle, attr: &str) -> usize {
    dom.attr(h, attr).and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n >= 1).unwrap_or(1).min(1000)
}

#[derive(Default)]
pub(crate) struct Counts {
    pub tables: usize,
    pub cells: usize,
    pub spans: usize,
    pub approx: usize,
    pub ignored_group_borders: usize,
}

/// Collapse every `border-collapse: collapse` table, per media context.
pub(crate) fn compile(resolved: &mut Resolved, dom: &Dom, rtl: &BTreeMap<Handle, bool>, report: &mut Report) {
    let any = resolved.iter().any(|((_, st, _), pr)| st.is_empty() && pr.get("border-collapse-marker").is_some_and(|(_, v)| v == "collapse"));
    let mut c = Counts::default();
    if any {
        let media: BTreeSet<String> = resolved.keys().filter(|(_, st, _)| st.is_empty()).map(|(_, _, m)| m.clone()).collect();
        let tables: Vec<Handle> = (0..dom.nodes.len() as Handle).filter(|h| dom.connected(*h) && matches!(dom.get(*h).map(|n| &n.kind), Some(Kind::Element(_)))).collect();
        let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
        for m in &media {
            let mut writes: Vec<(Handle, &'static str, String)> = vec![];
            {
                let v = View { resolved, dom, media: m.clone() };
                for &t in &tables {
                    if !matches!(v.display(t).as_str(), "table" | "inline-table") || !v.collapsed(t) { continue }
                    // In a media context, only tables something in that context touches.
                    if !m.is_empty() && !touches(resolved, dom, t, m) { continue }
                    collapse_table(&v, t, rtl.get(&t).copied().unwrap_or(false), &mut writes, &mut c);
                }
            }
            for (h, p, val) in writes {
                resolved.entry((h, String::new(), m.clone())).or_default().insert(p.to_string(), (pri, val));
            }
        }
    }
    // `hidden` was carried for the conflict; the profile draws it as `none`.
    for (_, props) in resolved.iter_mut() {
        props.remove("border-collapse-marker");
        for side in ["top", "right", "bottom", "left"] {
            if let Some((_, v)) = props.get_mut(&format!("border-{side}-style")) { if v == "hidden" { *v = "none".into() } }
        }
    }
    if c.tables > 0 { report.drop_n("collapsed table borders compiled to separated ones (CSS 2.1 §17.6.2)", c.tables) }
    if c.spans > 0 { report.drop_n("collapsed border of a spanning cell: one winner for the whole side", c.spans) }
    if c.approx > 0 { report.drop_n("collapsed table with a border width read without layout (em/pt/unknown)", c.approx) }
    if c.ignored_group_borders > 0 { report.drop_n("collapsed border on a column or row group (not in the conflict)", c.ignored_group_borders) }
}

/// Whether media context `m` declares anything on the table or in it.
fn touches(resolved: &Resolved, dom: &Dom, t: Handle, m: &str) -> bool {
    let mut stack = vec![t];
    while let Some(h) = stack.pop() {
        if resolved.contains_key(&(h, String::new(), m.to_string())) { return true }
        stack.extend(dom.element_children(h));
    }
    false
}

fn collapse_table(v: &View, t: Handle, rtl: bool, writes: &mut Vec<(Handle, &'static str, String)>, c: &mut Counts) {
    let dom = v.dom;
    let rows = v.rows(t);
    if rows.is_empty() { return }
    // Cells and their slots, as the renderer assigns them.
    let cells_of: Vec<Vec<Handle>> = rows.iter().map(|r| dom.element_children(*r).into_iter().filter(|x| v.display(*x) == "table-cell").collect()).collect();
    let nrows = rows.len();
    let mut busy: Vec<usize> = vec![];
    let mut placed: Vec<(Handle, usize, usize, usize, usize)> = vec![]; // cell, r0, c0, rs, cs
    for (r, cells) in cells_of.iter().enumerate() {
        let mut k = 0usize;
        for &cell in cells {
            while busy.get(k).is_some_and(|b| *b > 0) { k += 1 }
            let (n, rs) = (span(dom, cell, "colspan"), span(dom, cell, "rowspan").min(nrows - r));
            if busy.len() < k + n { busy.resize(k + n, 0) }
            for b in &mut busy[k..k + n] { *b = rs }
            placed.push((cell, r, k, rs, n));
            k += n;
        }
        for b in &mut busy { *b = b.saturating_sub(1) }
    }
    let ncols = placed.iter().map(|p| p.2 + p.4).max().unwrap_or(0);
    if ncols == 0 { return }
    let mut grid: Vec<Vec<Option<usize>>> = vec![vec![None; ncols]; nrows];
    for (i, &(_, r0, c0, rs, cs)) in placed.iter().enumerate() {
        for row in grid.iter_mut().skip(r0).take(rs) { for slot in row.iter_mut().skip(c0).take(cs) { *slot = Some(i) } }
    }
    // Column and row-group borders are not in the conflict: counted.
    for x in dom.element_children(t) {
        if matches!(dom.tag(x), Some("col" | "colgroup" | "thead" | "tbody" | "tfoot")) {
            for side in ["top", "right", "bottom", "left"] {
                if v.border(x, side, 1, &mut 0).visible() { c.ignored_group_borders += 1; break }
            }
        }
    }
    // Logical sides: START is left in ltr, right in rtl.
    let (start, end) = if rtl { ("right", "left") } else { ("left", "right") };
    let cell_b = |i: usize, side: &str, approx: &mut usize| v.border(placed[i].0, side, 4, approx);
    let row_b = |r: usize, side: &str, approx: &mut usize| v.border(rows[r], side, 3, approx);
    let table_b = |side: &str, approx: &mut usize| v.border(t, side, 0, approx);
    let mut approx = 0usize;
    for (i, &(cell, r0, c0, rs, cs)) in placed.iter().enumerate() {
        // END line: per row segment, this cell's end, the next cell's start,
        // and at the table's end edge the row's and the table's end borders.
        let mut seg = vec![];
        for r in r0..r0 + rs {
            let mut cands = vec![];
            // Rule 5: the one further to the start wins ties — this cell.
            cands.push(cell_b(i, end, &mut approx));
            match grid[r].get(c0 + cs).copied().flatten() {
                Some(j) => cands.push(cell_b(j, start, &mut approx)),
                None => { cands.push(row_b(r, end, &mut approx)); cands.push(table_b(end, &mut approx)) }
            }
            seg.push(winner(&cands));
        }
        let end_w = winner(&seg);
        // BOTTOM line: per column segment.
        let mut seg = vec![];
        for col in c0..c0 + cs {
            let mut cands = vec![cell_b(i, "bottom", &mut approx), row_b(r0 + rs - 1, "bottom", &mut approx)];
            match grid.get(r0 + rs).and_then(|row| row[col]) {
                Some(j) => { cands.push(row_b(r0 + rs, "top", &mut approx)); cands.push(cell_b(j, "top", &mut approx)) }
                None => cands.push(table_b("bottom", &mut approx)),
            }
            seg.push(winner(&cands));
        }
        let bottom_w = winner(&seg);
        // START line: only where nothing is on the other side.
        let start_w = if c0 == 0 || (r0..r0 + rs).any(|r| grid[r][c0 - 1].is_none()) {
            let mut seg = vec![];
            for r in r0..r0 + rs {
                let mut cands = vec![];
                if c0 == 0 { cands.push(table_b(start, &mut approx)); cands.push(row_b(r, start, &mut approx)) }
                cands.push(cell_b(i, start, &mut approx));
                seg.push(winner(&cands));
            }
            winner(&seg)
        } else { B::none() };
        // TOP line: likewise.
        let top_w = if r0 == 0 || (c0..c0 + cs).any(|col| grid[r0 - 1][col].is_none()) {
            let mut seg = vec![];
            for _ in c0..c0 + cs {
                let mut cands = vec![];
                if r0 == 0 { cands.push(table_b("top", &mut approx)) }
                cands.push(row_b(r0, "top", &mut approx));
                cands.push(cell_b(i, "top", &mut approx));
                seg.push(winner(&cands));
            }
            winner(&seg)
        } else { B::none() };
        if rs > 1 || cs > 1 { c.spans += 1 }
        for (side, b) in [(end, end_w), ("bottom", bottom_w), (start, start_w), ("top", top_w)] {
            let (wp, sp, cp): (&'static str, &'static str, &'static str) = match side {
                "top" => ("border-top-width", "border-top-style", "border-top-color"),
                "right" => ("border-right-width", "border-right-style", "border-right-color"),
                "bottom" => ("border-bottom-width", "border-bottom-style", "border-bottom-color"),
                _ => ("border-left-width", "border-left-style", "border-left-color"),
            };
            if b.visible() {
                writes.push((cell, wp, b.w_text.clone()));
                writes.push((cell, sp, b.style.clone()));
                writes.push((cell, cp, b.color.clone()));
            } else {
                writes.push((cell, wp, "0px".into()));
                writes.push((cell, sp, "none".into()));
            }
        }
        c.cells += 1;
    }
    // The table's and the rows' own borders were in the conflict; the cells
    // draw the result.
    for h in std::iter::once(t).chain(rows.iter().copied()) {
        for (wp, sp) in [("border-top-width", "border-top-style"), ("border-right-width", "border-right-style"),
                         ("border-bottom-width", "border-bottom-style"), ("border-left-width", "border-left-style")] {
            writes.push((h, wp, "0px".into()));
            writes.push((h, sp, "none".into()));
        }
    }
    writes.push((t, "border-spacing", "0px 0px".into()));
    // A collapsed table has no padding (§17.6.2).
    for pp in ["padding-top", "padding-right", "padding-bottom", "padding-left"] { writes.push((t, pp, "0px".into())); }
    if approx > 0 { c.approx += 1 }
    c.tables += 1;
}
