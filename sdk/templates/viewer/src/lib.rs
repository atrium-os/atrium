//! {{title}}: what the core and its decode worker share (template: viewer).
//!
//! The core never reads the bytes of a file the person opens. It hands the
//! descriptor to the `decode` worker (atrium.toml's `[[workers]]`), which
//! runs in a jail with no capabilities, and gets back a [`Picture`] whose
//! bounds were checked as it arrived.
//!
//! `cargo atrium fuzz worker:decode` fuzzes [`workers::decode`] with no
//! setup: each input is the opened file. Replace the BMP decoder with
//! yours (or with any crate: in the worker it is caged) and keep the shape.

use atrium_message::Message;

/// The largest file the worker reads.
pub const MAX_FILE: u64 = 64 << 20;
/// The largest side of a picture sent back.
pub const MAX_SIDE: u32 = 4096;

/// What the core asks for.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Ask {
    /// Scale the picture down to fit this many pixels a side.
    #[message(min = 1, max = 4096)]
    pub max_side: u32,
}

/// What a fuzz run asks for (the input is the file).
impl Default for Ask {
    fn default() -> Self { Ask { max_side: 1024 } }
}

/// What the worker answers.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Picture {
    #[message(min = 1, max = 4096)]
    pub width: u32,
    #[message(min = 1, max = 4096)]
    pub height: u32,
    /// RGBA8, width × height × 4 bytes.
    #[message(max_len = 67_108_864)]
    pub rgba: Vec<u8>,
}

impl Picture {
    /// What no bound can say: that the pixels fit the size. The core checks
    /// it before it draws anything.
    pub fn is_whole(&self) -> bool {
        self.rgba.len() as u64 == self.width as u64 * self.height as u64 * 4
    }
}

/// The workers' handlers: `cargo atrium fuzz` finds `workers::<name>` for
/// each `[[workers]]` entry.
pub mod workers {
    use super::*;
    use std::fs::File;
    use std::io::Read;

    /// The decode worker: the file the person opened, a picture out.
    pub fn decode(ask: Ask, file: Option<File>) -> Result<Picture, String> {
        let file = file.ok_or("no file")?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_FILE {
            return Err("the file is larger than 64 MiB".into());
        }
        bmp::decode(&bytes, ask.max_side.clamp(1, MAX_SIDE))
    }
}

/// Uncompressed BMP, 24 or 32 bits a pixel, read straight into a picture
/// at most `max_side` pixels a side.
pub mod bmp {
    use super::Picture;

    fn u16_at(b: &[u8], o: usize) -> Result<u16, String> {
        b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| "the file ends early".into())
    }
    fn u32_at(b: &[u8], o: usize) -> Result<u32, String> {
        b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| "the file ends early".into())
    }

    pub fn decode(b: &[u8], max_side: u32) -> Result<Picture, String> {
        if b.get(0..2) != Some(b"BM".as_slice()) {
            return Err("not a BMP picture".into());
        }
        let offset = u32_at(b, 10)? as u64;
        if u32_at(b, 14)? < 40 {
            return Err("an OS/2 BMP header isn't read".into());
        }
        let width = u32_at(b, 18)? as i32;
        let height = u32_at(b, 22)? as i32;
        let bpp = u16_at(b, 28)?;
        if u32_at(b, 30)? != 0 {
            return Err("compressed BMP isn't read".into());
        }
        if bpp != 24 && bpp != 32 {
            return Err(format!("{bpp} bits a pixel isn't read (24 or 32)"));
        }
        let (w, h) = (width.unsigned_abs() as u64, height.unsigned_abs() as u64);
        if width <= 0 || h == 0 {
            return Err("the picture has no pixels".into());
        }
        let bytes_pp = bpp as u64 / 8;
        let stride = (w * bytes_pp + 3) & !3;
        if offset.checked_add(stride * h).is_none_or(|end| end > b.len() as u64) {
            return Err("the pixels run past the end of the file".into());
        }
        // Nearest-neighbour down to max_side; never up.
        let side = w.max(h);
        let (ow, oh) = if side <= max_side as u64 {
            (w, h)
        } else {
            ((w * max_side as u64 / side).max(1), (h * max_side as u64 / side).max(1))
        };
        let mut rgba = Vec::with_capacity((ow * oh * 4) as usize);
        for y in 0..oh {
            let sy = y * h / oh;
            let row = if height < 0 { sy } else { h - 1 - sy };
            let base = offset + row * stride;
            for x in 0..ow {
                let p = (base + (x * w / ow) * bytes_pp) as usize;
                rgba.extend_from_slice(&[b[p + 2], b[p + 1], b[p], 255]);
            }
        }
        Ok(Picture { width: ow as u32, height: oh as u32, rgba })
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn the_seed_decodes() {
            let p = super::decode(include_bytes!("../fuzz/corpus/worker-decode/seed.bmp"), 4096).unwrap();
            assert_eq!((p.width, p.height), (2, 2));
            assert!(p.is_whole());
            assert!(super::decode(b"BM", 4096).is_err());
        }
    }
}
