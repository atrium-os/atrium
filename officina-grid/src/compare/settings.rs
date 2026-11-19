//! Worksheet settings use stored sheet coordinates in both copies.
use super::Change;
use crate::{filter::FilterState, ops::GridOp, Workbook};

#[derive(Debug, Clone, PartialEq)]
enum Setting {
    Tab(Option<u32>),
    Freeze { end: (u32, u32), from: (u32, u32) },
    Visibility(FilterState),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingChange {
    pub(super) sheet: u16,
    name: String,
    before: Setting,
    after: Setting,
}

impl Setting {
    fn tab(wb: &Workbook, s: u16, _: u16) -> Self {
        Self::Tab(wb.sheet(s).tab_color)
    }

    fn freeze(wb: &Workbook, s: u16, _: u16) -> Self {
        let sh = wb.sheet(s);
        Self::Freeze {
            end: sh.freeze,
            from: sh.freeze_from,
        }
    }

    fn visibility(wb: &Workbook, s: u16, local: u16) -> Self {
        let mut state = wb.filter_state(s);
        state.sheet = local;
        if let Some(f) = &mut state.filter {
            f.rect.sheet = local;
        }
        Self::Visibility(state)
    }
}

pub(super) fn compare(
    ours: &Workbook,
    s: u16,
    theirs: &Workbook,
    t: u16,
    out: &mut Vec<Change>,
) -> bool {
    for read in [
        Setting::tab as fn(&Workbook, u16, u16) -> Setting,
        Setting::freeze,
        Setting::visibility,
    ] {
        let before = read(ours, s, s);
        let after = read(theirs, t, s);
        if before != after {
            out.push(Change::Setting(SettingChange {
                sheet: s,
                name: ours.sheet_name(s),
                before,
                after,
            }));
            if out.len() >= super::MAX_CHANGES {
                return true;
            }
        }
    }
    false
}

impl SettingChange {
    pub(super) fn reorder(&mut self, sheet: u16) {
        self.sheet = sheet;
        for setting in [&mut self.before, &mut self.after] {
            if let Setting::Visibility(state) = setting {
                state.sheet = sheet;
                if let Some(f) = &mut state.filter { f.rect.sheet = sheet; }
            }
        }
    }

    pub(super) fn describe(&self) -> String {
        let detail = match &self.after {
            Setting::Tab(Some(rgb)) => format!("Tab color → #{rgb:06X}"),
            Setting::Tab(None) => "Tab color cleared".into(),
            Setting::Freeze { end: (0, 0), .. } => "Frozen panes cleared".into(),
            Setting::Freeze {
                end: (rows, cols),
                from,
            } => format!(
                "Frozen panes from {} · {} rows, {} columns",
                crate::addr::a1(from.0, from.1),
                rows - from.0,
                cols - from.1
            ),
            Setting::Visibility(v) => format!(
                "Auto-filter {}, {} hidden rows",
                if v.filter.is_some() { "on" } else { "off" },
                v.hidden.iter().map(|(a, b)| b - a + 1).sum::<u32>()
            ),
        };
        format!("{} · {detail}", self.name)
    }

    pub(super) fn take(&self, wb: &Workbook) -> Option<Vec<GridOp>> {
        let sheet = self.sheet;
        if wb.sheet_id(&self.name) != Some(sheet) {
            return None;
        }
        let (current, op) = match &self.after {
            Setting::Tab(color) => (
                Setting::tab(wb, sheet, sheet),
                GridOp::TabColor {
                    sheet,
                    color: *color,
                },
            ),
            Setting::Freeze {
                end: (rows, cols),
                from,
            } => (
                Setting::freeze(wb, sheet, sheet),
                GridOp::Freeze {
                    sheet,
                    rows: *rows,
                    cols: *cols,
                    from: *from,
                },
            ),
            Setting::Visibility(state) => (
                Setting::visibility(wb, sheet, sheet),
                GridOp::Filter(state.clone()),
            ),
        };
        (current == self.before).then_some(vec![op])
    }
}
