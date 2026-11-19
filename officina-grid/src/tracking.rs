//! Persistent baseline for a tracked cell change set.
use std::collections::BTreeMap;

use officina_core::wire::{Reader, WireError, Writer};

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::{Input, Workbook, MAX_SHEETS, MAX_SHEET_NAME};
use crate::compare::{CellSnapshot, Change, MergeError};
use crate::ops::{read_value, write_value};
use crate::strings::MAX_TEXT;

pub const MAX_AUTHOR: usize = 255;
const MAX_BYTES: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct TrackedCells {
    author: String,
    baseline: CellSnapshot,
    // Budget for tree entries and their owned text. Also bounds the wire payload.
    bytes: u64,
}

impl TrackedCells {
    pub fn capture(wb: &Workbook, author: &str) -> Option<Self> {
        let mut set = Self::empty(&author[..author.floor_char_boundary(author.len().min(MAX_AUTHOR))]);
        for sheet in 0..wb.sheet_count() as u16 {
            let name = wb.sheet_name(sheet);
            set.reserve(128 + name.len() as u64)?;
            let mut cells = BTreeMap::new();
            let mut fits = true;
            let (rows, cols) = wb.extent(sheet);
            if rows != 0 && cols != 0 {
                let rect = Rect { sheet, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 };
                wb.each_cell(&rect, &mut |row, col, _| {
                    let addr = CellAddr::new(sheet, row, col);
                    if fits && wb.spilled_from(addr).is_none() {
                        let input = wb.input(addr);
                        fits = set.reserve_input(&input).is_some();
                        if fits { cells.insert((row, col), input); }
                    }
                });
            }
            if !fits { return None; }
            // Formula groups can exist before their cells have been calculated.
            for (_, group) in wb.groups().filter(|(_, g)| g.sheet == sheet) {
                for row in group.r0..=group.r1 {
                    if let std::collections::btree_map::Entry::Vacant(entry) = cells.entry((row, group.col)) {
                        let input = wb.input(CellAddr::new(sheet, row, group.col));
                        set.reserve_input(&input)?;
                        entry.insert(input);
                    }
                }
            }
            set.baseline.sheets.insert(name, cells);
        }
        Some(set)
    }

    pub fn author(&self) -> &str { &self.author }

    pub fn changes(&self, wb: &Workbook) -> Result<Vec<Change>, MergeError> {
        self.baseline.changes_since(wb)
    }

    pub fn memory_bytes(&self) -> u64 { self.bytes }

    fn empty(author: &str) -> Self {
        Self {
            author: author.into(),
            baseline: CellSnapshot { sheets: BTreeMap::new() },
            bytes: 128 + author.len() as u64,
        }
    }

    fn reserve(&mut self, bytes: u64) -> Option<()> {
        let used = self.bytes + bytes;
        if used > MAX_BYTES { return None; }
        self.bytes = used;
        Some(())
    }

    fn reserve_input(&mut self, input: &Input) -> Option<()> {
        let text = match input {
            Input::Formula(f) => f.len(),
            Input::Value(crate::Value::Str(s)) => s.len(),
            _ => 0,
        };
        if text > MAX_TEXT { return None; }
        self.reserve(128 + text as u64)
    }

    pub fn encode(&self, w: &mut Writer) {
        w.str(&self.author);
        w.var(self.baseline.sheets.len() as u64);
        for (name, cells) in &self.baseline.sheets {
            w.str(name);
            w.var(cells.len() as u64);
            for (&(row, col), input) in cells {
                w.var(row as u64);
                w.var(col as u64);
                match input {
                    Input::Value(value) => {
                        w.u8(0);
                        write_value(w, value);
                    }
                    Input::Formula(text) => {
                        w.u8(1);
                        w.str(text);
                    }
                    Input::Empty => w.u8(2),
                }
            }
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Self, WireError> {
        const TOO_BIG: WireError = WireError::Invalid("tracked cell baseline exceeds its budget");
        if r.remaining() as u64 > MAX_BYTES { return Err(TOO_BIG); }
        let mut set = Self::empty(r.str("change set author", MAX_AUTHOR as u64)?);
        let count = r.count("tracked sheets", MAX_SHEETS as u64, 2)?;
        for _ in 0..count {
            let name = r.str("tracked sheet name", MAX_SHEET_NAME as u64)?;
            if name.is_empty() || set.baseline.sheets.contains_key(name) {
                return Err(WireError::Invalid("tracked sheet name"));
            }
            set.reserve(128 + name.len() as u64).ok_or(TOO_BIG)?;
            let count = r.count("tracked inputs", (MAX_BYTES - set.bytes) / 128, 3)?;
            let mut cells = BTreeMap::new();
            let mut previous = None;
            for _ in 0..count {
                let row = r.len("tracked row", (MAX_ROWS - 1) as u64)? as u32;
                let col = r.len("tracked column", (MAX_COLS - 1) as u64)? as u32;
                if previous.is_some_and(|p| p >= (row, col)) {
                    return Err(WireError::Invalid("tracked inputs out of order"));
                }
                previous = Some((row, col));
                let input = match r.u8()? {
                    0 => Input::Value(read_value(r)?),
                    1 => Input::Formula(r.str("tracked formula", MAX_TEXT as u64)?.into()),
                    2 => Input::Empty,
                    _ => return Err(WireError::Invalid("tracked input")),
                };
                set.reserve_input(&input).ok_or(TOO_BIG)?;
                cells.insert((row, col), input);
            }
            set.baseline.sheets.insert(name.into(), cells);
        }
        Ok(set)
    }
}

impl Workbook {
    pub fn tracked_cells(&self) -> Option<&TrackedCells> { self.tracked_cells.as_ref() }

    pub fn set_tracked_cells(&mut self, set: TrackedCells) { self.tracked_cells = Some(set); }
}
