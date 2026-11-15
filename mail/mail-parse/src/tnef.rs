//! TNEF (`winmail.dat`, `application/ms-tnef`) from Outlook: unpacks the
//! attachments and the plain body so they show as ordinary attachments
//! (docs/spec/mail.md §7.1). Read only; nothing in it runs.

const SIGNATURE: u32 = 0x223E_9F78;
const ATT_BODY: u16 = 0x800C;
const ATT_ATTACH_DATA: u16 = 0x800F;
const ATT_ATTACH_TITLE: u16 = 0x8010;
const ATT_ATTACH_REND_DATA: u16 = 0x9002;
const ATT_ATTACHMENT: u16 = 0x9005;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tnef {
    pub body: Option<String>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotTnef,
    Truncated,
    TooMany,
}

fn u16le(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    // TNEF strings are in the sender's ANSI code page; windows-1252 is the
    // common case and never fails
    crate::decode::to_utf8(&b[..end], Some("windows-1252")).0
}

/// Unpacks at most `max_attachments`.
pub fn unpack(b: &[u8], max_attachments: usize) -> Result<Tnef, Error> {
    if u32le(b, 0) != Some(SIGNATURE) {
        return Err(Error::NotTnef);
    }
    let mut out = Tnef::default();
    let mut i = 6; // signature + key
    let mut cur: Option<Attachment> = None;
    while i < b.len() {
        let _level = *b.get(i).ok_or(Error::Truncated)?;
        let id = u32le(b, i + 1).ok_or(Error::Truncated)?;
        let len = u32le(b, i + 5).ok_or(Error::Truncated)? as usize;
        let start = i + 9;
        let data = b.get(start..start.checked_add(len).ok_or(Error::Truncated)?).ok_or(Error::Truncated)?;
        let tag = (id & 0xffff) as u16;
        match tag {
            ATT_ATTACH_REND_DATA => {
                if let Some(a) = cur.take() {
                    out.attachments.push(a);
                }
                if out.attachments.len() >= max_attachments {
                    return Err(Error::TooMany);
                }
                cur = Some(Attachment { name: String::new(), data: Vec::new() });
            }
            ATT_ATTACH_TITLE => {
                if let Some(a) = cur.as_mut() {
                    if a.name.is_empty() {
                        a.name = cstr(data);
                    }
                }
            }
            ATT_ATTACH_DATA => {
                if let Some(a) = cur.as_mut() {
                    a.data = data.to_vec();
                }
            }
            ATT_ATTACHMENT => {
                if let (Some(a), Some(long)) = (cur.as_mut(), long_filename(data)) {
                    a.name = long;
                }
            }
            ATT_BODY if out.body.is_none() => out.body = Some(cstr(data)),
            _ => {}
        }
        i = start + len + 2; // checksum
    }
    if let Some(a) = cur.take() {
        out.attachments.push(a);
    }
    for a in out.attachments.iter_mut() {
        a.name = crate::mime::sanitize_filename(&a.name);
        if a.name.is_empty() {
            a.name = "attachment".into();
        }
    }
    Ok(out)
}

/// `PR_ATTACH_LONG_FILENAME` from an attAttachment MAPI property list.
/// Only string properties are walked far enough to find it; anything odd
/// stops the walk (the short title is kept then).
fn long_filename(b: &[u8]) -> Option<String> {
    let count = u32le(b, 0)? as usize;
    let mut i = 4;
    for _ in 0..count.min(256) {
        let ptype = u16le(b, i)?;
        let pid = u16le(b, i + 2)?;
        i += 4;
        if pid >= 0x8000 {
            return None; // named properties: not walked
        }
        let fixed = match ptype {
            0x0002 | 0x000B => 4, // short, bool (padded)
            0x0003 | 0x0004 => 4,
            0x0005 | 0x0007 | 0x0014 | 0x0040 | 0x0006 => 8,
            0x001E | 0x001F | 0x0102 | 0x000D => 0,
            _ => return None,
        };
        if fixed > 0 {
            i += fixed;
            continue;
        }
        let n = u32le(b, i)? as usize; // value count
        i += 4;
        for _ in 0..n.min(16) {
            let len = u32le(b, i)? as usize;
            i += 4;
            let v = b.get(i..i.checked_add(len)?)?;
            if pid == PR_ATTACH_LONG_FILENAME {
                return Some(match ptype {
                    0x001F => {
                        let u: Vec<u16> = v.chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                        String::from_utf16_lossy(&u)
                    }
                    _ => cstr(v),
                });
            }
            i += (len + 3) & !3;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(level: u8, tag: u16, ty: u16, data: &[u8]) -> Vec<u8> {
        let mut v = vec![level];
        v.extend_from_slice(&((ty as u32) << 16 | tag as u32).to_le_bytes());
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        let sum: u32 = data.iter().map(|&c| c as u32).sum();
        v.extend_from_slice(&((sum & 0xffff) as u16).to_le_bytes());
        v
    }

    fn sample() -> Vec<u8> {
        let mut b = SIGNATURE.to_le_bytes().to_vec();
        b.extend_from_slice(&0x1234u16.to_le_bytes());
        b.extend(attr(1, ATT_BODY, 0x0001, b"Hello from Outlook\0"));
        b.extend(attr(2, ATT_ATTACH_REND_DATA, 0x0006, &[0; 14]));
        b.extend(attr(2, ATT_ATTACH_TITLE, 0x0001, b"REPORT~1.PDF\0"));
        b.extend(attr(2, ATT_ATTACH_DATA, 0x0006, b"%PDF-1.4\n"));
        // MAPI list: one PT_UNICODE PR_ATTACH_LONG_FILENAME
        let name: Vec<u8> = "../Quarterly Report é.pdf\0".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let mut props = 1u32.to_le_bytes().to_vec();
        props.extend_from_slice(&0x001Fu16.to_le_bytes());
        props.extend_from_slice(&PR_ATTACH_LONG_FILENAME.to_le_bytes());
        props.extend_from_slice(&1u32.to_le_bytes());
        props.extend_from_slice(&(name.len() as u32).to_le_bytes());
        props.extend_from_slice(&name);
        b.extend(attr(2, ATT_ATTACHMENT, 0x0006, &props));
        b.extend(attr(2, ATT_ATTACH_REND_DATA, 0x0006, &[0; 14]));
        b.extend(attr(2, ATT_ATTACH_TITLE, 0x0001, b"run.bat\0"));
        b.extend(attr(2, ATT_ATTACH_DATA, 0x0006, b"echo hi"));
        b
    }

    #[test]
    fn unpacks() {
        let t = unpack(&sample(), 16).unwrap();
        assert_eq!(t.body.as_deref(), Some("Hello from Outlook"));
        assert_eq!(t.attachments.len(), 2);
        assert_eq!(t.attachments[0], Attachment { name: "Quarterly Report é.pdf".into(), data: b"%PDF-1.4\n".to_vec() });
        assert_eq!(t.attachments[1].name, "run.bat");
    }

    #[test]
    fn refuses() {
        assert_eq!(unpack(b"nope", 16), Err(Error::NotTnef));
        let s = sample();
        assert_eq!(unpack(&s[..s.len() - 5], 16), Err(Error::Truncated));
        assert_eq!(unpack(&s, 1), Err(Error::TooMany));
        // a huge length can't overflow
        let mut b = SIGNATURE.to_le_bytes().to_vec();
        b.extend_from_slice(&[0, 0, 1, 0x0c, 0x80, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(unpack(&b, 16), Err(Error::Truncated));
    }
}
