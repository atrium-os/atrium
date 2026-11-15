//! A small PowerPoint 97 deck built record by record, for tests here and in
//! the apps: one master; one slide with the title "Hello from PowerPoint 97"
//! (a placeholder whose text is in the slide list) and a text box "Second
//! shape" in bold; the notes "Speaker notes". Optionally protected with a
//! password (RC4 CryptoAPI, as PowerPoint and LibreOffice write it).

use super::rec::ty;
use crate::xls::biff::Rc4;

fn rec(ver: u8, inst: u16, t: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(ver as u16 | inst << 4).to_le_bytes());
    out.extend_from_slice(&t.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn atom(inst: u16, t: u16, body: &[u8]) -> Vec<u8> {
    rec(0, inst, t, body)
}

fn cont(inst: u16, t: u16, kids: &[Vec<u8>]) -> Vec<u8> {
    rec(0xF, inst, t, &kids.concat())
}

fn le32(xs: &[u32]) -> Vec<u8> {
    xs.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// A shape: type, anchor (left, top, right, bottom), placeholder, text records.
fn shape(spid: u32, spt: u16, a: [i16; 4], ph: Option<u8>, text: Vec<Vec<u8>>) -> Vec<u8> {
    let mut kids = vec![atom(spt, ty::FSP, &le32(&[spid, 0x0A00]))];
    // No fill or line: fFilled and fLine off (with their fUse bits).
    let mut opt = Vec::new();
    for (id, v) in [(0x01BFu16, 0x0010_0000u32), (0x01FF, 0x0008_0000)] {
        opt.extend_from_slice(&id.to_le_bytes());
        opt.extend_from_slice(&v.to_le_bytes());
    }
    kids.push(rec(3, 2, ty::FOPT, &opt));
    let anchor: Vec<u8> = [a[1], a[0], a[2], a[3]].iter().flat_map(|x| x.to_le_bytes()).collect();
    kids.push(atom(0, ty::CLIENT_ANCHOR, &anchor));
    if let Some(p) = ph {
        kids.push(cont(0, ty::CLIENT_DATA, &[atom(0, ty::PLACEHOLDER, &[0, 0, 0, 0, p, 0, 0, 0])]));
    }
    if !text.is_empty() {
        kids.push(cont(0, ty::CLIENT_TEXTBOX, &text));
    }
    cont(0, ty::SP_CONTAINER, &kids)
}

fn drawing(shapes: Vec<Vec<u8>>) -> Vec<u8> {
    let patriarch = cont(0, ty::SP_CONTAINER, &[atom(1, ty::FSPGR, &[0; 16]), atom(0, ty::FSP, &le32(&[1024, 0x05]))]);
    let mut kids = vec![patriarch];
    kids.extend(shapes);
    let sg = cont(0, ty::SPGR_CONTAINER, &kids);
    cont(0, ty::PP_DRAWING, &[cont(0, ty::DG_CONTAINER, &[atom(1, 0xF008, &le32(&[3, 1027])), sg])])
}

fn slide_atom(master: u32, notes: u32) -> Vec<u8> {
    let mut b = vec![0u8; 24];
    b[12..16].copy_from_slice(&master.to_le_bytes());
    b[16..20].copy_from_slice(&notes.to_le_bytes());
    b
}

fn persist_atom(id: u32, slide_id: u32) -> Vec<u8> {
    atom(0, ty::SLIDE_PERSIST_ATOM, &le32(&[id, 0, 0, slide_id, 0]))
}

/// The deck as a `.ppt` file.
pub fn deck(password: Option<&str>) -> Vec<u8> {
    // Persist ids: 1 document, 2 master, 3 slide, 4 notes, 5 encryption.
    let title = "Hello from PowerPoint 97";
    let text_bold = "Second shape";
    let scheme = atom(1, ty::COLOR_SCHEME, &le32(&[0xFFFFFF, 0x000000, 0x808080, 0x000000, 0xE3E0BB, 0x993333, 0x999900, 0x00CC99]));
    let master = cont(0, ty::MAIN_MASTER, &[atom(0, ty::SLIDE_ATOM, &slide_atom(0, 0)), drawing(Vec::new()), scheme.clone()]);
    // The title's text lives in the slide list; the text box's in the shape.
    let mut style = le32(&[text_bold.len() as u32 + 1]);
    style.extend_from_slice(&0u16.to_le_bytes());
    style.extend_from_slice(&0u32.to_le_bytes());
    style.extend_from_slice(&le32(&[text_bold.len() as u32 + 1, 1]));
    style.extend_from_slice(&1u16.to_le_bytes());
    let slide = cont(
        0,
        ty::SLIDE,
        &[
            atom(2, ty::SLIDE_ATOM, &slide_atom(0x8000_0001, 0x100)),
            drawing(vec![
                shape(1026, 1, [576, 288, 5184, 1152], Some(13), vec![atom(0, ty::OUTLINE_TEXT_REF, &le32(&[0]))]),
                shape(1027, 202, [576, 1440, 5184, 2304], None, vec![atom(0, ty::TEXT_HEADER, &le32(&[4])), atom(0, ty::TEXT_CHARS, &utf16(text_bold)), atom(0, ty::STYLE_TEXT_PROP, &style)]),
            ]),
        ],
    );
    let notes = cont(
        0,
        ty::NOTES,
        &[atom(1, ty::NOTES_ATOM, &le32(&[0x100, 0])), drawing(vec![shape(1028, 1, [576, 2304, 5184, 4032], Some(12), vec![atom(0, ty::TEXT_HEADER, &le32(&[2])), atom(0, ty::TEXT_BYTES, b"Speaker notes")])])],
    );
    let font = {
        let mut b = utf16("Arial");
        b.resize(68, 0);
        b
    };
    let document = cont(
        0,
        ty::DOCUMENT,
        &[
            atom(1, ty::DOCUMENT_ATOM, &[le32(&[5760, 4320, 4320, 5760, 1, 2]), vec![0; 16]].concat()),
            cont(0, ty::ENVIRONMENT, &[cont(0, ty::FONT_COLLECTION, &[atom(0, ty::FONT_ENTITY, &font)])]),
            cont(1, ty::SLIDE_LIST_WITH_TEXT, &[persist_atom(2, 0x8000_0001)]),
            cont(0, ty::SLIDE_LIST_WITH_TEXT, &[persist_atom(3, 0x100), atom(0, ty::TEXT_HEADER, &le32(&[0])), atom(0, ty::TEXT_CHARS, &utf16(title))]),
            cont(2, ty::SLIDE_LIST_WITH_TEXT, &[persist_atom(4, 0x100)]),
        ],
    );
    let crypt = password.map(crate::xls::sample::crypto_api);
    let mut objs: Vec<(u32, Vec<u8>)> = vec![(1, document), (2, master), (3, slide), (4, notes)];
    if let Some((_, header)) = &crypt {
        objs.push((5, rec(0xF, 0, ty::CRYPT_SESSION, header)));
    }
    let mut s = Vec::new();
    let mut offsets = Vec::new();
    for (id, b) in &objs {
        offsets.push(s.len() as u32);
        let mut b = b.clone();
        if let (Some((k, _)), true) = (&crypt, *id != 5) {
            Rc4::new(&k.block_key(*id)).apply(&mut b);
        }
        s.extend_from_slice(&b);
    }
    let dir_at = s.len() as u32;
    let mut dir = le32(&[1 | (objs.len() as u32) << 20]);
    dir.extend_from_slice(&le32(&offsets));
    s.extend_from_slice(&atom(0, ty::PERSIST_DIRECTORY, &dir));
    let edit_at = s.len() as u32;
    let mut edit = le32(&[0x100, 0x0300_0000 | 0x2F7E, 0, dir_at, 1, objs.len() as u32 + 1, 1]);
    if crypt.is_some() {
        edit.extend_from_slice(&le32(&[5]));
    }
    s.extend_from_slice(&atom(0, ty::USER_EDIT, &edit));
    let token: u32 = if crypt.is_some() { 0xF3D1_C4DF } else { 0xE391_C05F };
    let mut cu = le32(&[0x14, token, edit_at]);
    cu.extend_from_slice(&[0, 0, 0xF4, 0x03, 3, 0, 0, 0]);
    let cu = atom(0, ty::CURRENT_USER, &cu);
    crate::cfb::write(&[("Current User", &cu), ("PowerPoint Document", &s)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ppt::{read, PptError};

    fn texts(d: &mut officina_slides::model::Deck) -> (Vec<String>, String) {
        let s = d.slide(0).unwrap();
        let t = s.shapes.iter().filter_map(|x| x.text()).map(|t| t.text()).collect();
        (t, s.notes.as_ref().map(|n| n.text()).unwrap_or_default())
    }

    #[test]
    fn the_sample_reads_back() {
        let (mut d, _) = read(&deck(None), None).unwrap();
        assert_eq!(d.slide_count(), 1);
        let (t, notes) = texts(&mut d);
        assert!(t.iter().any(|x| x.contains("Hello from PowerPoint 97")), "{t:?}");
        assert!(t.iter().any(|x| x.contains("Second shape")), "{t:?}");
        assert!(notes.contains("Speaker notes"), "{notes:?}");
        let locked = deck(Some("pa55"));
        assert!(!locked.windows(6).any(|w| w == b"Second" || w == b"Speake"));
        assert_eq!(read(&locked, None).err(), Some(PptError::NeedsPassword));
        assert_eq!(read(&locked, Some("pass")).err(), Some(PptError::WrongPassword));
        let (mut d, _) = read(&locked, Some("pa55")).unwrap();
        let (t, notes) = texts(&mut d);
        assert!(t.iter().any(|x| x.contains("Second shape")) && notes.contains("Speaker notes"));
    }
}
