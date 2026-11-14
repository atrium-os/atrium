//! The workbook's shared string pool: each distinct text is stored once and cells hold
//! its id. Interning is by exact text (case matters for display).

use std::collections::HashMap;
use std::sync::Arc;

pub type StrId = u32;

/// The most distinct strings a workbook holds.
pub const MAX_STRINGS: usize = 1 << 28;
/// The longest text a cell holds, in bytes (Excel: 32,767 characters).
pub const MAX_TEXT: usize = 128 * 1024;

#[derive(Debug, Default, Clone)]
pub struct StringPool {
    texts: Vec<Arc<str>>,
    ids: HashMap<Arc<str>, StrId>,
}

impl StringPool {
    pub fn new() -> Self {
        Self::default()
    }
    /// The id for `s`, adding it if new. Texts over [`MAX_TEXT`] are cut at a character
    /// boundary.
    pub fn intern(&mut self, s: &str) -> StrId {
        let s = clip(s);
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        assert!(self.texts.len() < MAX_STRINGS, "string pool full");
        let a: Arc<str> = Arc::from(s);
        let id = self.texts.len() as StrId;
        self.texts.push(a.clone());
        self.ids.insert(a, id);
        id
    }
    pub fn intern_arc(&mut self, s: &Arc<str>) -> StrId {
        if s.len() <= MAX_TEXT {
            if let Some(&id) = self.ids.get(s) {
                return id;
            }
        }
        self.intern(s)
    }
    pub fn get(&self, id: StrId) -> &Arc<str> {
        &self.texts[id as usize]
    }
    pub fn len(&self) -> usize {
        self.texts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Arc<str>> {
        self.texts.iter()
    }
}

/// `s` cut to at most [`MAX_TEXT`] bytes at a character boundary.
pub fn clip(s: &str) -> &str {
    if s.len() <= MAX_TEXT {
        return s;
    }
    let mut end = MAX_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
