//! The formula AST.

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::value::ErrorKind;
use std::sync::Arc;

/// A row or column coordinate: absolute (`$B$3`) or relative to the formula's own cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Coord {
    Abs(u32),
    Rel(i32),
}

impl Coord {
    /// The coordinate for a formula at `at`; `None` when it falls off the sheet.
    #[inline]
    pub fn resolve(self, at: u32, max: u32) -> Option<u32> {
        match self {
            Coord::Abs(v) => Some(v),
            Coord::Rel(d) => {
                let v = at as i64 + d as i64;
                (v >= 0 && v < max as i64).then_some(v as u32)
            }
        }
    }
    pub fn is_rel(self) -> bool {
        matches!(self, Coord::Rel(_))
    }
}

/// One cell reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRef {
    /// `None`: the formula's own sheet.
    pub sheet: Option<u16>,
    pub row: Coord,
    pub col: Coord,
}

/// How a range was written, for printing it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeKind {
    Cells,
    /// `A:C`
    Cols,
    /// `1:3`
    Rows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RangeRef {
    pub sheet: Option<u16>,
    pub r0: Coord,
    pub c0: Coord,
    pub r1: Coord,
    pub c1: Coord,
    pub kind: RangeKind,
}

impl RangeRef {
    pub fn whole_cols(sheet: Option<u16>, c0: Coord, c1: Coord) -> Self {
        RangeRef { sheet, r0: Coord::Abs(0), c0, r1: Coord::Abs(MAX_ROWS - 1), c1, kind: RangeKind::Cols }
    }
    pub fn whole_rows(sheet: Option<u16>, r0: Coord, r1: Coord) -> Self {
        RangeRef { sheet, r0, c0: Coord::Abs(0), r1, c1: Coord::Abs(MAX_COLS - 1), kind: RangeKind::Rows }
    }
}

/// Which rows of a table a structured reference takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableArea {
    /// The data rows (no specifier, or `[#Data]`).
    Data,
    All,
    Headers,
    Totals,
    /// `[#Headers],[#Data]`
    HeadersData,
    /// `[#Data],[#Totals]`
    DataTotals,
    /// `[@Col]`, `[#This Row]`: the formula's own row.
    ThisRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    /// Empty: the table around the formula's cell (`[@Qty]` written inside a table).
    pub table: Arc<str>,
    pub area: TableArea,
    /// The columns, first and last, by header name; `None`: every column.
    pub cols: Option<(Arc<str>, Arc<str>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    pub fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Pow => "^",
            BinOp::Concat => "&",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
        }
    }
}

/// A function as written: one Officina implements, or a name it doesn't know (which
/// evaluates to `#NAME?` but is kept, so the formula round-trips).
#[derive(Debug, Clone, PartialEq)]
pub enum Func {
    Known(u16),
    Unknown(Arc<str>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Str(Arc<str>),
    Bool(bool),
    Err(ErrorKind),
    Ref(CellRef),
    Range(RangeRef),
    /// A defined name or a table's name (its data rows); `#NAME?` when neither.
    Name(Arc<str>),
    /// A structured reference: `Sales[Amount]`, `[@Qty]`, `T[[#Headers],[A]:[C]]`.
    Table(Arc<TableRef>),
    Neg(Box<Expr>),
    Plus(Box<Expr>),
    Pct(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
    /// An array constant `{1,2;3,4}`, rows of constants.
    Array(Vec<Vec<Expr>>),
    /// Parentheses as written, kept so printing gives back what the user typed.
    Paren(Box<Expr>),
}

impl Expr {
    /// Calls `f` on every node, depth first.
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Neg(e) | Expr::Plus(e) | Expr::Pct(e) | Expr::Paren(e) => e.walk(f),
            Expr::Bin(_, a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::Call(_, args) => args.iter().for_each(|a| a.walk(f)),
            Expr::Array(rows) => rows.iter().flatten().for_each(|a| a.walk(f)),
            _ => {}
        }
    }
}
