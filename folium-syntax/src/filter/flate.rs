//! FlateDecode with `miniz_oxide` (pure Rust), bounded.

use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_IGNORE_ADLER32, TINFL_FLAG_PARSE_ZLIB_HEADER, TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use miniz_oxide::inflate::core::{decompress as tinfl, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

use crate::{Error, Result};

/// Inflate into a buffer that grows to at most `limit`. Returns the status
/// and what was written (a failed stream keeps its good prefix).
fn inflate(mut input: &[u8], zlib: bool, limit: usize) -> (TINFLStatus, Vec<u8>) {
    let mut flags = TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_IGNORE_ADLER32;
    if zlib {
        flags |= TINFL_FLAG_PARSE_ZLIB_HEADER;
    }
    let mut out = vec![0u8; input.len().saturating_mul(4).clamp(64, 1 << 20).min(limit.max(1))];
    let mut d = Box::<DecompressorOxide>::default();
    let mut pos = 0;
    loop {
        let (st, used, wrote) = tinfl(&mut d, input, &mut out, pos, flags);
        pos += wrote;
        match st {
            TINFLStatus::HasMoreOutput => {
                input = &input[used.min(input.len())..];
                if out.len() >= limit {
                    out.truncate(pos);
                    return (st, out);
                }
                let n = out.len().saturating_mul(2).min(limit);
                out.resize(n, 0);
            }
            _ => {
                out.truncate(pos);
                return (st, out);
            }
        }
    }
}

/// zlib-wrapped deflate, else raw deflate (some writers drop the header).
/// A stream that breaks part way gives what was decoded before the break,
/// as other readers do (damaged content streams are common); one that
/// would pass `limit` is refused.
pub fn decode(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let zlib = data.len() >= 2 && data[0] & 0x0f == 8 && (u16::from(data[0]) << 8 | u16::from(data[1])) % 31 == 0;
    let (st, v) = inflate(data, zlib, limit);
    let (st, v) = if zlib && v.is_empty() && st != TINFLStatus::Done {
        // A zlib header over a stream that isn't one: try it raw.
        inflate(data, false, limit)
    } else {
        (st, v)
    };
    match st {
        TINFLStatus::Done => Ok(v),
        TINFLStatus::HasMoreOutput => Err(Error::Limit("a stream decoded")),
        _ if !v.is_empty() => Ok(v),
        _ => Err(Error::Syntax { offset: 0, what: "a Flate stream that doesn't inflate" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_raw_and_bomb() {
        let src: Vec<u8> = (0..3000).flat_map(|i| format!("{} {} l ", i * 7 % 1013, i * 13 % 997).into_bytes()).collect();
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&src, 6);
        assert_eq!(decode(&z, 1 << 20).unwrap(), src);
        let raw = miniz_oxide::deflate::compress_to_vec(&src, 6);
        assert_eq!(decode(&raw, 1 << 20).unwrap(), src);
        // 64 MiB of zeros compresses to about 64 KiB; refused at 1 MiB.
        let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; 64 << 20], 9);
        assert_eq!(decode(&bomb, 1 << 20), Err(Error::Limit("a stream decoded")));
        // Cut short: what was decoded is kept.
        let cut = &z[..z.len() / 2];
        let part = decode(cut, 1 << 20).unwrap();
        assert!(!part.is_empty() && src.starts_with(&part));
    }
}
