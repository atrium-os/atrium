#[cfg(feature = "read")]
use crate::report::{Kind, Report};
#[cfg(feature = "read")]
use officina_grid::protection::{ProtectionFormat, SheetProtection};

#[cfg(feature = "read")]
pub fn read<'a>(
    format: ProtectionFormat,
    attrs: impl IntoIterator<Item = (&'a str, &'a str)>,
    report: &mut Report,
) -> SheetProtection {
    let mut protection = SheetProtection::new(format);
    for (key, value) in attrs {
        if !protection.keep(key, value) {
            report.note(Kind::Dropped, "oversized sheet protection settings");
        }
    }
    notice(&protection, report);
    protection
}

#[cfg(feature = "read")]
fn notice(protection: &SheetProtection, report: &mut Report) {
    let mut password = false;
    let mut permissions = false;
    for (key, value) in protection.settings() {
        match key {
            "password" | "hashValue" | "table:protection-key" => password |= !value.is_empty(),
            "formatCells" | "formatRows" | "formatColumns" | "insertRows" | "insertColumns"
            | "insertHyperlinks" | "deleteRows" | "deleteColumns" | "sort" | "autoFilter"
            | "pivotTables" | "objects" | "scenarios" => {
                permissions |= matches!(value, "0" | "false")
            }
            "selectLockedCells" | "selectUnlockedCells" => {
                permissions |= matches!(value, "1" | "true")
            }
            _ => {}
        }
    }
    if password {
        report.note(Kind::Approximate, "sheet protection passwords kept for the source format. Sheets cannot remove these locks");
    }
    if permissions {
        report.note(Kind::Approximate, "custom sheet protection permissions kept for the source format. Sheets allows edits to unlocked cells only");
    }
}
