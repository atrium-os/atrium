//! The Watch Window's cells (docs/spec/officina.md §16.145): cells a person
//! keeps an eye on while working elsewhere, kept with their sheet as Excel
//! keeps `<cellWatches>`. The list changes only through
//! [`crate::ops::GridOp::Watches`], so it undoes.

use officina_core::wire::{Reader, WireError, Writer};

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::shift::Shift;

/// The most watched cells a sheet keeps.
pub const MAX_WATCHES: usize = 1000;

/// A sheet's watches: each on the sheet, none twice, at most [`MAX_WATCHES`].
pub fn valid_list(list: &[(u32, u32)]) -> bool {
    if list.len() > MAX_WATCHES || !list.iter().all(|&(r, c)| r < MAX_ROWS && c < MAX_COLS) {
        return false;
    }
    let mut at = list.to_vec();
    at.sort_unstable();
    at.windows(2).all(|w| w[0] != w[1])
}

pub fn encode(list: &[(u32, u32)], w: &mut Writer) {
    w.var(list.len() as u64);
    for &(r, c) in list {
        w.u32(r);
        w.u32(c);
    }
}

pub fn decode(r: &mut Reader) -> Result<Vec<(u32, u32)>, WireError> {
    let n = r.count("watches", MAX_WATCHES as u64, 8)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        list.push((r.u32()?, r.u32()?));
    }
    if !valid_list(&list) {
        return Err(WireError::Invalid("watches"));
    }
    Ok(list)
}

/// The list after rows or columns move: each watch follows its cell, and
/// one on a deleted cell goes.
pub(crate) fn shifted(list: &[(u32, u32)], s: Shift) -> Vec<(u32, u32)> {
    list.iter().filter_map(|&(r, c)| s.map_cell(r, c)).collect()
}
