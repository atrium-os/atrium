//! What-If scenarios (docs/spec/officina.md §16.142): named sets of values
//! for a sheet's changing cells, kept with the sheet as Excel keeps them.
//! Showing one types its values into the cells; the list itself changes
//! only through [`crate::ops::GridOp::Scenarios`], so it undoes.

use officina_core::wire::{Reader, WireError, Writer};

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::shift::Shift;

/// Excel's limits: 32 changing cells per scenario; we also bound the list.
pub const MAX_SCENARIOS: usize = 256;
pub const MAX_CELLS: usize = 32;
pub const MAX_NAME: usize = 255;
pub const MAX_COMMENT: usize = 1024;
/// The longest value text a changing cell keeps.
pub const MAX_VALUE: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub name: String,
    pub comment: String,
    /// (row, col, value as typed), the cells distinct.
    pub cells: Vec<(u32, u32, String)>,
}

impl Scenario {
    /// Within the limits, a non-empty name, at least one cell, no cell twice.
    pub fn valid(&self) -> bool {
        let n = self.name.trim();
        !n.is_empty()
            && self.name.len() <= MAX_NAME
            && self.comment.len() <= MAX_COMMENT
            && (1..=MAX_CELLS).contains(&self.cells.len())
            && self.cells.iter().all(|(r, c, v)| *r < MAX_ROWS && *c < MAX_COLS && v.len() <= MAX_VALUE)
            && self.cells.iter().enumerate().all(|(i, a)| self.cells[..i].iter().all(|b| (a.0, a.1) != (b.0, b.1)))
    }
}

/// A list of scenarios: each valid, names distinct without regard to case.
pub fn valid_list(list: &[Scenario]) -> bool {
    list.len() <= MAX_SCENARIOS
        && list.iter().all(Scenario::valid)
        && list.iter().enumerate().all(|(i, a)| list[..i].iter().all(|b| !b.name.trim().eq_ignore_ascii_case(a.name.trim())))
}

pub fn encode(list: &[Scenario], w: &mut Writer) {
    w.var(list.len() as u64);
    for s in list {
        w.str(&s.name);
        w.str(&s.comment);
        w.var(s.cells.len() as u64);
        for (r, c, v) in &s.cells {
            w.u32(*r);
            w.u32(*c);
            w.str(v);
        }
    }
}

pub fn decode(r: &mut Reader) -> Result<Vec<Scenario>, WireError> {
    let n = r.count("scenarios", MAX_SCENARIOS as u64, 3)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.str("scenario name", MAX_NAME as u64)?.to_string();
        let comment = r.str("scenario comment", MAX_COMMENT as u64)?.to_string();
        let k = r.count("scenario cells", MAX_CELLS as u64, 9)?;
        let mut cells = Vec::with_capacity(k);
        for _ in 0..k {
            let (row, col) = (r.u32()?, r.u32()?);
            cells.push((row, col, r.str("scenario value", MAX_VALUE as u64)?.to_string()));
        }
        list.push(Scenario { name, comment, cells });
    }
    if !valid_list(&list) {
        return Err(WireError::Invalid("scenarios"));
    }
    Ok(list)
}

/// The list after rows or columns move: cells follow, deleted cells leave
/// their scenario, and a scenario left with none goes.
pub(crate) fn shifted(list: &[Scenario], s: Shift) -> Vec<Scenario> {
    list.iter().filter_map(|sc| {
        let cells: Vec<(u32, u32, String)> = sc.cells.iter().filter_map(|(r, c, v)| s.map_cell(*r, *c).map(|(r, c)| (r, c, v.clone()))).collect();
        (!cells.is_empty()).then(|| Scenario { cells, ..sc.clone() })
    }).collect()
}
