//! The file around the records (MS-PPT §2.1–2.3): the `Current User` stream
//! points at the last edit; edits chain back, each with a persist directory
//! mapping object ids to offsets in the `PowerPoint Document` stream (newer
//! edits win). A protected file encrypts each persist object with RC4 keyed
//! by its id, and each picture with block 0.

use std::collections::HashMap;

use super::rec::{self, rec_at, ty, u32_at, Rec};
use crate::cfb::Cfb;
use crate::crypt::CryptError;
use crate::xls::biff::{self, Cipher, Cur, Rc4};

/// Why a file can't be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum Open {
    NeedsPassword,
    WrongPassword,
    Bad(String),
}

fn bad<T>(s: impl Into<String>) -> Result<T, Open> {
    Err(Open::Bad(s.into()))
}

/// Most edits followed back through the file.
const MAX_EDITS: usize = 4096;
/// Most persist objects.
const MAX_PERSIST: usize = 1 << 20;

pub struct PptFile {
    /// The `PowerPoint Document` stream, decrypted.
    pub doc: Vec<u8>,
    /// The `Pictures` stream (decrypted picture by picture when read).
    pub pictures: Vec<u8>,
    pub persist: HashMap<u32, u32>,
    pub doc_ref: u32,
    pub cipher: Option<Cipher>,
}

impl PptFile {
    pub fn open(data: &[u8], password: Option<&str>) -> Result<PptFile, Open> {
        let c = Cfb::open(data).map_err(|e| Open::Bad(e.0))?;
        if !c.has("PowerPoint Document") {
            if c.has("PP97_DUALSTORAGE") || c.has("PP40") {
                return bad("PowerPoint 95 and older presentations aren't supported; save it from PowerPoint 97 or later");
            }
            return bad("no PowerPoint Document stream");
        }
        let mut doc = c.stream("PowerPoint Document").map_err(|e| Open::Bad(e.0))?;
        let pictures = if c.has("Pictures") { c.stream("Pictures").map_err(|e| Open::Bad(e.0))? } else { Vec::new() };
        // The last edit: from Current User, else the last UserEditAtom in the stream.
        let cu = if c.has("Current User") { c.stream("Current User").ok() } else { None };
        let mut encrypted = false;
        let mut at = None;
        if let Some((r, _)) = cu.as_deref().and_then(rec_at) {
            if r.ty == ty::CURRENT_USER && r.body.len() >= 12 {
                encrypted = u32_at(r.body, 4) == 0xF3D1_C4DF;
                at = Some(u32_at(r.body, 8) as usize);
            }
        }
        let edits = user_edits(&doc);
        if edits.is_empty() && cu.as_deref().and_then(rec_at).is_none_or(|(r, _)| r.ty != ty::CURRENT_USER) {
            return bad("PowerPoint 95 and older presentations aren't supported; save it from PowerPoint 97 or later");
        }
        let at = match at.filter(|&a| matches!(rec_at(doc.get(a..).unwrap_or(&[])), Some((r, _)) if r.ty == ty::USER_EDIT)) {
            Some(a) => a,
            None => *edits.last().ok_or_else(|| Open::Bad("no edit record".into()))?,
        };
        // Follow the edits back; the newest entry for an id wins.
        let mut persist: HashMap<u32, u32> = HashMap::new();
        let mut doc_ref = None;
        let mut crypt_ref = None;
        let mut seen = std::collections::HashSet::new();
        let mut edit = at;
        for _ in 0..MAX_EDITS {
            if !seen.insert(edit) {
                break;
            }
            let Some((r, _)) = rec_at(doc.get(edit..).unwrap_or(&[])) else { break };
            if r.ty != ty::USER_EDIT || r.body.len() < 28 {
                break;
            }
            let b = r.body;
            doc_ref.get_or_insert(u32_at(b, 16));
            if b.len() >= 32 && crypt_ref.is_none() {
                crypt_ref = Some(u32_at(b, 28));
            }
            let dir_at = u32_at(b, 12) as usize;
            if let Some((d, _)) = rec_at(doc.get(dir_at..).unwrap_or(&[])) {
                if d.ty == ty::PERSIST_DIRECTORY {
                    let mut p = 0;
                    while p + 4 <= d.body.len() && persist.len() < MAX_PERSIST {
                        let info = u32_at(d.body, p);
                        let (first, n) = (info & 0xF_FFFF, (info >> 20) as usize);
                        p += 4;
                        for k in 0..n {
                            if p + 4 > d.body.len() {
                                break;
                            }
                            persist.entry(first + k as u32).or_insert(u32_at(d.body, p));
                            p += 4;
                        }
                    }
                }
            }
            let back = u32_at(b, 8) as usize;
            if back == 0 {
                break;
            }
            // A damaged chain (pointing at itself, forward, or at no edit)
            // goes on from the edit before this one in the stream.
            let ok = back < edit && !seen.contains(&back) && edits.binary_search(&back).is_ok();
            edit = if ok {
                back
            } else {
                match edits.iter().rev().find(|&&e| e < edit && !seen.contains(&e)) {
                    Some(&e) => e,
                    None => break,
                }
            };
        }
        let Some(doc_ref) = doc_ref else { return bad("no edit record") };
        let mut cipher = None;
        if encrypted {
            let Some(pw) = password else { return Err(Open::NeedsPassword) };
            let cs = crypt_ref.and_then(|id| persist.get(&id)).and_then(|&o| rec_at(doc.get(o as usize..)?)).map(|x| x.0);
            let Some(cs) = cs.filter(|r| r.ty == ty::CRYPT_SESSION) else { return bad("the encryption record is missing") };
            let k = match biff::rc4_info(&mut Cur::of(cs.body), pw) {
                Ok(k) => k,
                Err(CryptError::WrongPassword) => return Err(Open::WrongPassword),
                Err(e) => return bad(e.to_string()),
            };
            let skip = crypt_ref.unwrap_or(u32::MAX);
            let mut ids: Vec<(u32, u32)> = persist.iter().filter(|(id, _)| **id != skip).map(|(a, b)| (*a, *b)).collect();
            ids.sort();
            for (id, off) in ids {
                decrypt_record(&mut doc, off as usize, &k, id);
            }
            cipher = Some(k);
        }
        Ok(PptFile { doc, pictures, persist, doc_ref, cipher })
    }

    /// The persist object `id`.
    pub fn obj(&self, id: u32) -> Option<Rec<'_>> {
        let o = *self.persist.get(&id)? as usize;
        rec_at(self.doc.get(o..)?).map(|x| x.0)
    }

    /// The picture record at `at` in the Pictures stream, decrypted.
    pub fn picture(&self, at: usize) -> Option<Vec<u8>> {
        let mut b = self.pictures.get(at..)?.to_vec();
        if let Some(k) = &self.cipher {
            decrypt_record(&mut b, 0, k, 0);
        }
        let (r, _) = rec_at(&b)?;
        let n = 8 + r.body.len();
        b.truncate(n);
        Some(b)
    }
}

/// Decrypts the record at `at` (header and body) in place.
fn decrypt_record(b: &mut [u8], at: usize, k: &Cipher, block: u32) {
    if at + 8 > b.len() {
        return;
    }
    let mut rc4 = Rc4::new(&k.block_key(block));
    rc4.apply(&mut b[at..at + 8]);
    let len = rec::u32_at(b, at + 4) as usize;
    let end = (at + 8).saturating_add(len).min(b.len());
    rc4.apply(&mut b[at + 8..end]);
}

/// Where the stream's top-level UserEditAtoms are, in order.
fn user_edits(doc: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut p = 0;
    let mut n = 0;
    while let Some((r, _)) = doc.get(p..).and_then(rec_at) {
        if r.ty == ty::USER_EDIT && out.len() < MAX_EDITS {
            out.push(p);
        }
        p += 8 + r.body.len();
        n += 1;
        if n > 1 << 20 {
            break;
        }
    }
    out
}
