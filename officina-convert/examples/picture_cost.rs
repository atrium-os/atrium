//! What decoding in the worker costs against decoding in-process, on the
//! host: `cargo run --release --example picture_cost` (the converter binary
//! next to it). Prints milliseconds for the first picture (which starts the
//! process) and the ones after.

use std::time::Instant;

use navigator_image::image::{decode_unaddressed, Ceilings};
use officina_convert::pictures::Remote;
use officina_text::picture::Decoder;

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&[(x * 7) as u8, (y * 3) as u8, ((x ^ y) & 255) as u8]);
        }
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    let crc = |b: &[u8]| {
        let mut c = !0u32;
        for &x in b {
            c ^= x as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    };
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

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let conv = exe.parent().unwrap().parent().unwrap().join("officina-convert");
    std::env::set_var("OFFICINA_CONVERT", &conv);
    let pic = png(2000, 1500);
    println!("picture: {} KB encoded", pic.len() / 1024);
    let target = Some((800, 600));
    for _ in 0..2 {
        let _ = decode_unaddressed(&pic, Ceilings { target, ..Ceilings::default() });
    }
    let t = Instant::now();
    let n = 10;
    for _ in 0..n {
        decode_unaddressed(&pic, Ceilings { target, ..Ceilings::default() }).unwrap();
    }
    println!("in-process: {:.1} ms a picture (2000x1500 PNG to 800x600)", ms(t) / n as f64);
    let r = Remote::default();
    let t = Instant::now();
    r.decode(&pic, target).unwrap();
    println!("worker, first picture (starts it): {:.1} ms", ms(t));
    let t = Instant::now();
    for _ in 0..n {
        r.decode(&pic, target).unwrap();
    }
    println!("worker, after: {:.1} ms a picture", ms(t) / n as f64);
}
