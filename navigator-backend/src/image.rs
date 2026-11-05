//! Image decoding: where a hostile PNG, JPEG, GIF or WebP becomes pixels
//! (threat model §2.4).
//!
//! ★★★ THIS RUNS IN THE DOCUMENT WORKER, NEVER IN THE BROKER OR FRESCO.
//! Decoders are historically the richest source of browser RCE, and a
//! decoder in the compositor is a decoder shared by every site on the
//! screen (§7.3). So the bytes are decoded in the jail that already holds
//! this document's hostile input, and what leaves it is a bare RGBA8 buffer
//! that the broker checks for shape (`validate`) before anything else sees
//! it.
//!
//! The order of checks is the point of the module:
//!
//! 1. the encoded size is bounded (`MAX_ENCODED_IMAGE_BYTES`);
//! 2. the bytes hash to the address they were asked for by — the store
//!    checked this too, and the worker does not take the broker's word;
//! 3. the format is sniffed from the magic bytes, never from a name or a
//!    MIME type, and anything not a raster format we decode is refused;
//! 4. the HEADER's dimensions are read and checked against the profile
//!    (`MAX_IMAGE_DIMENSION` per axis, the natural frame against the
//!    transient ceiling, and the output it reduces to against the
//!    document's remaining decoded-bytes budget) BEFORE the frame is
//!    allocated — the decompression bomb is refused on a dozen bytes of
//!    header;
//! 5. each decoder also gets its own memory limit, so a format whose
//!    internals allocate beyond the frame (PNG text chunks, an ICC profile)
//!    is bounded by the same transient ceiling;
//! 6. the output is converted to one layout, reduced to the painted size,
//!    and its length checked before it is returned.
//!
//! ★★ DECODED TO THE PAINTED SIZE. The broker sends the size the image is
//! painted at (device pixels, `Ceilings::target`); the decoder decodes the
//! natural frame, which it holds only for the length of the decode, and
//! hands back a reduction to at most that size (`resample::fit`: one scale
//! for both axes, never up). Two ceilings, then, both checked on the header
//! before anything is allocated: the NATURAL frame against `frame` (the
//! transient, one image at a time), and the OUTPUT against `budget` (what
//! the document keeps, and what it is charged). None of the four decoders
//! here can decode at a reduced scale itself (zune-jpeg has no DCT scaling),
//! so the reduction is always a resample after the decode.
//!
//! ★ THE OUTPUT FORMAT, stated once: RGBA, 8 bits per channel, **straight
//! (not premultiplied) alpha**, rows top to bottom, no padding (stride =
//! 4 × width), channel values exactly as the file encodes them (sRGB by
//! convention; embedded colour profiles and gamma chunks are ignored). An
//! animated image yields its first frame. Premultiplying is the
//! compositor's job, on a buffer whose shape is already checked.
//!
//! The decoders are pure Rust and forbid `unsafe` in the configuration this
//! crate builds them in: `png`, `zune-jpeg` without its SIMD features (with
//! them off the crate is `forbid(unsafe_code)`), `image-webp`, and `gif`.

use navigator_dom::profile::{MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_DIMENSION};
use std::io::Cursor;

/// The largest encoded image accepted. The two corpora's largest blob is
/// 5.0 MiB (n = 3,667, measured 2026-11-05); this is 3× that, and below
/// the wire's `MAX_FRAME` so a blob always fits in one frame to the worker.
pub const MAX_ENCODED_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Bytes per output pixel: RGBA8.
pub const BYTES_PER_PIXEL: usize = 4;

/// What the decoder needs to know about the document it decodes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceilings {
    /// Per axis, in pixels.
    pub max_dimension: u32,
    /// What is left of the document's decoded-bytes budget. The OUTPUT
    /// (after any reduction to `target`) must fit in it: it is what the
    /// document keeps and is charged for.
    pub budget: usize,
    /// The largest NATURAL frame decoded on the way to the output, which
    /// is held only while this one image decodes; each decoder's internal
    /// allocations are capped by it too. The profile's per-document total:
    /// a single image larger than a whole document's images is refused.
    pub frame: usize,
    /// The size the image is painted at, in device pixels (the largest of
    /// its painted tiles, times the device scale); `None` decodes at the
    /// natural size. The output is `resample::fit(natural, target)`.
    pub target: Option<(u32, u32)>,
}

impl Default for Ceilings {
    fn default() -> Self {
        Ceilings { max_dimension: MAX_IMAGE_DIMENSION, budget: MAX_DECODED_IMAGE_BYTES,
                   frame: MAX_DECODED_IMAGE_BYTES, target: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format { Png, Jpeg, Gif, Webp, Svg }

impl Format {
    pub fn name(self) -> &'static str {
        match self { Format::Png => "png", Format::Jpeg => "jpeg", Format::Gif => "gif", Format::Webp => "webp", Format::Svg => "svg" }
    }
}

/// A decoded image. See the module comment for the layout.
#[derive(Clone, PartialEq, Eq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pixels {}x{} ({} bytes)", self.width, self.height, self.rgba.len())
    }
}

/// Why an image was not decoded. Every variant is a refusal the fuzzer must
/// reach (tests/image_fuzz.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// More encoded bytes than `MAX_ENCODED_IMAGE_BYTES`.
    EncodedTooLarge { len: usize },
    /// The bytes do not hash to the address they were requested by.
    AddressMismatch,
    /// Not a raster format this decodes. `kind` is what the bytes look like
    /// (`svg`, `avif`, `html`, `bmp`, `unknown`), for the measurement.
    NotARaster { kind: &'static str },
    /// The header could not be read far enough to give dimensions.
    Header { format: Format, why: String },
    /// A width or height of zero.
    Empty { format: Format },
    /// An axis over the ceiling. Refused on the header, before allocating.
    TooLarge { format: Format, width: u32, height: u32 },
    /// The output would not fit in what is left of the document's budget.
    /// Refused on the header, before allocating.
    OverBudget { format: Format, needed: usize, left: usize },
    /// The natural frame (and, for a progressive JPEG, its coefficients)
    /// is over the transient ceiling `Ceilings::frame`, whatever it would
    /// be reduced to. Refused on the header, before allocating.
    FrameTooLarge { format: Format, needed: usize, limit: usize },
    /// The decoder refused the data after the header: truncated, corrupt, a
    /// feature it does not support, or its own memory limit.
    Corrupt { format: Format, why: String },
    /// Outside the format's profile (the SVG profile, src/svg.rs), refused
    /// by name before anything is rendered: `why` is what the measurement
    /// counts.
    Refused { format: Format, why: &'static str },
}

impl DecodeError {
    /// A stable name, for counting failures by cause.
    pub fn cause(&self) -> &'static str {
        match self {
            DecodeError::EncodedTooLarge { .. } => "encoded-too-large",
            DecodeError::AddressMismatch => "address-mismatch",
            DecodeError::NotARaster { .. } => "not-a-raster",
            DecodeError::Header { .. } => "header",
            DecodeError::Empty { .. } => "empty",
            DecodeError::TooLarge { .. } => "too-large",
            DecodeError::OverBudget { .. } => "over-budget",
            DecodeError::FrameTooLarge { .. } => "frame-too-large",
            DecodeError::Corrupt { .. } => "corrupt",
            DecodeError::Refused { .. } => "refused-by-profile",
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::EncodedTooLarge { len } =>
                write!(f, "image is {len} bytes encoded, over {MAX_ENCODED_IMAGE_BYTES}"),
            DecodeError::AddressMismatch => write!(f, "image bytes do not hash to their address"),
            DecodeError::NotARaster { kind } => write!(f, "not a raster image this decodes ({kind})"),
            DecodeError::Header { format, why } => write!(f, "{} header unreadable: {why}", format.name()),
            DecodeError::Empty { format } => write!(f, "{} image has a zero dimension", format.name()),
            DecodeError::TooLarge { format, width, height } =>
                write!(f, "{} image is {width}x{height}, over {MAX_IMAGE_DIMENSION} on an axis", format.name()),
            DecodeError::OverBudget { format, needed, left } =>
                write!(f, "{} image needs {needed} decoded bytes, {left} left in the document's budget", format.name()),
            DecodeError::FrameTooLarge { format, needed, limit } =>
                write!(f, "{} image needs a {needed}-byte frame to decode, over {limit}", format.name()),
            DecodeError::Corrupt { format, why } => write!(f, "{} image did not decode: {why}", format.name()),
            DecodeError::Refused { format, why } => write!(f, "{} image refused by its profile: {why}", format.name()),
        }
    }
}

/// What the bytes look like, from their first bytes alone.
pub fn sniff(b: &[u8]) -> Result<Format, &'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") { return Ok(Format::Png) }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) { return Ok(Format::Jpeg) }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { return Ok(Format::Gif) }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") { return Ok(Format::Webp) }
    // What it is instead, so a failure count says something.
    if b.get(4..8) == Some(b"ftyp") {
        return Err(match b.get(8..12) { Some(b"avif") | Some(b"avis") => "avif", Some(b"heic") | Some(b"heix") | Some(b"mif1") => "heif", _ => "isobmff" });
    }
    if b.starts_with(b"BM") { return Err("bmp") }
    if b.starts_with(&[0, 0, 1, 0]) { return Err("ico") }
    let head = String::from_utf8_lossy(&b[..b.len().min(1024)]).to_ascii_lowercase();
    // An HTML page that happens to hold an inline <svg> is still HTML.
    if head.contains("<html") || head.contains("<!doctype html") { return Err("html") }
    if head.contains("<svg") { return Ok(Format::Svg) }
    if head.contains("<!doctype") { return Err("html") }
    if b.is_empty() { return Err("empty") }
    Err("unknown")
}

/// The address an image is requested by: `blake3:` and 64 lowercase hex.
pub fn address_hex(address: &str) -> Option<&str> {
    let hex = address.strip_prefix("blake3:")?;
    (hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(hex)
}

/// Decode `bytes`, which were requested as `address`.
pub fn decode(address: &str, bytes: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    if bytes.len() > MAX_ENCODED_IMAGE_BYTES { return Err(DecodeError::EncodedTooLarge { len: bytes.len() }) }
    // ★ Checked here as well as in the store: the worker is handed these
    // bytes by the broker, and a decoder that took "these are the bytes you
    // asked for" on trust would decode whatever it was given.
    match address_hex(address) {
        Some(hex) if blake3::hash(bytes).to_hex().as_str() == hex => {}
        _ => return Err(DecodeError::AddressMismatch),
    }
    decode_unaddressed(bytes, c)
}

/// The decode alone, after the address is checked. Public for the fuzzer,
/// which mutates bytes and cannot keep re-hashing them into an address.
pub fn decode_unaddressed(bytes: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    if bytes.len() > MAX_ENCODED_IMAGE_BYTES { return Err(DecodeError::EncodedTooLarge { len: bytes.len() }) }
    let format = sniff(bytes).map_err(|kind| DecodeError::NotARaster { kind })?;
    // ★ SVG is drawn AT the painted size (src/svg.rs), not decoded at a
    // natural size and reduced: a vector stays sharp at any size, so a zoom
    // or device-scale change asks again at the new size and is drawn again,
    // never a scaled bitmap.
    if format == Format::Svg {
        let px = crate::svg::rasterize(bytes, c)?;
        validate(px.width, px.height, px.rgba.len(), c).map_err(|why| DecodeError::Corrupt { format, why })?;
        return Ok(px);
    }
    let px = match format {
        Format::Png => png_decode(bytes, c),
        Format::Jpeg => jpeg_decode(bytes, c),
        Format::Gif => gif_decode(bytes, c),
        Format::Webp => webp_decode(bytes, c),
        Format::Svg => unreachable!("drawn above"),
    }?;
    // ★ To the painted size, the natural frame dropped as soon as the
    // reduction exists. `admit` already checked the output's size.
    let (ow, oh) = output_size((px.width, px.height), c);
    let px = if (ow, oh) == (px.width, px.height) { px } else {
        let rgba = crate::resample::resample(&px.rgba, px.width, px.height, ow, oh, crate::resample::Filter::DEFAULT);
        drop(px);
        Pixels { width: ow, height: oh, rgba }
    };
    // ★ The decoders are trusted to be memory-safe, not to be right. The
    // shape is re-checked on the way out, as the broker will check it again.
    validate(px.width, px.height, px.rgba.len(), c).map_err(|why| DecodeError::Corrupt { format, why })?;
    Ok(px)
}

/// What a natural size decodes to under these ceilings.
fn output_size(natural: (u32, u32), c: Ceilings) -> (u32, u32) {
    match c.target { Some(t) => crate::resample::fit(natural, t), None => natural }
}

/// ★ THE ADMISSION CHECK, run on the header's dimensions before a frame is
/// allocated: the natural frame against the transient ceiling, the output
/// against the budget. Returns the natural frame's size in bytes.
fn admit(format: Format, width: u32, height: u32, c: Ceilings) -> Result<usize, DecodeError> {
    if width == 0 || height == 0 { return Err(DecodeError::Empty { format }) }
    if width > c.max_dimension || height > c.max_dimension {
        return Err(DecodeError::TooLarge { format, width, height });
    }
    // Cannot overflow: both axes are at most 16,384 here.
    let frame = width as usize * height as usize * BYTES_PER_PIXEL;
    if frame > c.frame { return Err(DecodeError::FrameTooLarge { format, needed: frame, limit: c.frame }) }
    let (ow, oh) = output_size((width, height), c);
    let needed = ow as usize * oh as usize * BYTES_PER_PIXEL;
    if needed > c.budget { return Err(DecodeError::OverBudget { format, needed, left: c.budget }) }
    Ok(frame)
}

/// ★★ THE BROKER'S CHECK on a buffer a worker hands back — and the worker's
/// own last check before it does. A compromised worker can return anything,
/// so this is what stands between it and the compositor: dimensions within
/// the ceilings, and a byte length that is exactly what they imply.
pub fn validate(width: u32, height: u32, len: usize, c: Ceilings) -> Result<(), String> {
    if width == 0 || height == 0 { return Err(format!("{width}x{height} has a zero dimension")) }
    if width > c.max_dimension || height > c.max_dimension {
        return Err(format!("{width}x{height} is over {} on an axis", c.max_dimension));
    }
    let want = width as usize * height as usize * BYTES_PER_PIXEL;
    if len != want { return Err(format!("{width}x{height} RGBA8 is {want} bytes, the buffer is {len}")) }
    if want > c.budget { return Err(format!("{want} decoded bytes, {} left in the budget", c.budget)) }
    // ★ With a target, an honest decode is at or under it on at least one
    // axis: `resample::fit` takes one scale from the axis that asks for
    // more, and returns the natural size only when that axis already fits.
    // An image over the target on BOTH axes ignored it.
    if let Some((tw, th)) = c.target {
        if width > tw && height > th { return Err(format!("{width}x{height} is over the painted size {tw}x{th} on both axes")) }
    }
    Ok(())
}

fn png_decode(b: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    let f = Format::Png;
    let corrupt = |e: png::DecodingError| DecodeError::Corrupt { format: f, why: e.to_string() };
    // The IHDR is the first chunk, at a fixed offset: its dimensions are
    // checked before the decoder is even built, so not even ancillary
    // chunks ahead of the image data are read for an image that is refused.
    let be32 = |i: usize| b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    if b.get(12..16) != Some(b"IHDR") {
        return Err(DecodeError::Header { format: f, why: "first chunk is not IHDR".into() });
    }
    let (w, h) = match (be32(16), be32(20)) {
        (Some(w), Some(h)) => (w, h),
        _ => return Err(DecodeError::Header { format: f, why: "truncated IHDR".into() }),
    };
    let frame = admit(f, w, h, c)?;
    // ★ The decoder's own limit is the frame ceiling: a PNG can make the
    // decoder allocate for text and ICC chunks and its row buffers as well
    // as the frame, and none of that may exceed it.
    let mut dec = png::Decoder::new_with_limits(Cursor::new(b), png::Limits { bytes: c.frame });
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    dec.set_ignore_text_chunk(true);
    dec.set_ignore_iccp_chunk(true);
    let mut reader = dec.read_info().map_err(|e| match e {
        png::DecodingError::IoError(_) | png::DecodingError::Format(_) =>
            DecodeError::Header { format: f, why: e.to_string() },
        e => corrupt(e),
    })?;
    // The decoder's view must agree with the bytes checked above.
    let info = reader.info();
    if (info.width, info.height) != (w, h) {
        return Err(DecodeError::Header { format: f, why: "IHDR disagrees with the decoder".into() });
    }
    let (ct, _) = reader.output_color_type();
    let samples = ct.samples();
    let size = reader.output_buffer_size();
    // EXPAND + STRIP_16 leaves 8-bit samples, 1 to 4 of them per pixel.
    if size != w as usize * h as usize * samples || size > frame {
        return Err(DecodeError::Corrupt { format: f, why: format!("output buffer {size} disagrees with {w}x{h}x{samples}") });
    }
    let mut buf = vec![0u8; size];
    reader.next_frame(&mut buf).map_err(corrupt)?;
    Ok(Pixels { width: w, height: h, rgba: to_rgba(buf, samples) })
}

/// 1–4 samples per pixel (grey, grey+alpha, RGB, RGBA) to RGBA.
fn to_rgba(buf: Vec<u8>, samples: usize) -> Vec<u8> {
    match samples {
        4 => buf,
        3 => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        2 => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    }
}

fn jpeg_decode(b: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let f = Format::Jpeg;
    // ★ The ceilings are ALSO the decoder's options, so zune refuses an
    // oversized frame header itself — and the check below re-asks, so the
    // refusal is ours whichever fires first.
    let opts = DecoderOptions::default()
        .set_max_width(c.max_dimension as usize)
        .set_max_height(c.max_dimension as usize)
        .set_use_unsafe(false)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(b), opts);
    if let Err(e) = dec.decode_headers() {
        // zune's own dimension refusal reads as TooLarge, from the header.
        let why = format!("{e:?}");
        if let Some((w, h)) = jpeg_frame_size(b) {
            if w == 0 || h == 0 { return Err(DecodeError::Empty { format: f }) }
            if w > c.max_dimension || h > c.max_dimension {
                return Err(DecodeError::TooLarge { format: f, width: w, height: h });
            }
        }
        return Err(DecodeError::Header { format: f, why });
    }
    let info = dec.info().ok_or_else(|| DecodeError::Header { format: f, why: "no frame header".into() })?;
    let (w, h) = (info.width as u32, info.height as u32);
    let frame = admit(f, w, h, c)?;
    // ★ A PROGRESSIVE JPEG keeps every coefficient until its last scan:
    // zune holds an i16 per sample per component, MCU-padded (at most 16 px
    // per axis). That scales with the frame, so it is charged against the
    // transient ceiling with it, before decoding. A baseline JPEG decodes a row of
    // MCUs at a time and needs no such charge.
    let components = (info.components as usize).max(1);
    let scratch = if info.sof.is_progressive() {
        (w as usize + 16) * (h as usize + 16) * components * 2
    } else { 0 };
    if frame + scratch > c.frame {
        return Err(DecodeError::FrameTooLarge { format: f, needed: frame + scratch, limit: c.frame });
    }
    match dec.output_buffer_size() {
        Some(n) if n == frame => {}
        n => return Err(DecodeError::Corrupt { format: f, why: format!("output buffer {n:?} disagrees with {w}x{h}") }),
    }
    let mut out = vec![0u8; frame];
    dec.decode_into(&mut out).map_err(|e| DecodeError::Corrupt { format: f, why: format!("{e:?}") })?;
    Ok(Pixels { width: w, height: h, rgba: out })
}

/// The frame header's dimensions, read by walking segments: for naming a
/// refusal when zune declines the header (it reports its own max-size error
/// as text).
pub(crate) fn jpeg_frame_size(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    let mut i = 2usize;
    while i + 9 <= b.len() {
        if b[i] != 0xFF { i += 1; continue }
        let m = b[i + 1];
        if (0xC0..=0xCF).contains(&m) && ![0xC4, 0xC8, 0xCC].contains(&m) {
            return Some((be16(i + 7)?, be16(i + 5)?));
        }
        let seg = be16(i + 2)? as usize;
        if seg < 2 { return None }
        i += 2 + seg;
    }
    None
}

fn webp_decode(b: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    let f = Format::Webp;
    // ★ The canvas size is read from the bytes and admitted BEFORE the
    // decoder is built: `WebPDecoder::new` walks the whole chunk list (and
    // a VP8X canvas can claim 16,777,216² in 24-bit fields).
    let (w, h) = webp_size(b).ok_or_else(|| DecodeError::Header { format: f, why: "no VP8X, VP8 or VP8L header".into() })?;
    let frame = admit(f, w, h, c)?;
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(b))
        .map_err(|e| DecodeError::Header { format: f, why: e.to_string() })?;
    if dec.dimensions() != (w, h) {
        return Err(DecodeError::Header { format: f, why: format!("header says {w}x{h}, the decoder {:?}", dec.dimensions()) });
    }
    dec.set_memory_limit(c.frame);
    let alpha = dec.has_alpha();
    let size = dec.output_buffer_size()
        .ok_or_else(|| DecodeError::Corrupt { format: f, why: "size overflows".into() })?;
    let samples = if alpha { 4 } else { 3 };
    if size != w as usize * h as usize * samples || size > frame {
        return Err(DecodeError::Corrupt { format: f, why: format!("output buffer {size} disagrees with {w}x{h}") });
    }
    let mut buf = vec![0u8; size];
    dec.read_image(&mut buf).map_err(|e| DecodeError::Corrupt { format: f, why: e.to_string() })?;
    Ok(Pixels { width: w, height: h, rgba: to_rgba(buf, samples) })
}

/// A WebP's canvas size from its first chunk.
fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    let le24 = |i: usize| -> Option<u32> { Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0])) };
    let le16 = |i: usize| -> Option<u32> { Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    match b.get(12..16)? {
        b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
        b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
        b"VP8L" => {
            let bits = u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        _ => None,
    }
}

fn gif_decode(b: &[u8], c: Ceilings) -> Result<Pixels, DecodeError> {
    let f = Format::Gif;
    let corrupt = |e: gif::DecodingError| DecodeError::Corrupt { format: f, why: e.to_string() };
    let le16 = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as u32);
    // The logical screen is the image's size: checked on the header bytes.
    let (w, h) = match (le16(6), le16(8)) {
        (Some(w), Some(h)) => (w, h),
        _ => return Err(DecodeError::Header { format: f, why: "truncated logical screen".into() }),
    };
    let frame = admit(f, w, h, c)?;
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    // The first frame's buffer is held alongside the screen, so the
    // decoder gets what is left after the screen.
    opts.set_memory_limit(gif::MemoryLimit::Bytes(
        std::num::NonZeroU64::new((c.frame - frame).max(1) as u64).expect("nonzero")));
    let mut dec = opts.read_info(Cursor::new(b)).map_err(|e| DecodeError::Header { format: f, why: e.to_string() })?;
    let mut screen = vec![0u8; frame];
    // ★ The first frame, composited onto a transparent screen at its own
    // offset and CLIPPED to the screen, as browsers draw it: a frame header
    // may claim to sit anywhere, at any size up to 65,535².
    let Some(fr) = dec.read_next_frame().map_err(corrupt)? else {
        return Err(DecodeError::Corrupt { format: f, why: "no frames".into() });
    };
    let (fl, ft, fw, fh) = (fr.left as usize, fr.top as usize, fr.width as usize, fr.height as usize);
    if fr.buffer.len() != fw * fh * 4 {
        return Err(DecodeError::Corrupt { format: f, why: "frame buffer disagrees with its header".into() });
    }
    let (sw, sh) = (w as usize, h as usize);
    for y in 0..fh {
        let sy = ft + y;
        if sy >= sh { break }
        if fl >= sw { break }
        let n = fw.min(sw - fl);
        let src = &fr.buffer[y * fw * 4..(y * fw + n) * 4];
        let dst = &mut screen[(sy * sw + fl) * 4..(sy * sw + fl + n) * 4];
        dst.copy_from_slice(src);
    }
    Ok(Pixels { width: w, height: h, rgba: screen })
}
