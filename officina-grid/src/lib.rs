//! Officina's spreadsheet engine (docs/spec/officina.md §8). Headless: storage, formulas,
//! dependencies and recalculation; the grid on screen is Pergola's `GridView` (O2).
//!
//! ```
//! use officina_grid::{Workbook, CellAddr};
//! let mut wb = Workbook::new();
//! let s = wb.add_sheet("Sheet1").unwrap();
//! wb.set_numbers(s, 0, 0, &[1.0, 2.0, 3.0]);
//! wb.fill_formula(s, 1, 0, 2, "=A1*10").unwrap();
//! wb.set_formula(CellAddr::new(s, 3, 1), "=SUM(B1:B3)").unwrap();
//! wb.recalc();
//! assert_eq!(wb.get(CellAddr::new(s, 3, 1)).display(), "60");
//! ```

pub mod addr;
pub mod book;
pub mod column;
pub mod csv;
pub mod deps;
pub mod eval;
pub mod fast;
pub mod formula;
pub mod native;
pub mod numfmt;
pub mod ops;
pub mod recalc;
pub mod strings;
pub mod value;

pub use addr::{CellAddr, Rect};
pub use book::{Input, Workbook};
pub use value::{ErrorKind, Value};
