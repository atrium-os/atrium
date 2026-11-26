use officina_grid::ops::GridOp;
use officina_grid::Workbook;

pub(crate) const REFUSED: &str = "This sheet is protected. Only unlocked cells can be edited.";

pub(crate) const STRUCTURE_REFUSED: &str = "The workbook's structure is protected: sheets can't be added, deleted, renamed, moved, hidden or recoloured.";

pub(crate) const FINAL_REFUSED: &str = "This workbook is marked as final. Turn off Mark as Final to edit it.";

/// Whether `status` says an edit was refused for protection.
pub(crate) fn refused(status: &str) -> bool {
    status == REFUSED || status == STRUCTURE_REFUSED || status == FINAL_REFUSED
}

/// Check a whole transaction before applying any part of it. Metadata and
/// structural changes use the conservative sheet protection policy.
pub(crate) fn allows(wb: &Workbook, ops: &[GridOp]) -> bool {
    refusal(wb, ops).is_none()
}

/// Why the transaction is refused, if it is.
pub(crate) fn refusal(wb: &Workbook, ops: &[GridOp]) -> Option<&'static str> {
    // A final workbook takes only a change of its properties (which is how
    // the mark comes off).
    if wb.properties().marked_final && !ops.iter().all(|op| matches!(op, GridOp::Properties(_))) {
        return Some(FINAL_REFUSED);
    }
    if wb.structure_protected() && ops.iter().any(changes_structure) {
        return Some(STRUCTURE_REFUSED);
    }
    (wb.any_sheet_protected() && !ops.iter().all(|op| allows_op(wb, op))).then_some(REFUSED)
}

fn changes_structure(op: &GridOp) -> bool {
    match op {
        GridOp::Batch(ops) => ops.iter().any(changes_structure),
        GridOp::AddSheet(_) | GridOp::AddIdentifiedSheet { .. } | GridOp::DropSheet | GridOp::MoveSheet { .. }
        | GridOp::RenameSheet { .. } | GridOp::HideSheet { .. } | GridOp::TabColor { .. } => true,
        _ => false,
    }
}

/// Whether sheet `sheet` is open to `what`: unprotected, or protected with
/// that Excel allowance granted.
fn open_to(wb: &Workbook, sheet: u16, what: &str) -> bool {
    wb.sheet(sheet).protection.as_ref().is_none_or(|p| p.allows(what))
}

fn allows_op(wb: &Workbook, op: &GridOp) -> bool {
    if (op_sheet(op)).is_some_and(|s| s as usize >= wb.sheet_count()) {
        return false;
    }
    match op {
        GridOp::Replace(region) => wb.can_edit_cells(region.rect),
        GridOp::Batch(ops) => ops.iter().all(|op| allows_op(wb, op)),
        GridOp::Restyle(style) => open_to(wb, style.rect.sheet, "formatCells"),
        GridOp::Filter(state) => filter_allowed(wb, state),
        // Taking protection off asks for its password first (the command's job).
        GridOp::Protection { .. } => true,
        GridOp::Sizes { sheet, rows, .. } | GridOp::LayoutSizes { sheet, rows, .. } => open_to(wb, *sheet, if *rows { "formatRows" } else { "formatColumns" }),
        GridOp::HiddenWidths { sheet, .. } => open_to(wb, *sheet, "formatColumns"),
        GridOp::Scenarios { sheet, .. } => open_to(wb, *sheet, "scenarios"),
        GridOp::Comments { sheet, .. } => open_to(wb, *sheet, "objects"),
        GridOp::Links { sheet, .. } => open_to(wb, *sheet, "insertHyperlinks"),
        // A calculation setting of the workbook, not a sheet's contents.
        GridOp::Iteration(_) | GridOp::Structure(_) | GridOp::Calculation(_) => true,
        // The document's properties, not a sheet's contents.
        GridOp::Properties(_) | GridOp::DateSystem1904(_) => true,
        // Named views of the workbook, not a sheet's contents.
        GridOp::CustomViews(_) => true,
        // What the Watch Window shows is no part of the sheet's contents.
        GridOp::Watches { .. } => true,
        // Error Checking's view, not the sheet's contents.
        GridOp::IgnoredErrors { .. } => true,
        // Who may change what is fixed while the sheet is protected.
        GridOp::EditRanges { sheet, .. } => !wb.sheet_protected(*sheet),
        GridOp::RenameSheet { sheet, .. }
        | GridOp::TabColor { sheet, .. }
        | GridOp::HideSheet { sheet, .. }
        | GridOp::Merges { sheet, .. }
        | GridOp::AxisIdentities { sheet, .. } | GridOp::Outline { sheet, .. } | GridOp::Sparklines { sheet, .. } => !wb.sheet_protected(*sheet),
        GridOp::Tables(tables) => tables
            .iter()
            .filter(|t| wb.sheet_protected(t.rect.sheet))
            .eq(wb
                .tables()
                .iter()
                .filter(|t| wb.sheet_protected(t.rect.sheet))),
        GridOp::Charts(charts) => charts
            .iter()
            .filter(|ch| !open_to(wb, ch.anchor.sheet, "objects"))
            .eq(wb
                .charts()
                .iter()
                .filter(|ch| !open_to(wb, ch.anchor.sheet, "objects"))),
        // The theme and table styles are the workbook's; shapes and cameras
        // on a protected sheet are its objects.
        GridOp::Look(look) => {
            let fixed = |s: u16| !open_to(wb, s, "objects");
            let now = wb.workbook_look();
            look.shapes.iter().filter(|x| fixed(x.anchor.sheet)).eq(now.shapes.iter().filter(|x| fixed(x.anchor.sheet)))
                && look.cameras.iter().filter(|x| fixed(x.anchor.sheet)).eq(now.cameras.iter().filter(|x| fixed(x.anchor.sheet)))
        }
        GridOp::Pictures(pictures) => pictures
            .iter()
            .filter(|p| !open_to(wb, p.anchor.sheet, "objects"))
            .eq(wb
                .pictures()
                .iter()
                .filter(|p| !open_to(wb, p.anchor.sheet, "objects"))),
        GridOp::CondFormats(formats) => wb.decode_cond_formats(formats).is_ok_and(|formats| {
            formats.iter().filter(|f| wb.sheet_protected(f.sheet)).eq(wb
                .cond_formats()
                .iter()
                .filter(|f| wb.sheet_protected(f.sheet)))
        }),
        GridOp::Validations(rules) => wb.decode_validations(rules).is_ok_and(|rules| {
            rules
                .iter()
                .filter(|rule| wb.sheet_protected(rule.sheet))
                .eq(wb
                    .validations()
                    .iter()
                    .filter(|rule| wb.sheet_protected(rule.sheet)))
        }),
        GridOp::AddSheet(_) | GridOp::AddIdentifiedSheet { .. } | GridOp::Freeze { .. } => true,
        GridOp::Pivots(_) | GridOp::Names(_) | GridOp::DropSheet | GridOp::MoveSheet { .. } => {
            false
        }
    }
}

/// The sheet a per-sheet op names, to check it exists before asking about it.
fn op_sheet(op: &GridOp) -> Option<u16> {
    match op {
        GridOp::Restyle(style) => Some(style.rect.sheet),
        GridOp::Filter(state) => Some(state.sheet),
        GridOp::Sizes { sheet, .. } | GridOp::LayoutSizes { sheet, .. } | GridOp::HiddenWidths { sheet, .. } | GridOp::Scenarios { sheet, .. }
        | GridOp::Comments { sheet, .. } | GridOp::Links { sheet, .. } => Some(*sheet),
        _ => None,
    }
}

/// Hidden rows travel with the AutoFilter in one op: hiding rows by hand
/// needs `formatRows`; using an AutoFilter already there (its range kept,
/// rows outside it as they were) needs `autoFilter`. Adding or taking away
/// a filter needs both.
fn filter_allowed(wb: &Workbook, state: &officina_grid::filter::FilterState) -> bool {
    let sh = wb.sheet(state.sheet);
    let Some(p) = sh.protection.as_ref() else { return true };
    let area = |f: &Option<officina_grid::filter::AutoFilter>| f.as_ref().map(|f| f.rect);
    let same_filter = state.filter == sh.filter;
    if same_filter && p.allows("formatRows") {
        return true;
    }
    let Some(rect) = area(&sh.filter).filter(|r| area(&state.filter) == Some(*r)) else {
        return p.allows("formatRows") && p.allows("autoFilter");
    };
    // Rows outside the filter's body, which only formatRows may change.
    let outside = |v: &[(u32, u32)]| -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for &(a, b) in v {
            if a <= rect.r0 {
                out.push((a, b.min(rect.r0)));
            }
            if b > rect.r1 {
                out.push((a.max(rect.r1 + 1), b));
            }
        }
        out
    };
    p.allows("autoFilter") && outside(&state.hidden) == outside(&sh.hidden)
}
