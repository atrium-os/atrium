//! Read only cell values embedded in the XLSX. Link targets and remote
//! relationships are never consulted.

use super::read::{attr, xml, XlsxError};
use crate::zip::Archive;
use officina_grid::addr::{parse_col, parse_row};
use officina_grid::strings;
use officina_grid::{ErrorKind, Value};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

pub(super) const MAX_LINKS: usize = 128;
const MAX_XML_BYTES: u64 = 16 << 20;
const MAX_CELLS: usize = 100_000;

#[derive(Default)]
pub(super) struct Cache {
    books: HashMap<usize, Book>,
}

#[derive(Default)]
struct Book {
    sheets: Vec<String>,
    cells: HashMap<(usize, u32, u32), Value>,
}

impl Cache {
    pub(super) fn read(z: &Archive, paths: impl Iterator<Item = (usize, String)>) -> Self {
        let (mut bytes, mut cells) = (MAX_XML_BYTES, MAX_CELLS);
        let mut cache = Self::default();
        for (index, part) in paths {
            let Some(entry) = z.find(&part) else { continue };
            let Some(left) = bytes.checked_sub(entry.size) else {
                continue;
            };
            bytes = left;
            if let Ok(book) = z
                .read(entry)
                .map_err(XlsxError::from)
                .and_then(|data| read_book(&data, &mut cells))
            {
                cache.books.insert(index, book);
            }
            if cells == 0 {
                break;
            }
        }
        cache
    }

    /// A single fixed or absolute-coordinate cell, optionally quoted.
    /// Shared formula followers may use the cache only for an absolute address.
    pub(super) fn cell(&self, formula: &str, absolute: bool) -> Option<Value> {
        if self.books.is_empty() {
            return None;
        }
        let (qualified, address) = formula.trim().rsplit_once('!')?;
        if address.len() > 16 {
            return None;
        }
        let (col, row) = split_a1(address)?;
        if absolute && !(col.starts_with('$') && row.starts_with('$')) {
            return None;
        }
        let (row, col) = address_parts(col, row)?;
        let decoded;
        let sheet = if let Some(quoted) = qualified
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
        {
            decoded = quoted.replace("''", "'");
            decoded.as_str()
        } else {
            qualified
        };
        let (index, name) = qualifier(sheet)?;
        let book = self.books.get(&index)?;
        let sheet = book
            .sheets
            .iter()
            .position(|sheet| sheet.eq_ignore_ascii_case(name))?;
        book.cells.get(&(sheet, row, col)).cloned()
    }
}

fn split_a1(address: &str) -> Option<(&str, &str)> {
    let split = address
        .strip_prefix('$')
        .unwrap_or(address)
        .find(|c: char| c == '$' || c.is_ascii_digit())?;
    let col = split + usize::from(address.starts_with('$'));
    Some(address.split_at(col))
}

fn address_parts(col: &str, row: &str) -> Option<(u32, u32)> {
    let row = parse_row(row.strip_prefix('$').unwrap_or(row))?;
    let col = parse_col(col.strip_prefix('$').unwrap_or(col))?;
    (row < super::write::XLSX_ROWS && col < super::write::XLSX_COLS).then_some((row, col))
}

fn qualifier(text: &str) -> Option<(usize, &str)> {
    let (index, sheet) = text.strip_prefix('[')?.split_once(']')?;
    if index.is_empty() || !index.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let index = index.parse::<usize>().ok()?;
    (index != 0).then_some((index, sheet))
}

fn read_book(data: &[u8], capacity: &mut usize) -> Result<Book, XlsxError> {
    let mut reader = xml(data);
    let mut book = Book::default();
    let (mut external_book, mut sheet) = (false, None);
    loop {
        match reader.read_event()? {
            Event::Start(tag) if tag.local_name().as_ref() == b"externalBook" => {
                external_book = true
            }
            Event::End(tag) if tag.local_name().as_ref() == b"externalBook" => break,
            Event::Start(tag) if external_book && tag.local_name().as_ref() == b"sheetData" => {
                sheet = attr(&tag, b"sheetId")?
                    .and_then(|id| id.parse::<usize>().ok())
                    .filter(|id| *id < officina_grid::book::MAX_SHEETS);
            }
            Event::Start(tag) | Event::Empty(tag)
                if external_book && tag.local_name().as_ref() == b"sheetName" =>
            {
                if book.sheets.len() < officina_grid::book::MAX_SHEETS {
                    book.sheets.push(attr(&tag, b"val")?.unwrap_or_default());
                }
            }
            Event::Start(tag) if tag.local_name().as_ref() == b"cell" && *capacity != 0 => {
                let address = attr(&tag, b"r")?.and_then(|r| {
                    let (col, row) = split_a1(&r)?;
                    address_parts(col, row)
                });
                let kind = attr(&tag, b"t")?;
                if let (Some(sheet), Some((row, col))) = (sheet, address) {
                    if let Some(value) = read_value(&mut reader, kind.as_deref())? {
                        book.cells.insert((sheet, row, col), value);
                        *capacity -= 1;
                    }
                }
            }
            Event::End(tag) if tag.local_name().as_ref() == b"sheetData" => sheet = None,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(book)
}

fn read_value(reader: &mut Reader<&[u8]>, kind: Option<&str>) -> Result<Option<Value>, XlsxError> {
    loop {
        match reader.read_event()? {
            Event::Start(tag) if tag.local_name().as_ref() == b"v" => {
                let text = reader.read_text(tag.name())?;
                return Ok(decode(kind, &text));
            }
            Event::Empty(tag) if tag.local_name().as_ref() == b"v" => return Ok(decode(kind, "")),
            Event::End(tag) if tag.local_name().as_ref() == b"cell" => return Ok(None),
            Event::Start(tag) => {
                reader.read_to_end(tag.name())?;
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

fn decode(kind: Option<&str>, text: &str) -> Option<Value> {
    match kind.unwrap_or("n") {
        "n" => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .map(Value::num),
        "str" => {
            let value = quick_xml::escape::unescape(text).ok()?;
            Some(Value::str(strings::clip(&value)))
        }
        "b" => match text.trim() {
            "0" => Some(Value::Bool(false)),
            "1" => Some(Value::Bool(true)),
            _ => None,
        },
        "e" => ErrorKind::from_text(text.trim()).map(Value::Err),
        _ => None,
    }
}

/// Find an indexed external qualifier, excluding Excel string literals and
/// brackets within structured table references.
pub(super) fn has_reference(formula: &str) -> bool {
    let bytes = formula.as_bytes();
    let mut i = 0;
    while let Some(&token) = bytes.get(i) {
        match token {
            quote @ (b'\'' | b'"') => {
                let start = i + 1;
                i = start;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        if bytes.get(i + 1) != Some(&quote) {
                            if quote == b'\''
                                && bytes.get(i + 1) == Some(&b'!')
                                && qualifier(&formula[start..i]).is_some()
                            {
                                return true;
                            }
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'[' => {
                let prev = i.checked_sub(1).and_then(|p| bytes.get(p));
                let separate = prev.is_none_or(|ch| {
                    !(ch.is_ascii_alphanumeric()
                        || matches!(ch, b'_' | b'.' | b'\\' | b'$')
                        || *ch >= 128)
                });
                if separate {
                    if let Some((_, suffix)) = qualifier(&formula[i..]) {
                        if suffix
                            .trim_start_matches(|ch: char| {
                                ch.is_alphanumeric() || matches!(ch, '_' | '.' | '$')
                            })
                            .starts_with('!')
                        {
                            return true;
                        }
                    }
                }
                // Nested table brackets and apostrophe-escaped bracket characters.
                let mut depth = 1;
                while i + 1 < bytes.len() && depth != 0 {
                    i += 1;
                    match bytes[i] {
                        b'[' => depth += 1,
                        b']' => depth -= 1,
                        b'\'' if i + 1 < bytes.len() => i += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}
