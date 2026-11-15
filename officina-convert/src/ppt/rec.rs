//! PowerPoint 97–2003 records (MS-PPT §2.3.1) and OfficeArt's, which share
//! the header: version and instance, type, length. A version of 0xF marks a
//! container whose body is more records.

/// One record: where its body is and what it is.
#[derive(Debug, Clone, Copy)]
pub struct Rec<'a> {
    pub ver: u8,
    pub inst: u16,
    pub ty: u16,
    pub body: &'a [u8],
}

pub fn u16_at(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

pub fn i32_at(b: &[u8], at: usize) -> i32 {
    u32_at(b, at) as i32
}

pub fn i16_at(b: &[u8], at: usize) -> i16 {
    u16_at(b, at) as i16
}

/// The record at the start of `b`, and the bytes after it; a record cut
/// short is cut at the end of `b`.
pub fn rec_at(b: &[u8]) -> Option<(Rec<'_>, &[u8])> {
    if b.len() < 8 {
        return None;
    }
    let vi = u16_at(b, 0);
    let ty = u16_at(b, 2);
    let len = u32_at(b, 4) as usize;
    let end = 8usize.saturating_add(len).min(b.len());
    Some((Rec { ver: (vi & 0xF) as u8, inst: vi >> 4, ty, body: &b[8..end] }, &b[end..]))
}

/// The records one after another in `b`.
pub fn recs(mut b: &[u8]) -> impl Iterator<Item = Rec<'_>> {
    std::iter::from_fn(move || {
        let (r, rest) = rec_at(b)?;
        b = rest;
        Some(r)
    })
}

impl<'a> Rec<'a> {
    pub fn is_container(&self) -> bool {
        self.ver == 0xF
    }

    /// The records inside a container (none in an atom).
    pub fn kids(&self) -> impl Iterator<Item = Rec<'a>> {
        recs(if self.is_container() { self.body } else { &[] })
    }

    pub fn child(&self, ty: u16) -> Option<Rec<'a>> {
        self.kids().find(|r| r.ty == ty)
    }

    /// The first record of type `ty` anywhere below, at most `depth` deep.
    pub fn find(&self, ty: u16, depth: usize) -> Option<Rec<'a>> {
        for k in self.kids() {
            if k.ty == ty {
                return Some(k);
            }
            if depth > 0 && k.is_container() {
                if let Some(x) = k.find(ty, depth - 1) {
                    return Some(x);
                }
            }
        }
        None
    }
}

pub mod ty {
    pub const DOCUMENT: u16 = 0x03E8;
    pub const DOCUMENT_ATOM: u16 = 0x03E9;
    pub const SLIDE: u16 = 0x03EE;
    pub const SLIDE_ATOM: u16 = 0x03EF;
    pub const NOTES: u16 = 0x03F0;
    pub const NOTES_ATOM: u16 = 0x03F1;
    pub const ENVIRONMENT: u16 = 0x03F2;
    pub const SLIDE_PERSIST_ATOM: u16 = 0x03F3;
    pub const MAIN_MASTER: u16 = 0x03F8;
    pub const SLIDE_SHOW_SLIDE_INFO: u16 = 0x03F9;
    pub const LIST: u16 = 0x07D0;
    pub const FONT_COLLECTION: u16 = 0x07D5;
    pub const COLOR_SCHEME: u16 = 0x07F0;
    pub const PP_DRAWING_GROUP: u16 = 0x040B;
    pub const PP_DRAWING: u16 = 0x040C;
    pub const OUTLINE_TEXT_REF: u16 = 0x0F9E;
    pub const TEXT_HEADER: u16 = 0x0F9F;
    pub const TEXT_CHARS: u16 = 0x0FA0;
    pub const STYLE_TEXT_PROP: u16 = 0x0FA1;
    pub const TEXT_MASTER_STYLE: u16 = 0x0FA3;
    pub const TEXT_BYTES: u16 = 0x0FA8;
    pub const TEXT_SPECIAL_INFO: u16 = 0x0FAA;
    pub const FONT_ENTITY: u16 = 0x0FB7;
    pub const SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
    pub const USER_EDIT: u16 = 0x0FF5;
    pub const CURRENT_USER: u16 = 0x0FF6;
    pub const PLACEHOLDER: u16 = 0x0BC3;
    pub const INTERACTIVE_INFO: u16 = 0x0FF2;
    pub const INTERACTIVE_INFO_ATOM: u16 = 0x0FF3;
    pub const EX_OBJ_LIST: u16 = 0x0409;
    pub const EX_HYPERLINK: u16 = 0x0FD7;
    pub const EX_HYPERLINK_ATOM: u16 = 0x0FD3;
    pub const CSTRING: u16 = 0x0FBA;
    pub const TEXT_INTERACTIVE_INFO: u16 = 0x0FDF;
    pub const PERSIST_DIRECTORY: u16 = 0x1772;
    pub const SLIDE_NUMBER_MC: u16 = 0x0FD8;
    pub const DATE_TIME_MC: u16 = 0x0FF7;
    pub const GENERIC_DATE_MC: u16 = 0x0FF8;
    pub const RTF_DATE_TIME_MC: u16 = 0x1015;
    pub const CRYPT_SESSION: u16 = 0x2F14;
    pub const HEADERS_FOOTERS: u16 = 0x0FD9;
    pub const ROUND_TRIP_SLIDE_SYNC: u16 = 0x0414;

    // OfficeArt.
    pub const DGG_CONTAINER: u16 = 0xF000;
    pub const BSTORE: u16 = 0xF001;
    pub const DG_CONTAINER: u16 = 0xF002;
    pub const SPGR_CONTAINER: u16 = 0xF003;
    pub const SP_CONTAINER: u16 = 0xF004;
    pub const BSE: u16 = 0xF007;
    pub const FSPGR: u16 = 0xF009;
    pub const FSP: u16 = 0xF00A;
    pub const FOPT: u16 = 0xF00B;
    pub const CLIENT_TEXTBOX: u16 = 0xF00D;
    pub const CHILD_ANCHOR: u16 = 0xF00F;
    pub const CLIENT_ANCHOR: u16 = 0xF010;
    pub const CLIENT_DATA: u16 = 0xF011;
    pub const SECONDARY_FOPT: u16 = 0xF121;
    pub const TERTIARY_FOPT: u16 = 0xF122;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_records() {
        // A container holding an atom of 2 bytes and an empty atom.
        let mut b = vec![0x0F, 0, 0xE8, 0x03, 18, 0, 0, 0];
        b.extend_from_slice(&[0x00, 0, 0xE9, 0x03, 2, 0, 0, 0, 7, 9]);
        b.extend_from_slice(&[0x10, 0, 0xF3, 0x03, 0, 0, 0, 0]);
        let (r, rest) = rec_at(&b).unwrap();
        assert!(rest.is_empty() && r.is_container() && r.ty == ty::DOCUMENT);
        let kids: Vec<_> = r.kids().collect();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].body, &[7, 9]);
        assert_eq!(kids[1].inst, 1);
        assert!(r.child(ty::SLIDE_PERSIST_ATOM).is_some());
        // A length past the end is cut, not trusted.
        let short = [0x0F, 0, 0xE8, 0x03, 0xFF, 0xFF, 0, 0, 1];
        assert_eq!(rec_at(&short).unwrap().0.body, &[1]);
    }
}
