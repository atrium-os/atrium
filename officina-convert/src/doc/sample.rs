//! A small Word 97 document built byte by byte, for tests here and in the
//! apps: "Hello from Word 97" (with "Hello" bold) and "Second paragraph",
//! optionally protected with a password (RC4 CryptoAPI, as Word and
//! LibreOffice write it).

use super::bin;

const TEXT: &[u8] = b"Hello from Word 97\rSecond paragraph\r";
/// Where the text starts in the WordDocument stream, and its one
/// character-property page.
const TEXT_AT: u32 = 0x400;
const CHPX_PAGE: u32 = 0x600;

fn u16s(out: &mut Vec<u8>, xs: &[u16]) {
    for x in xs {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

fn u32s(out: &mut Vec<u8>, xs: &[u32]) {
    for x in xs {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

/// The document as a `.doc` file.
pub fn document(password: Option<&str>) -> Vec<u8> {
    let crypt = password.map(crate::xls::sample::crypto_api);
    // The table stream: the encryption header first, then the piece table
    // and the character properties' bin table.
    let mut table = crypt.as_ref().map(|(_, h)| h.clone()).unwrap_or_default();
    let l_key = table.len() as u32;
    let clx_at = table.len() as u32;
    let n = TEXT.len() as u32;
    table.push(2);
    u32s(&mut table, &[4 * 2 + 8, 0, n]);
    table.extend_from_slice(&0u16.to_le_bytes());
    u32s(&mut table, &[(TEXT_AT * 2) | 0x4000_0000]);
    table.extend_from_slice(&0u16.to_le_bytes());
    let clx_len = table.len() as u32 - clx_at;
    let chpx_at = table.len() as u32;
    u32s(&mut table, &[TEXT_AT, TEXT_AT + n, CHPX_PAGE / 512]);
    let chpx_len = table.len() as u32 - chpx_at;

    // WordDocument: the FIB, the text, one CHPX page ("Hello" bold).
    let mut w = Vec::new();
    let flags: u16 = 0x0004 | if crypt.is_some() { 0x0100 } else { 0 };
    u16s(&mut w, &[0xA5EC, 0x00C1, 0, 0x0409, 0, flags, 0x00BF]);
    u32s(&mut w, &[l_key]);
    w.resize(32, 0);
    // fibRgW (14 shorts), fibRgLw (22 longs), fibRgFcLcb97 (93 pairs).
    w.extend_from_slice(&14u16.to_le_bytes());
    w.resize(w.len() + 28, 0);
    w.extend_from_slice(&22u16.to_le_bytes());
    let lw = w.len();
    w.resize(lw + 88, 0);
    w[lw + 12..lw + 16].copy_from_slice(&n.to_le_bytes());
    w.extend_from_slice(&93u16.to_le_bytes());
    let fc = w.len();
    w.resize(fc + 93 * 8, 0);
    let mut set = |i: usize, at: u32, len: u32| {
        w[fc + i * 8..fc + i * 8 + 4].copy_from_slice(&at.to_le_bytes());
        w[fc + i * 8 + 4..fc + i * 8 + 8].copy_from_slice(&len.to_le_bytes());
    };
    set(bin::fc::CLX, clx_at, clx_len);
    set(bin::fc::PLCF_BTE_CHPX, chpx_at, chpx_len);
    w.extend_from_slice(&0u16.to_le_bytes());
    w.resize(TEXT_AT as usize, 0);
    w.extend_from_slice(TEXT);
    w.resize(CHPX_PAGE as usize, 0);
    // The page: run boundaries, each run's CHPX offset (in words), the
    // CHPXs at the page's end, and the run count last.
    let mut page = vec![0u8; 512];
    let runs = [TEXT_AT, TEXT_AT + 5, TEXT_AT + n];
    for (i, f) in runs.iter().enumerate() {
        page[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    let bold_at = 500usize;
    page[bold_at..bold_at + 4].copy_from_slice(&[3, 0x35, 0x08, 1]);
    page[12] = (bold_at / 2) as u8;
    page[13] = 0;
    page[511] = 2;
    w.extend_from_slice(&page);

    let mut data = Vec::new();
    if let Some((cipher, _)) = &crypt {
        // RC4 is its own inverse.
        bin::rc4_stream(&mut w, cipher, 68);
        bin::rc4_stream(&mut table, cipher, l_key as usize);
        bin::rc4_stream(&mut data, cipher, 0);
    }
    crate::cfb::write(&[("WordDocument", &w), ("0Table", &table)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_reads_back() {
        let (doc, _) = crate::doc::read(&document(None), None).unwrap();
        let text = doc.plain_text();
        assert!(text.contains("Hello from Word 97"), "{text:?}");
        assert!(text.contains("Second paragraph"), "{text:?}");
        let locked = document(Some("pa55"));
        assert!(matches!(crate::doc::read(&locked, None), Err(crate::doc::DocError::NeedsPassword)));
        assert!(matches!(crate::doc::read(&locked, Some("pass")), Err(crate::doc::DocError::WrongPassword)));
        let (doc, _) = crate::doc::read(&locked, Some("pa55")).unwrap();
        assert!(doc.plain_text().contains("Second paragraph"));
    }
}
