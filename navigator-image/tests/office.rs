//! Office's picture formats, with the `office` feature (Officina only).
#![cfg(feature = "office")]

use navigator_image::image::{decode_unaddressed, natural_size, sniff, Ceilings, Format};

/// A placeable WMF, 2 × 1 inches, filled red.
fn red_wmf() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(0x9AC6_CDD7u32.to_le_bytes());
    b.extend([0, 0]);
    for v in [0i16, 0, 2880, 1440] {
        b.extend(v.to_le_bytes());
    }
    b.extend(1440u16.to_le_bytes());
    b.extend([0; 6]);
    b.extend([1, 0, 9, 0, 0, 3, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
    let rec = |b: &mut Vec<u8>, f: u16, p: &[i16]| {
        b.extend((3 + p.len() as u32).to_le_bytes());
        b.extend(f.to_le_bytes());
        for v in p {
            b.extend(v.to_le_bytes());
        }
    };
    rec(&mut b, 0x02FC, &[0, 0xFF, 0, 0]);
    rec(&mut b, 0x012D, &[0]);
    rec(&mut b, 0x041B, &[1440, 2880, 0, 0]);
    rec(&mut b, 0, &[]);
    b
}

#[test]
fn metafiles_draw_at_the_painted_size() {
    let b = red_wmf();
    assert_eq!(sniff(&b), Ok(Format::Wmf));
    assert_eq!(natural_size(&b), Ok((Format::Wmf, 192, 96)));
    let px = decode_unaddressed(&b, Ceilings { target: Some((800, 800)), ..Ceilings::default() }).unwrap();
    assert_eq!((px.width, px.height), (800, 400), "drawn at the target, not scaled up from 192");
    assert_eq!(&px.rgba[(200 * 800 + 400) * 4..][..4], &[255, 0, 0, 255]);
}

#[test]
fn bitmaps_decode() {
    let mut b = b"BM".to_vec();
    b.extend(((14 + 40 + 4) as u32).to_le_bytes());
    b.extend([0; 4]);
    b.extend(54u32.to_le_bytes());
    for v in [40u32, 1, 1] {
        b.extend(v.to_le_bytes());
    }
    b.extend([1, 0, 24, 0]);
    b.extend([0; 24]);
    b.extend([255, 0, 0, 0]);
    assert_eq!(sniff(&b), Ok(Format::Bmp));
    let px = decode_unaddressed(&b, Ceilings::default()).unwrap();
    assert_eq!(px.rgba, vec![0, 0, 255, 255]);
}
