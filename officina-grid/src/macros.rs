//! Macros kept inert (docs/spec/officina.md §5.3, §17 D7): a file's macro parts are
//! carried byte for byte and never parsed or run, so saving a `.xlsm` or a macro
//! `.ods` back to its own format doesn't damage it. The user can remove them.
//!
//! The grid knows nothing of either format's insides: a part is a path in the
//! package, a media type and bytes. The converter decides which parts are macro
//! parts on reading and which it will write back.

use std::sync::Arc;

use officina_core::wire::{Reader, WireError, Writer};

/// Parts at most.
pub const MAX_PARTS: usize = 1024;
/// Bytes of one part, and of all parts, at most.
pub const MAX_PART: usize = 32 << 20;
pub const MAX_TOTAL: usize = 64 << 20;
pub const MAX_PATH: usize = 255;
pub const MAX_MEDIA_TYPE: usize = 255;
/// A VBA code name (`ThisWorkbook`, `Sheet1`): at most this long.
pub const MAX_CODE_NAME: usize = 64;

/// The package family the parts came from; they are written back only to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroFamily {
    /// `.xlsm`: `xl/vbaProject.bin` and its signatures.
    Ooxml,
    /// `.ods`: the `Basic/`, `Dialogs/` and `Scripts/` folders.
    Odf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroPart {
    /// Path in the package: relative, `/`-separated, no `.` or `..` segments.
    pub path: Arc<str>,
    pub media_type: Arc<str>,
    pub data: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macros {
    pub family: MacroFamily,
    pub parts: Vec<MacroPart>,
    /// VBA's names for the workbook and for sheets by index (`codeName` in
    /// `.xlsx`), which the project's modules are bound to.
    pub workbook_code: Option<Arc<str>>,
    pub sheet_codes: Vec<(u16, Arc<str>)>,
}

/// A path a package may hold: printable ASCII without `\`, `:` or quotes, no empty,
/// `.` or `..` segments, not absolute.
pub fn path_ok(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH
        && p.bytes().all(|b| (0x20..0x7f).contains(&b) && !matches!(b, b'\\' | b':' | b'"' | b'\'' | b'<' | b'>' | b'&' | b'*' | b'?' | b'|'))
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

fn code_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_CODE_NAME && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn media_ok(s: &str) -> bool {
    s.len() <= MAX_MEDIA_TYPE && s.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'<' | b'>' | b'&' | b'\''))
}

impl Macros {
    /// Checks the bounds and that no path repeats.
    pub fn check(&self, sheets: usize) -> Result<(), &'static str> {
        if self.parts.is_empty() {
            return Err("macros without parts");
        }
        if self.parts.len() > MAX_PARTS {
            return Err("too many macro parts");
        }
        let mut total = 0usize;
        let mut seen = std::collections::HashSet::new();
        for p in &self.parts {
            if !path_ok(&p.path) {
                return Err("a macro part's path isn't a plain relative path");
            }
            if !media_ok(&p.media_type) {
                return Err("a macro part's media type is malformed");
            }
            if p.data.len() > MAX_PART {
                return Err("a macro part is too large");
            }
            total += p.data.len();
            if !seen.insert(p.path.to_ascii_lowercase()) {
                return Err("two macro parts with one path");
            }
        }
        if total > MAX_TOTAL {
            return Err("macro parts too large in all");
        }
        if self.workbook_code.as_deref().is_some_and(|c| !code_ok(c)) || self.sheet_codes.iter().any(|(s, c)| *s as usize >= sheets || !code_ok(c)) {
            return Err("a malformed macro code name");
        }
        let mut sh: Vec<u16> = self.sheet_codes.iter().map(|x| x.0).collect();
        sh.sort_unstable();
        if sh.windows(2).any(|w| w[0] == w[1]) {
            return Err("two code names for one sheet");
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> usize {
        self.parts.iter().map(|p| p.data.len()).sum()
    }

    /// The header (family and code names); parts go in chunks of their own.
    pub fn encode_head(&self, w: &mut Writer) {
        w.u8(match self.family {
            MacroFamily::Ooxml => 0,
            MacroFamily::Odf => 1,
        });
        w.str(self.workbook_code.as_deref().unwrap_or(""));
        w.var(self.sheet_codes.len() as u64);
        for (s, c) in &self.sheet_codes {
            w.u16(*s);
            w.str(c);
        }
    }

    pub fn decode_head(r: &mut Reader) -> Result<Macros, WireError> {
        let family = match r.u8()? {
            0 => MacroFamily::Ooxml,
            1 => MacroFamily::Odf,
            _ => return Err(WireError::Invalid("macro family")),
        };
        let wc = r.str("code name", MAX_CODE_NAME as u64)?;
        let workbook_code = (!wc.is_empty()).then(|| Arc::from(wc));
        let k = r.count("code names", crate::book::MAX_SHEETS as u64, 3)?;
        let mut sheet_codes = Vec::with_capacity(k);
        for _ in 0..k {
            let s = r.u16()?;
            sheet_codes.push((s, Arc::from(r.str("code name", MAX_CODE_NAME as u64)?)));
        }
        Ok(Macros { family, parts: Vec::new(), workbook_code, sheet_codes })
    }
}

impl MacroPart {
    pub fn encode(&self, w: &mut Writer) {
        w.str(&self.path);
        w.str(&self.media_type);
        w.bytes(&self.data);
    }

    pub fn decode(r: &mut Reader) -> Result<MacroPart, WireError> {
        let path = Arc::from(r.str("macro path", MAX_PATH as u64)?);
        let media_type = Arc::from(r.str("media type", MAX_MEDIA_TYPE as u64)?);
        let data = Arc::from(r.bytes("macro part", MAX_PART as u64)?);
        Ok(MacroPart { path, media_type, data })
    }
}

impl crate::book::Workbook {
    /// The file's macros, kept inert.
    pub fn macros(&self) -> Option<&Macros> {
        self.macros.as_ref()
    }

    /// Keeps `m` (replacing any), after checking it.
    pub fn set_macros(&mut self, m: Macros) -> Result<(), &'static str> {
        m.check(self.sheet_count())?;
        self.macros = Some(m);
        Ok(())
    }

    /// Drops the macros: the next save writes none.
    pub fn remove_macros(&mut self) -> Option<Macros> {
        self.macros.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Workbook;

    fn part(p: &str, n: usize) -> MacroPart {
        MacroPart { path: p.into(), media_type: "application/vnd.ms-office.vbaProject".into(), data: vec![7u8; n].into() }
    }

    #[test]
    fn paths_and_bounds() {
        for p in ["xl/vbaProject.bin", "Basic/Standard/Module 1.xml", "Scripts/python/a_b.py"] {
            assert!(path_ok(p), "{p}");
        }
        for p in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a/./b", "C:\\x", "a\"b", "x/", "é"] {
            assert!(!path_ok(p), "{p}");
        }
        let mut wb = Workbook::new();
        wb.add_sheet("A").unwrap();
        let m = |parts: Vec<MacroPart>| Macros { family: MacroFamily::Ooxml, parts, workbook_code: Some("ThisWorkbook".into()), sheet_codes: vec![(0, "Sheet1".into())] };
        assert!(wb.set_macros(m(vec![part("xl/vbaProject.bin", 10)])).is_ok());
        assert!(wb.set_macros(m(vec![])).is_err());
        assert!(wb.set_macros(m(vec![part("xl/a.bin", 1), part("XL/A.bin", 1)])).is_err());
        assert!(wb.set_macros(m(vec![part("xl/a.bin", MAX_PART + 1)])).is_err());
        let mut bad = m(vec![part("xl/a.bin", 1)]);
        bad.sheet_codes = vec![(5, "S".into())];
        assert!(wb.set_macros(bad).is_err());
        let mut bad = m(vec![part("xl/a.bin", 1)]);
        bad.workbook_code = Some("This Workbook\"".into());
        assert!(wb.set_macros(bad).is_err());
        assert!(wb.remove_macros().is_some());
        assert!(wb.macros().is_none());
    }

    #[test]
    fn macros_in_native_files() {
        let mut wb = Workbook::new();
        wb.add_sheet("A").unwrap();
        wb.add_sheet("B").unwrap();
        let m = Macros {
            family: MacroFamily::Odf,
            parts: vec![part("Basic/script-lc.xml", 30), part("Basic/Standard/Module1.xml", 5000), part("Basic/Standard/empty", 0)],
            workbook_code: None,
            sheet_codes: vec![(1, "Tabelle2".into())],
        };
        wb.set_macros(m.clone()).unwrap();
        let b = crate::native::save_to_vec(&wb);
        assert_eq!(crate::native::load(&b).unwrap().macros(), Some(&m));
        // Without macros: none come back.
        wb.remove_macros();
        assert!(crate::native::load(&crate::native::save_to_vec(&wb)).unwrap().macros().is_none());
    }
}
