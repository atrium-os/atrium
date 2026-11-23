//! Editing restrictions and opaque settings from protected foreign sheets.

use crate::addr::{CellAddr, Rect};
use crate::book::Workbook;
use officina_core::wire::{Reader, WireError, Writer};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectionFormat {
    Excel,
    Ods,
}

impl ProtectionFormat {
    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Excel => &[
                "password",
                "algorithmName",
                "hashValue",
                "saltValue",
                "spinCount",
                "objects",
                "scenarios",
                "formatCells",
                "formatRows",
                "formatColumns",
                "insertColumns",
                "insertRows",
                "insertHyperlinks",
                "deleteColumns",
                "deleteRows",
                "selectLockedCells",
                "sort",
                "autoFilter",
                "pivotTables",
                "selectUnlockedCells",
            ],
            Self::Ods => &[
                "table:protection-key",
                "table:protection-key-digest-algorithm",
                "loext:protection-key-digest-algorithm-2",
            ],
        }
    }
}

const MAX_SETTING: usize = 2048;

/// The sheet lock is enforced by Sheets. Foreign verifier and permission
/// settings are opaque and only written back to their original format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SheetProtection {
    format: ProtectionFormat,
    settings: BTreeMap<String, String>,
}

impl SheetProtection {
    pub fn new(format: ProtectionFormat) -> Self {
        Self {
            format,
            settings: BTreeMap::new(),
        }
    }

    /// Unknown properties are ignored. Returns false for an oversized known property.
    pub fn keep(&mut self, key: &str, value: &str) -> bool {
        if !self.format.keys().contains(&key) {
            return true;
        }
        if value.len() > MAX_SETTING {
            return false;
        }
        self.settings.insert(key.into(), value.into());
        true
    }

    pub fn format(&self) -> ProtectionFormat {
        self.format
    }

    pub fn settings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.settings
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        w.u8(match self.format {
            ProtectionFormat::Excel => 0,
            ProtectionFormat::Ods => 1,
        });
        w.var(self.settings.len() as u64);
        for (key, value) in self.settings() {
            w.str(key);
            w.str(value);
        }
    }

    pub(crate) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let format = match r.u8()? {
            0 => ProtectionFormat::Excel,
            1 => ProtectionFormat::Ods,
            _ => return Err(WireError::Invalid("sheet protection format")),
        };
        let count = r.count("sheet protection settings", format.keys().len() as u64, 2)?;
        let mut protection = Self::new(format);
        for _ in 0..count {
            let key = r.str("sheet protection property", 64)?;
            let value = r.str("sheet protection setting", MAX_SETTING as u64)?;
            if !format.keys().contains(&key) || protection.settings.contains_key(key) {
                return Err(WireError::Invalid("sheet protection property"));
            }
            protection.keep(key, value);
        }
        Ok(protection)
    }
}

impl Workbook {
    pub fn sheet_protected(&self, sheet: u16) -> bool {
        self.sheets
            .get(sheet as usize)
            .is_some_and(|sheet| sheet.protection.is_some())
    }

    pub fn any_sheet_protected(&self) -> bool {
        self.sheets.iter().any(|sheet| sheet.protection.is_some())
    }

    /// A protected sheet's default style is locked. Check runs, including
    /// explicitly unlocked blank cells, without walking every cell.
    pub fn can_edit_cells(&self, rect: Rect) -> bool {
        if !self.sheet_protected(rect.sheet) {
            return true;
        }
        let sheet = self.sheet(rect.sheet);
        (rect.c0..=rect.c1).all(|col| {
            sheet.styles.get(col as usize).is_some_and(|style| {
                style
                    .spans(rect.r0, rect.r1)
                    .iter()
                    .all(|&(_, _, id)| self.styles.get(id).unlocked)
            })
        })
    }

    pub fn hides_value(&self, cell: CellAddr) -> bool {
        self.sheet_protected(cell.sheet) && self.style(cell).hide_value
    }

    pub fn hides_formula(&self, cell: CellAddr) -> bool {
        self.sheet_protected(cell.sheet) && self.style(cell).hide_formula
    }
}
