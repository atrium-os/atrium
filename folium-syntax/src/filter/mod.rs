//! Stream filters (ISO 32000-2 §7.4). Every decoder writes into a buffer
//! that refuses to grow past its limit, so a bomb stops at the limit, not
//! at the allocator.
//!
//! Image codecs (DCT, JPX, JBIG2, CCITTFax) are not undone here: decoding
//! stops before the first of them and hands back the rest of the chain, for
//! the image decoders to take at the painted size (folium.md §7.1).

mod ascii;
mod flate;
mod lzw;
mod predictor;
mod runlength;

use crate::object::{Dict, Name, Object};
use crate::{Error, Result};

/// A filter the image layer decodes, with its parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageFilter {
    pub name: Name,
    pub params: Option<Dict>,
}

/// One stage of a chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub name: Name,
    pub params: Option<Dict>,
}

pub fn is_image_codec(n: &Name) -> bool {
    matches!(n.as_bytes(), b"DCTDecode" | b"DCT" | b"JPXDecode" | b"JBIG2Decode" | b"CCITTFaxDecode" | b"CCF")
}

/// The chain from a stream dictionary's `/Filter` and `/DecodeParms`
/// (also the abbreviated `/F`, `/DP` of inline images), already resolved
/// to direct objects by the caller.
pub fn chain(filter: Option<&Object>, parms: Option<&Object>) -> Result<Vec<Stage>> {
    let names: Vec<Name> = match filter {
        None | Some(Object::Null) => Vec::new(),
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => a.iter().filter_map(|o| o.as_name().cloned()).collect(),
        Some(_) => return Err(Error::Syntax { offset: 0, what: "/Filter is not a name or an array" }),
    };
    if names.len() > crate::limits::FILTERS_MAX {
        return Err(Error::Limit("filters on one stream"));
    }
    let parm = |i: usize| -> Option<Dict> {
        match parms {
            Some(Object::Dict(d)) if i == 0 => Some(d.clone()),
            Some(Object::Array(a)) => a.get(i).and_then(|o| o.as_dict()).cloned(),
            _ => None,
        }
    };
    Ok(names.into_iter().enumerate().map(|(i, name)| Stage { name, params: parm(i) }).collect())
}

/// Undo `stages` up to the first image codec. Returns the bytes and what
/// is left of the chain (empty when fully decoded). `limit` caps every
/// stage's output.
pub fn decode(data: Vec<u8>, stages: &[Stage], limit: usize) -> Result<(Vec<u8>, Vec<Stage>)> {
    let mut data = data;
    for (i, st) in stages.iter().enumerate() {
        if is_image_codec(&st.name) {
            return Ok((data, stages[i..].to_vec()));
        }
        let p = st.params.as_ref();
        data = match st.name.as_bytes() {
            b"FlateDecode" | b"Fl" => predictor::apply(flate::decode(&data, limit)?, p, limit)?,
            b"LZWDecode" | b"LZW" => {
                let early = p.and_then(|d| d.int("EarlyChange")).unwrap_or(1) != 0;
                predictor::apply(lzw::decode(&data, early, limit)?, p, limit)?
            }
            b"ASCIIHexDecode" | b"AHx" => ascii::hex(&data, limit)?,
            b"ASCII85Decode" | b"A85" => ascii::a85(&data, limit)?,
            b"RunLengthDecode" | b"RL" => runlength::decode(&data, limit)?,
            // Decryption happens before the chain; `/Crypt` with the
            // identity filter (the only one allowed in a chain without a
            // security handler naming it) passes the bytes through.
            b"Crypt" => data,
            _ => return Err(Error::Unsupported("an unknown stream filter")),
        };
    }
    Ok((data, Vec::new()))
}

/// A `Vec` that refuses to grow past `limit`.
pub(crate) struct Capped {
    pub v: Vec<u8>,
    pub limit: usize,
}

impl Capped {
    pub fn new(limit: usize, hint: usize) -> Self { Capped { v: Vec::with_capacity(hint.min(limit).min(1 << 20)), limit } }
    #[inline]
    pub fn push(&mut self, b: u8) -> Result<()> {
        if self.v.len() >= self.limit {
            return Err(Error::Limit("a stream decoded"));
        }
        self.v.push(b);
        Ok(())
    }
    pub fn extend(&mut self, s: &[u8]) -> Result<()> {
        if self.v.len() + s.len() > self.limit {
            return Err(Error::Limit("a stream decoded"));
        }
        self.v.extend_from_slice(s);
        Ok(())
    }
}
