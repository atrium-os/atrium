//! BIFF8 cell notes (Note, Obj and TxO) and hyperlinks (HLink).

use std::collections::HashMap;

use officina_grid::comments::{Comment, CommentRun, CommentStyle};
use officina_grid::links::{Link, MAX_TARGET};
use officina_grid::Rect;

use super::{biff::Cur, formula, FontRec, Record, Short};

pub(super) struct Note {
    pub row: u32,
    pub col: u32,
    pub object: u16,
    pub author: String,
}

impl Note {
    pub(super) fn read(rec: &Record) -> Result<Self, Short> {
        let mut c = Cur::new(rec);
        let row = c.u16()? as u32;
        let col = c.u16()? as u32;
        c.skip(2)?; // Note visibility and shared state.
        let object = c.u16()?;
        let author = c.xl_string()?;
        if col >= formula::COLS {
            return Err(Short);
        }
        Ok(Self {
            row,
            col,
            object,
            author,
        })
    }
}

pub(super) struct Text {
    text: String,
    runs: Vec<CommentRun>,
}

impl Text {
    pub(super) fn read(rec: &Record, fonts: &[FontRec]) -> Result<(Self, bool), Short> {
        let mut c = Cur::new(rec);
        c.skip(10)?;
        let count = c.u16()? as usize;
        let run_bytes = c.u16()? as usize;
        if count == 0 && run_bytes == 0 {
            return Ok((
                Self {
                    text: String::new(),
                    runs: Vec::new(),
                },
                false,
            ));
        }
        if count > 32_767 {
            return Err(Short);
        }
        let first = *rec.breaks.first().filter(|&&p| p >= 18).ok_or(Short)?;
        c.skip(first - 14)?;
        let text = c.chars(count, false)?;

        // Keep the text even when an optional run cannot be interpreted.
        let mut runs = Vec::new();
        let mut damaged = run_bytes < 16 || run_bytes % 8 != 0 || run_bytes > c.left();
        if !damaged {
            let mut offsets = HashMap::new();
            let mut units = 0;
            for (byte, ch) in text.char_indices() {
                offsets.insert(units, byte);
                units += ch.len_utf16();
            }
            offsets.insert(units, text.len());
            let mut start = 0;
            let mut style = CommentStyle::default();
            for _ in 0..run_bytes / 8 {
                let offset = c.u16()? as usize;
                let font = c.u16()?;
                c.skip(4)?;
                let Some(&end) = offsets.get(&offset) else {
                    damaged = true;
                    continue;
                };
                if end < start {
                    damaged = true;
                    continue;
                }
                if end > start && style.mask() != 0 {
                    runs.push(CommentRun { start, end, style });
                }
                start = end;
                style = match super::font_at(fonts, font) {
                    Some(f) => CommentStyle {
                        bold: f.bold,
                        italic: f.italic,
                        underline: f.underline,
                        strike: f.strike,
                    },
                    None => CommentStyle::default(),
                };
            }
            if start < text.len() && style.mask() != 0 {
                runs.push(CommentRun {
                    start,
                    end: text.len(),
                    style,
                });
            }
        }
        Ok((Self { text, runs }, damaged))
    }

    pub(super) fn comment(self, author: &str) -> Comment {
        Comment::with_runs(author, &self.text, self.runs)
    }
}

pub(super) fn hyperlink(sheet: u16, rec: &Record) -> Result<(Rect, Link), Short> {
    let mut c = Cur::new(rec);
    let r0 = c.u16()? as u32;
    let r1 = c.u16()? as u32;
    let c0 = c.u16()? as u32;
    let c1 = c.u16()? as u32;
    if r0 > r1 || c0 > c1 || c1 >= formula::COLS {
        return Err(Short);
    }
    c.skip(16)?; // The hyperlink component CLSID.
    if c.u32()? != 2 {
        return Err(Short);
    }
    let flags = c.u32()?;
    if flags & 0x10 != 0 {
        skip_link_string(&mut c)?; // Display name is independent of the cell's value.
    }
    if flags & 0x80 != 0 {
        skip_link_string(&mut c)?; // Target frame has no meaning in Sheets.
    }
    let mut target = if flags & 1 == 0 {
        String::new()
    } else if flags & 0x100 != 0 {
        read_link_string(&mut c)?
    } else {
        read_moniker(&mut c)?
    };
    if flags & 8 != 0 {
        let location = read_link_string(&mut c)?;
        if !location.is_empty() {
            target.push('#');
            target.push_str(location.trim_start_matches('#'));
        }
    }
    if target.len() > MAX_TARGET || target.contains('\0') {
        return Err(Short);
    }
    let link = Link::new(&target).ok_or(Short)?;
    Ok((
        Rect {
            sheet,
            r0,
            r1,
            c0,
            c1,
        },
        link,
    ))
}

fn skip_link_string(c: &mut Cur<'_>) -> Result<(), Short> {
    let units = c.u32()? as usize;
    c.skip(units.checked_mul(2).ok_or(Short)?)
}

fn read_link_string(c: &mut Cur<'_>) -> Result<String, Short> {
    let units = c.u32()? as usize;
    if units == 0 {
        return Ok(String::new());
    }
    if units > MAX_TARGET + 1 {
        return Err(Short);
    }
    utf16(c.bytes(units * 2)?, true)
}

fn utf16(raw: &[u8], terminated: bool) -> Result<String, Short> {
    if raw.len() % 2 != 0 {
        return Err(Short);
    }
    let mut words: Vec<_> = raw
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    if terminated && words.pop() != Some(0) {
        return Err(Short);
    }
    String::from_utf16(&words).map_err(|_| Short)
}

fn read_moniker(c: &mut Cur<'_>) -> Result<String, Short> {
    const URL: [u8; 16] = [
        0xe0, 0xc9, 0xea, 0x79, 0xf9, 0xba, 0xce, 0x11, 0x8c, 0x82, 0, 0xaa, 0, 0x4b, 0xa9, 0x0b,
    ];
    const FILE: [u8; 16] = [3, 3, 0, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46];
    match c.bytes(16)? {
        id if id == URL => {
            let size = c.u32()? as usize;
            if size > (MAX_TARGET + 1) * 2 + 24 {
                return Err(Short);
            }
            let bytes = c.bytes(size)?;
            let end = bytes
                .chunks_exact(2)
                .position(|p| p == [0, 0])
                .ok_or(Short)?
                * 2
                + 2;
            if !matches!(size - end, 0 | 24) {
                return Err(Short);
            }
            utf16(&bytes[..end], true)
        }
        id if id == FILE => {
            let parents = c.u16()? as usize;
            let size = c.u32()? as usize;
            if parents > MAX_TARGET / 3 || size > MAX_TARGET + 1 {
                return Err(Short);
            }
            let ansi = c.bytes(size)?.strip_suffix(&[0]).ok_or(Short)?;
            c.skip(24)?; // Server offset, version and reserved fields.
            let unicode = c.u32()? as usize;
            let path = if unicode != 0 {
                let size = c.u32()? as usize;
                if size > MAX_TARGET * 2 || size + 6 != unicode || c.u16()? != 3 {
                    return Err(Short);
                }
                utf16(c.bytes(size)?, false)?
            } else {
                // Without an explicit Unicode path, non-ASCII bytes depend
                // on the machine's Windows ANSI code page.
                if !ansi.is_ascii() {
                    return Err(Short);
                }
                String::from_utf8(ansi.to_vec()).map_err(|_| Short)?
            };
            Ok(format!("{}{path}", "../".repeat(parents)))
        }
        _ => Err(Short),
    }
}
