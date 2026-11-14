//! Cell addresses and A1 notation.

/// Rows a sheet may have (0-based rows `0..MAX_ROWS`). docs/spec/officina.md §8.1.
pub const MAX_ROWS: u32 = (1 << 31) - 1;
/// Columns a sheet may have.
pub const MAX_COLS: u32 = 1 << 20;

/// One cell: 0-based row and column on a sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellAddr {
    pub sheet: u16,
    pub row: u32,
    pub col: u32,
}

impl CellAddr {
    pub fn new(sheet: u16, row: u32, col: u32) -> Self {
        CellAddr { sheet, row, col }
    }
}

/// A rectangle of cells, inclusive bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    pub sheet: u16,
    pub r0: u32,
    pub c0: u32,
    pub r1: u32,
    pub c1: u32,
}

impl Rect {
    pub fn cell(a: CellAddr) -> Self {
        Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col }
    }
    pub fn rows(&self) -> u64 {
        (self.r1 - self.r0) as u64 + 1
    }
    pub fn cols(&self) -> u64 {
        (self.c1 - self.c0) as u64 + 1
    }
    pub fn cells(&self) -> u64 {
        self.rows() * self.cols()
    }
    pub fn contains(&self, a: CellAddr) -> bool {
        a.sheet == self.sheet && (self.r0..=self.r1).contains(&a.row) && (self.c0..=self.c1).contains(&a.col)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.sheet == o.sheet && self.r0 <= o.r1 && o.r0 <= self.r1 && self.c0 <= o.c1 && o.c0 <= self.c1
    }
}

/// Column letters for a 0-based column: 0 -> "A", 25 -> "Z", 26 -> "AA".
pub fn col_name(mut c: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

/// The 0-based column for letters (case-insensitive), within [`MAX_COLS`].
pub fn parse_col(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 5 {
        return None;
    }
    let mut c: u64 = 0;
    for b in s.bytes() {
        let d = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a',
            _ => return None,
        } as u64;
        c = c * 26 + d + 1;
    }
    let c = c - 1;
    (c < MAX_COLS as u64).then_some(c as u32)
}

/// The 0-based row for a 1-based row number in text, within [`MAX_ROWS`].
pub fn parse_row(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) || s.starts_with('0') {
        return None;
    }
    let n: u64 = s.parse().ok()?;
    (n >= 1 && n <= MAX_ROWS as u64).then_some((n - 1) as u32)
}

/// "B7" -> (row 6, col 1). No `$` handling (that is the formula lexer's).
pub fn parse_a1(s: &str) -> Option<(u32, u32)> {
    let split = s.find(|ch: char| ch.is_ascii_digit())?;
    Some((parse_row(&s[split..])?, parse_col(&s[..split])?))
}

pub fn a1(row: u32, col: u32) -> String {
    format!("{}{}", col_name(col), row as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns() {
        for (c, n) in [(0, "A"), (25, "Z"), (26, "AA"), (701, "ZZ"), (702, "AAA"), (16383, "XFD")] {
            assert_eq!(col_name(c), n);
            assert_eq!(parse_col(n), Some(c));
        }
        for c in 0..20000 {
            assert_eq!(parse_col(&col_name(c)), Some(c));
        }
        assert_eq!(parse_col(&col_name(MAX_COLS - 1)), Some(MAX_COLS - 1));
        assert_eq!(parse_col(&col_name(MAX_COLS)), None);
        assert_eq!(parse_a1("b7"), Some((6, 1)));
        assert_eq!(parse_a1("A0"), None);
        assert_eq!(parse_a1("A01"), None);
        assert_eq!(parse_row("2147483647"), Some(MAX_ROWS - 1));
        assert_eq!(parse_row("2147483648"), None);
    }
}
