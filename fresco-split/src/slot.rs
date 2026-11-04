//! The shared-memory frame slot: one packed `fresco_compute::Frame`.
//!
//! Layout (little endian, std430 structs as `fresco_compute` defines them):
//!
//! | bytes | what |
//! |---|---|
//! | 0..32 | [`SlotHeader`]: magic, seq, op, tex and texel counts |
//! | 32..96 | the frame `Header` |
//! | 96.. | `ops` (80 B each), then `tex` (16 B each), then `texels` (4 B each) |
//!
//! The broker allocates the slots, so it knows their size; it copies the
//! used bytes out first and only then parses that copy ([`read`]). The lut
//! is never read from the slot: the broker uses its own.

use fresco_compute::{bytes_of, lut, Frame, Header, Op};

pub const MAGIC: u32 = u32::from_le_bytes(*b"FRS1");
/// Default slot size: room for a 1080p RGBA texture and 64K ops.
pub const DEFAULT_SLOT_BYTES: u32 = 24 << 20;
const SLOT_HDR: usize = 32;
const FRAME_HDR: usize = 64;
const OP: usize = 80;

/// The slot's own header.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct SlotHeader {
    pub magic: u32,
    pub seq: u32,
    pub n_ops: u32,
    pub n_tex: u32,
    pub n_texels: u32,
    pub _pad: [u32; 3],
}

/// Bytes `f` takes in a slot.
pub fn bytes_needed(f: &Frame) -> usize {
    SLOT_HDR + FRAME_HDR + f.ops.len() * OP + f.tex.len() * 16 + f.texels.len() * 4
}

/// Write `f` (sequence `seq`) into `slot`. Renderer side.
pub fn write(slot: &mut [u8], f: &Frame, seq: u32) -> Result<(), &'static str> {
    if bytes_needed(f) > slot.len() {
        return Err("frame larger than the slot");
    }
    let h = SlotHeader {
        magic: MAGIC,
        seq,
        n_ops: f.ops.len() as u32,
        n_tex: f.tex.len() as u32,
        n_texels: f.texels.len() as u32,
        _pad: [0; 3],
    };
    let mut at = 0;
    for part in [bytes_of(&[h]), bytes_of(&[f.header]), bytes_of(&f.ops), bytes_of(&f.tex), bytes_of(&f.texels)] {
        slot[at..at + part.len()].copy_from_slice(&part);
        at += part.len();
    }
    Ok(())
}

fn words(b: &[u8]) -> impl Iterator<Item = u32> + '_ {
    b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()))
}

/// Copy the frame out of `slot` and check it. Broker side: `slot` may be
/// changing under us, so it is read once, by `copy_from_slice`, into
/// broker memory, and everything after looks only at that copy. The frame
/// must be for a `width` x `height` surface (pitch = width): the broker
/// decided that size, the renderer only fills it. On success the frame
/// has passed `Frame::validate` and carries the broker's own lut.
pub fn read(slot: &[u8], width: u32, height: u32) -> Result<(u32, Frame), String> {
    let bad = |m: &str| Err(format!("frame slot: {m}"));
    if slot.len() < SLOT_HDR + FRAME_HDR {
        return bad("slot too small");
    }
    let mut hdr = [0u8; SLOT_HDR + FRAME_HDR];
    hdr.copy_from_slice(&slot[..SLOT_HDR + FRAME_HDR]);
    let w: Vec<u32> = words(&hdr).collect();
    if w[0] != MAGIC {
        return bad("magic");
    }
    let (seq, n_ops, n_tex, n_texels) = (w[1], w[2] as usize, w[3] as usize, w[4] as usize);
    if w[5..8].iter().any(|&x| x != 0) {
        return bad("reserved words");
    }
    let body = (n_ops as u64) * OP as u64 + (n_tex as u64) * 16 + (n_texels as u64) * 4;
    if body > (slot.len() - SLOT_HDR - FRAME_HDR) as u64 || n_ops == 0 || n_tex == 0 || n_texels == 0 {
        return bad("counts past the slot, or an empty binding");
    }
    let mut data = vec![0u8; body as usize];
    data.copy_from_slice(&slot[SLOT_HDR + FRAME_HDR..SLOT_HDR + FRAME_HDR + body as usize]);

    let f = &w[8..];
    let header = Header {
        width: f[0],
        height: f[1],
        pitch: f[2],
        tiles_x: f[3],
        tiles_y: f[4],
        op_count: f[5],
        wpt: f[6],
        tex_count: f[7],
        texel_words: f[8],
        _pad: [f[9], f[10], f[11]],
        clear: [f32::from_bits(f[12]), f32::from_bits(f[13]), f32::from_bits(f[14]), f32::from_bits(f[15])],
    };
    if header.width != width || header.height != height || header.pitch != width {
        return bad("not the size the broker gave the window");
    }
    if header._pad != [0; 3] {
        return bad("header padding");
    }
    let (ops_b, rest) = data.split_at(n_ops * OP);
    let (tex_b, texel_b) = rest.split_at(n_tex * 16);
    let ops = ops_b
        .chunks_exact(OP)
        .map(|c| {
            let v: Vec<u32> = words(c).collect();
            let f4 = |i: usize| [0, 1, 2, 3].map(|k| f32::from_bits(v[i * 4 + k]));
            Op { p0: f4(0), p1: f4(1), p2: f4(2), p3: f4(3), meta: [v[16], v[17], v[18], v[19]] }
        })
        .collect();
    let tex = tex_b.chunks_exact(16).map(|c| {
        let v: Vec<u32> = words(c).collect();
        [v[0], v[1], v[2], v[3]]
    }).collect();
    let texels = words(texel_b).collect();
    let frame = Frame { header, ops, tex, texels, lut: lut() };
    frame.validate((width * height) as usize).map_err(|e| format!("frame slot: {e}"))?;
    Ok((seq, frame))
}
