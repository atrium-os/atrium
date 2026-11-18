//! Pictures decoded by the converter's long-lived `pictures` mode, through
//! the decoder an app installs (src/pictures.rs).

use officina_convert::pictures::{Remote, MAX_PROCS};
use officina_text::picture::Decoder;

fn crc(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn adler(b: &[u8]) -> u32 {
    let (mut a, mut s) = (1u32, 0u32);
    for &x in b {
        a = (a + x as u32) % 65521;
        s = (s + a) % 65521;
    }
    (s << 16) | a
}

/// A `w` × `h` RGB PNG, red to the left of the middle, blue to the right.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(if x < w / 2 { &[255, 0, 0] } else { &[0, 0, 255] });
        }
    }
    // Stored deflate blocks in a zlib stream.
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push((i + 1 == chunks.len()) as u8);
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |t: &[u8], d: &[u8]| {
        out.extend_from_slice(&(d.len() as u32).to_be_bytes());
        let mut td = t.to_vec();
        td.extend_from_slice(d);
        out.extend_from_slice(&td);
        out.extend_from_slice(&crc(&td).to_be_bytes());
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

#[test]
fn sizes_and_decodes_in_the_converter() {
    std::env::set_var("OFFICINA_CONVERT", env!("CARGO_BIN_EXE_officina-convert"));
    let r = Remote::default();
    let p = png(64, 32);
    assert_eq!(r.size(&p), Some((64, 32)));
    let px = r.decode(&p, None).expect("decoded");
    assert_eq!((px.width, px.height), (64, 32));
    assert_eq!(&px.rgba[..4], &[255, 0, 0, 255]);
    assert_eq!(&px.rgba[(63 * 4)..(64 * 4)], &[0, 0, 255, 255]);
    // At about the drawn size, never above the natural one.
    let small = r.decode(&p, Some((16, 8))).expect("decoded small");
    assert_eq!((small.width, small.height), (16, 8));
    // Not a picture: an answer, not a dead process.
    assert!(r.decode(b"hello, not a picture", None).is_none());
    assert_eq!(r.size(b"<html><svg/></html>"), None);
    assert_eq!(r.running(), 1, "one process served all of it");
    // Several threads at once: at most MAX_PROCS processes.
    let r = std::sync::Arc::new(r);
    let hs: Vec<_> = (0..8).map(|i| {
        let (r, p) = (r.clone(), png(40 + i, 20));
        std::thread::spawn(move || r.decode(&p, None).map(|x| x.width))
    }).collect();
    for (i, h) in hs.into_iter().enumerate() {
        assert_eq!(h.join().unwrap(), Some(40 + i as u32));
    }
    assert!(r.running() <= MAX_PROCS);
}
