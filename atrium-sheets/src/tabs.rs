//! Sheet tabs: a strip under the grid naming the workbook's sheets, a press
//! on one shows it (officina.md §16.71). Only the tabs that fit are measured
//! and drawn, so a workbook of thousands of sheets costs what one of a few
//! does; ‹ and › move along them.

use officina_grid::Workbook;
use pergola::theme::{font, type_size, Weight};
use pergola::{Color, TextStyle};

/// The strip's height, above the status line.
pub const TABS_H: f32 = 28.0;
/// Where the first tab starts: after the add, earlier and later buttons.
pub const TABS_X: f32 = 92.0;
const TAB_MIN: f32 = 56.0;
const TAB_MAX: f32 = 200.0;
const TAB_PAD: f32 = 24.0;

/// A press on the strip or its rename bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabCmd {
    /// The Paste Special bar: what to paste (0 all, 1 values, 2 formulas,
    /// 3 formats, 4 comments), the operation (0 none, 1 add, 2 subtract,
    /// 3 multiply, 4 divide), skipping blanks, turning; paste or close.
    PasteWhat(u8),
    PasteOp(u8),
    PasteSkip,
    PasteTurn,
    PasteDone,
    PasteCancel,
    /// Show this sheet.
    Go(u16),
    /// A new sheet at the end, shown.
    Add,
    /// Move the strip a tab back or on.
    Earlier,
    Later,
    /// The sheet on screen deleted, or moved a place left or right.
    Delete,
    MoveLeft,
    MoveRight,
    /// The rename bar for the sheet on screen; its field submitted; closed.
    Rename,
    RenameDone,
    RenameCancel,
    /// The tab colour bar for the sheet on screen; a colour picked from it
    /// (None takes the colour off); closed.
    Coloring,
    Color(Option<u32>),
    ColorCancel,
    /// The comment bar for the cursor's cell; its field submitted; closed.
    Commenting,
    CommentDone,
    CommentCancel,
    /// The link bar for the cursor's cell; its field submitted; closed.
    Linking,
    LinkDone,
    LinkCancel,
    /// The exact-size bar's field submitted; closed.
    SizeDone,
    SizeCancel,
    /// The value a validation rule warned about: kept, or taken back.
    WarnKeep,
    WarnUndo,
}

/// The colours the tab colour bar offers, 0xRRGGBB, with their names.
pub const COLORS: [(u32, &str); 8] = [
    (0xC62828, "Red"),
    (0xEF6C00, "Orange"),
    (0xF9A825, "Yellow"),
    (0x2E7D32, "Green"),
    (0x00838F, "Teal"),
    (0x1565C0, "Blue"),
    (0x6A1B9A, "Purple"),
    (0x616161, "Grey"),
];

/// 0xRRGGBB as a Pergola colour.
pub fn color(rgb: u32) -> Color {
    let c = |sh: u32| ((rgb >> sh) & 0xFF) as f32 / 255.0;
    Color::rgba(c(16), c(8), c(0), 1.0)
}

/// The style tab names are drawn in.
pub fn style(color: Color, on: bool) -> TextStyle {
    TextStyle { family: font::SANS.into(), size: type_size::SM, weight: if on { Weight::Semibold } else { Weight::Regular }, color }
}

/// A tab's width for its sheet's name.
pub fn width(name: &str) -> f32 {
    let w = pergola::text::measure(name, &style(Color::TRANSPARENT, true)).w;
    (w + TAB_PAD).clamp(TAB_MIN, TAB_MAX)
}

/// The tabs that fit in `room` from sheet `first` on: (sheet, x from the
/// strip's first tab, width).
pub fn visible(wb: &Workbook, first: u16, room: f32) -> Vec<(u16, f32, f32)> {
    let mut out = Vec::new();
    let mut x = 0.0;
    for s in first as usize..wb.sheet_count() {
        let w = width(&wb.sheet_name(s as u16));
        if x + w > room && !out.is_empty() {
            break;
        }
        out.push((s as u16, x, w));
        x += w;
    }
    out
}

/// The first tab to draw so that sheet `s` shows: `first` if it already
/// does, else the strip moved just far enough.
pub fn reveal(wb: &Workbook, first: u16, s: u16, room: f32) -> u16 {
    if s < first {
        return s;
    }
    if visible(wb, first, room).iter().any(|t| t.0 == s) {
        return first;
    }
    // Back from `s` while the tabs still fit.
    let mut x = width(&wb.sheet_name(s));
    let mut f = s;
    while f > 0 {
        let w = width(&wb.sheet_name(f - 1));
        if x + w > room {
            break;
        }
        x += w;
        f -= 1;
    }
    f
}

/// A name for a new sheet: "SheetN" with the first N not taken.
pub fn new_name(wb: &Workbook) -> String {
    (wb.sheet_count() + 1..).map(|n| format!("Sheet{n}")).find(|n| wb.sheet_id(n).is_none()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_tabs_that_fit_are_laid_out() {
        let mut wb = Workbook::new();
        for i in 1..=4000 {
            wb.add_sheet(&format!("Sheet{i}")).unwrap();
        }
        let t = std::time::Instant::now();
        let v = visible(&wb, 0, 800.0);
        assert!(v.len() < 20 && v.len() > 5, "{}", v.len());
        assert!(v.last().map_or(0.0, |t| t.1 + t.2) <= 800.0);
        let f = reveal(&wb, 0, 3999, 800.0);
        assert!(visible(&wb, f, 800.0).iter().any(|t| t.0 == 3999));
        assert!(f > 3980);
        assert_eq!(reveal(&wb, f, 3995, 800.0), f);
        assert_eq!(reveal(&wb, f, f - 1, 800.0), f - 1);
        assert_eq!(reveal(&wb, f, 10, 800.0), 10);
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "{:?}", t.elapsed());
        assert_eq!(new_name(&wb), "Sheet4001");
    }
}
