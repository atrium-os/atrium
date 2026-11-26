//! Two lists of areas kept with a sheet (docs/spec/officina.md §16.146), as
//! Excel keeps them: Allow Edit Ranges (`<protectedRanges>`), the cells
//! people may change while the sheet is protected, and ignored errors
//! (`<ignoredErrors>`), the cells Error Checking leaves alone for a rule.
//! Each list changes only through its [`crate::ops::GridOp`], so it undoes.

use officina_core::wire::{Reader, WireError, Writer};

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::shift::Shift;

/// The most edit ranges a sheet keeps, the most areas one covers, and the
/// longest title.
pub const MAX_EDIT_RANGES: usize = 1024;
pub const MAX_AREAS: usize = 256;
pub const MAX_TITLE: usize = 255;
/// The most ignored-error areas a sheet keeps.
pub const MAX_IGNORED: usize = 4096;

/// One of Allow Edit Ranges: a title and the areas it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRange {
    pub title: String,
    /// (r0, c0, r1, c1), each on the sheet.
    pub areas: Vec<(u32, u32, u32, u32)>,
    /// Its password's verifier, as Excel's attributes (`algorithmName=…`
    /// lines, or the old `password=` hash): the range opens only once
    /// unlocked by the password ([`crate::Workbook::unlock_ranges`]).
    pub password: Option<String>,
}

/// The Error Checking rules a cell can be told to ignore: Excel's
/// `ignoredError` attributes, in its order.
pub const RULES: [&str; 9] = ["evalError", "twoDigitTextYear", "numberStoredAsText", "formula", "formulaRange", "unlockedFormula", "emptyCellReference", "listDataValidation", "calculatedColumn"];

/// Rule bits, as in [`RULES`].
pub const EVAL_ERROR: u16 = 1;
pub const NUMBER_AS_TEXT: u16 = 1 << 2;
pub const INCONSISTENT: u16 = 1 << 3;
pub const OMITS_ADJACENT: u16 = 1 << 4;
pub const UNLOCKED_FORMULA: u16 = 1 << 5;
const ALL_RULES: u16 = (1 << RULES.len()) - 1;

/// An area where Error Checking ignores the rules in `rules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ignored {
    pub r0: u32,
    pub c0: u32,
    pub r1: u32,
    pub c1: u32,
    pub rules: u16,
}

fn area_ok(&(r0, c0, r1, c1): &(u32, u32, u32, u32)) -> bool {
    r0 <= r1 && c0 <= c1 && r1 < MAX_ROWS && c1 < MAX_COLS
}

impl EditRange {
    pub fn valid(&self) -> bool {
        !self.title.trim().is_empty()
            && self.title.len() <= MAX_TITLE
            && !self.areas.is_empty()
            && self.areas.len() <= MAX_AREAS
            && self.areas.iter().all(area_ok)
            && self.password.as_ref().is_none_or(|p| p.len() <= 1024)
    }

    /// Whether the range covers (`row`, `col`).
    pub fn covers(&self, row: u32, col: u32) -> bool {
        self.areas.iter().any(|&(r0, c0, r1, c1)| (r0..=r1).contains(&row) && (c0..=c1).contains(&col))
    }
}

/// A sheet's edit ranges: each valid, titles distinct without case.
pub fn valid_ranges(list: &[EditRange]) -> bool {
    if list.len() > MAX_EDIT_RANGES || !list.iter().all(EditRange::valid) {
        return false;
    }
    let mut t: Vec<String> = list.iter().map(|e| e.title.to_lowercase()).collect();
    t.sort_unstable();
    t.windows(2).all(|w| w[0] != w[1])
}

/// A sheet's ignored errors: each area on the sheet with known rules.
pub fn valid_ignored(list: &[Ignored]) -> bool {
    list.len() <= MAX_IGNORED && list.iter().all(|i| area_ok(&(i.r0, i.c0, i.r1, i.c1)) && i.rules != 0 && i.rules & !ALL_RULES == 0)
}

/// The rules ignored at (`row`, `col`).
pub fn ignored_at(list: &[Ignored], row: u32, col: u32) -> u16 {
    list.iter().filter(|i| (i.r0..=i.r1).contains(&row) && (i.c0..=i.c1).contains(&col)).fold(0, |m, i| m | i.rules)
}

fn put_area(w: &mut Writer, (r0, c0, r1, c1): (u32, u32, u32, u32)) {
    for v in [r0, c0, r1, c1] {
        w.u32(v);
    }
}

fn get_area(r: &mut Reader) -> Result<(u32, u32, u32, u32), WireError> {
    Ok((r.u32()?, r.u32()?, r.u32()?, r.u32()?))
}

pub fn encode_ranges(list: &[EditRange], w: &mut Writer) {
    w.var(list.len() as u64);
    for e in list {
        w.str(&e.title);
        w.str(e.password.as_deref().unwrap_or(""));
        w.var(e.areas.len() as u64);
        for &a in &e.areas {
            put_area(w, a);
        }
    }
}

pub fn decode_ranges(r: &mut Reader) -> Result<Vec<EditRange>, WireError> {
    let n = r.count("edit ranges", MAX_EDIT_RANGES as u64, 3)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        let title = r.str("edit range title", MAX_TITLE as u64)?.to_string();
        let pw = r.str("edit range password", 1024)?;
        let password = (!pw.is_empty()).then(|| pw.to_string());
        let k = r.count("edit range areas", MAX_AREAS as u64, 16)?;
        let mut areas = Vec::with_capacity(k);
        for _ in 0..k {
            areas.push(get_area(r)?);
        }
        list.push(EditRange { title, areas, password });
    }
    if !valid_ranges(&list) {
        return Err(WireError::Invalid("edit ranges"));
    }
    Ok(list)
}

pub fn encode_ignored(list: &[Ignored], w: &mut Writer) {
    w.var(list.len() as u64);
    for i in list {
        put_area(w, (i.r0, i.c0, i.r1, i.c1));
        w.u16(i.rules);
    }
}

pub fn decode_ignored(r: &mut Reader) -> Result<Vec<Ignored>, WireError> {
    let n = r.count("ignored errors", MAX_IGNORED as u64, 18)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        let (r0, c0, r1, c1) = get_area(r)?;
        list.push(Ignored { r0, c0, r1, c1, rules: r.u16()? });
    }
    if !valid_ignored(&list) {
        return Err(WireError::Invalid("ignored errors"));
    }
    Ok(list)
}

fn shift_area(sheet: u16, a: (u32, u32, u32, u32), s: Shift) -> Option<(u32, u32, u32, u32)> {
    let r = s.map_rect(Rect { sheet, r0: a.0, c0: a.1, r1: a.2, c1: a.3 })?;
    Some((r.r0, r.c0, r.r1, r.c1))
}

/// The edit ranges after rows or columns move: each area follows, one
/// deleted whole goes, and so does a range left with no area.
pub(crate) fn shifted_ranges(list: &[EditRange], sheet: u16, s: Shift) -> Vec<EditRange> {
    list.iter().filter_map(|e| {
        let areas: Vec<_> = e.areas.iter().filter_map(|&a| shift_area(sheet, a, s)).collect();
        (!areas.is_empty()).then(|| EditRange { areas, ..e.clone() })
    }).collect()
}

/// The ignored errors after rows or columns move.
pub(crate) fn shifted_ignored(list: &[Ignored], sheet: u16, s: Shift) -> Vec<Ignored> {
    list.iter().filter_map(|i| shift_area(sheet, (i.r0, i.c0, i.r1, i.c1), s).map(|(r0, c0, r1, c1)| Ignored { r0, c0, r1, c1, rules: i.rules })).collect()
}
