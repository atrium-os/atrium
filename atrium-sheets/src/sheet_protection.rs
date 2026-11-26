use officina_grid::ops::GridOp;
use officina_grid::Workbook;

pub(crate) const REFUSED: &str = "This sheet is protected. Only unlocked cells can be edited.";

pub(crate) const STRUCTURE_REFUSED: &str = "The workbook's structure is protected: sheets can't be added, deleted, renamed, moved, hidden or recoloured.";

/// Whether `status` says an edit was refused for protection.
pub(crate) fn refused(status: &str) -> bool {
    status == REFUSED || status == STRUCTURE_REFUSED
}

/// Check a whole transaction before applying any part of it. Metadata and
/// structural changes use the conservative sheet protection policy.
pub(crate) fn allows(wb: &Workbook, ops: &[GridOp]) -> bool {
    refusal(wb, ops).is_none()
}

/// Why the transaction is refused, if it is.
pub(crate) fn refusal(wb: &Workbook, ops: &[GridOp]) -> Option<&'static str> {
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

fn allows_op(wb: &Workbook, op: &GridOp) -> bool {
    match op {
        GridOp::Replace(region) => wb.can_edit_cells(region.rect),
        GridOp::Batch(ops) => ops.iter().all(|op| allows_op(wb, op)),
        GridOp::Restyle(style) => !wb.sheet_protected(style.rect.sheet),
        GridOp::Filter(state) => !wb.sheet_protected(state.sheet),
        // A calculation setting of the workbook, not a sheet's contents.
        GridOp::Iteration(_) | GridOp::Structure(_) => true,
        GridOp::Sizes { sheet, .. }
        | GridOp::LayoutSizes { sheet, .. }
        | GridOp::RenameSheet { sheet, .. }
        | GridOp::TabColor { sheet, .. }
        | GridOp::HideSheet { sheet, .. }
        | GridOp::Merges { sheet, .. }
        | GridOp::Comments { sheet, .. }
        | GridOp::Links { sheet, .. }
        | GridOp::HiddenWidths { sheet, .. }
        | GridOp::AxisIdentities { sheet, .. } | GridOp::Outline { sheet, .. } | GridOp::Scenarios { sheet, .. } | GridOp::Sparklines { sheet, .. } => !wb.sheet_protected(*sheet),
        GridOp::Tables(tables) => tables
            .iter()
            .filter(|t| wb.sheet_protected(t.rect.sheet))
            .eq(wb
                .tables()
                .iter()
                .filter(|t| wb.sheet_protected(t.rect.sheet))),
        GridOp::Charts(charts) => charts
            .iter()
            .filter(|ch| wb.sheet_protected(ch.anchor.sheet))
            .eq(wb
                .charts()
                .iter()
                .filter(|ch| wb.sheet_protected(ch.anchor.sheet))),
        GridOp::Pictures(pictures) => pictures
            .iter()
            .filter(|p| wb.sheet_protected(p.anchor.sheet))
            .eq(wb
                .pictures()
                .iter()
                .filter(|p| wb.sheet_protected(p.anchor.sheet))),
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
