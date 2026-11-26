//! Document properties (docs/spec/officina.md §16.152): the title, subject,
//! author, keywords, category and comments a workbook carries, as Excel's
//! `docProps/core.xml` and ODF's `meta.xml` keep them. They change only
//! through [`crate::ops::GridOp::Properties`], so they undo.

use officina_core::wire::{Reader, WireError, Writer};

/// The properties, in this order: what Sheets calls each, Dublin Core's (and
/// OOXML core properties') element, ODF's element.
pub const FIELDS: [(&str, &str, &str); 6] = [
    ("title", "dc:title", "dc:title"),
    ("subject", "dc:subject", "dc:subject"),
    ("author", "dc:creator", "meta:initial-creator"),
    ("keywords", "cp:keywords", "meta:keyword"),
    ("category", "cp:category", "meta:user-defined"),
    ("comments", "dc:description", "dc:description"),
];

/// The longest a property may be, in bytes.
pub const MAX_PROP: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocProps {
    values: [String; 6],
}

impl DocProps {
    /// The value of [`FIELDS`]`[i]` (empty when unset or out of range).
    pub fn get(&self, i: usize) -> &str {
        self.values.get(i).map_or("", String::as_str)
    }

    /// Sets [`FIELDS`]`[i]`: trimmed, control characters other than line
    /// breaks dropped, cut to [`MAX_PROP`] bytes on a character boundary.
    pub fn set(&mut self, i: usize, value: &str) {
        let Some(slot) = self.values.get_mut(i) else { return };
        let clean: String = value.trim().chars().filter(|c| !c.is_control() || *c == '\n').collect();
        let mut end = clean.len().min(MAX_PROP);
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        *slot = clean[..end].to_string();
    }

    /// The index of the property named `name` (Sheets', Dublin Core's or
    /// ODF's name, ignoring case).
    pub fn index(name: &str) -> Option<usize> {
        let n = name.trim().to_ascii_lowercase();
        FIELDS.iter().position(|(a, b, c)| *a == n || *b == n || *c == n || b.split(':').nth(1) == Some(n.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.values.iter().all(String::is_empty)
    }

    fn valid(&self) -> bool {
        self.values.iter().all(|v| v.len() <= MAX_PROP && v.trim() == v && v.chars().all(|c| !c.is_control() || c == '\n'))
    }

    pub fn encode(&self, w: &mut Writer) {
        for v in &self.values {
            w.str(v);
        }
    }

    pub fn decode(r: &mut Reader) -> Result<DocProps, WireError> {
        let mut p = DocProps::default();
        for v in p.values.iter_mut() {
            *v = r.str("document property", MAX_PROP as u64)?.to_string();
        }
        if !p.valid() {
            return Err(WireError::Invalid("document property"));
        }
        Ok(p)
    }
}

impl crate::Workbook {
    pub fn properties(&self) -> &DocProps {
        &self.props
    }

    /// Sets the properties outside undo (files use it); use
    /// [`crate::ops::GridOp::Properties`] for an undoable change.
    pub fn set_properties(&mut self, p: DocProps) {
        self.props = p;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_are_cleaned_bounded_and_round_trip() {
        let mut p = DocProps::default();
        p.set(0, "  Budget\u{7} 2027 ");
        p.set(5, "line one\nline two");
        p.set(3, &"é".repeat(MAX_PROP));
        p.set(9, "ignored");
        assert_eq!(p.get(0), "Budget 2027");
        assert_eq!(p.get(5), "line one\nline two");
        assert!(p.get(3).len() <= MAX_PROP && p.get(3).chars().all(|c| c == 'é'));
        assert_eq!(DocProps::index("Creator"), Some(2));
        assert_eq!(DocProps::index("keywords"), Some(3));
        assert_eq!(DocProps::index("nope"), None);
        let mut w = Writer::new();
        p.encode(&mut w);
        let bytes = w.into_bytes();
        assert_eq!(DocProps::decode(&mut Reader::new(&bytes)).unwrap(), p);
        // A control character in a file is refused.
        let mut w = Writer::new();
        for v in ["a\u{1}", "", "", "", "", ""] {
            w.str(v);
        }
        assert!(DocProps::decode(&mut Reader::new(&w.into_bytes())).is_err());
    }

    #[test]
    fn properties_undo_and_survive_the_native_file() {
        use crate::ops::GridOp;
        use officina_core::history::Op;
        let mut wb = crate::Workbook::new();
        wb.add_sheet("S").unwrap();
        let mut p = DocProps::default();
        p.set(0, "Plan");
        p.set(2, "Ana");
        let undo = GridOp::Properties(p.clone()).apply(&mut wb);
        assert_eq!(wb.properties(), &p);
        let op = GridOp::Properties(p.clone());
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.properties(), &p);
        undo.apply(&mut wb);
        assert!(wb.properties().is_empty());
    }
}
