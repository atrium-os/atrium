//! The CBOR CTAP2 uses, and nothing else (sigillum.md §6.1).
//!
//! CTAP2 messages are CBOR in the "CTAP2 canonical" form: definite lengths,
//! shortest integer encodings, map keys sorted length-first then bytewise,
//! and only integers, byte and text strings, arrays, maps, booleans and
//! null. Everything an authenticator says is attacker-chosen (a fake key is
//! a cheap microcontroller), so the decoder is strict and bounded:
//!
//! - one item, no trailing bytes;
//! - nesting at most [`Limits::max_depth`], at most [`Limits::max_items`]
//!   items in total, input at most [`Limits::max_len`] bytes;
//! - no tags, floats, indefinite lengths or simple values other than
//!   `false`, `true` and `null`;
//! - integers in their shortest form, text valid UTF-8, no duplicate map keys.
//!
//! Map key *order* is not enforced on input: some shipping authenticators
//! get it wrong, and order carries no meaning. Output is always canonical.
#![forbid(unsafe_code)]

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A non-negative integer.
    Uint(u64),
    /// A negative integer `-1 - n`.
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated,
    TrailingBytes,
    TooDeep,
    TooManyItems,
    TooLong,
    /// Tags, floats, indefinite lengths, undefined, other simple values.
    Unsupported,
    NotShortest,
    BadUtf8,
    DuplicateKey,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Error {}

/// Caps for one decode. The defaults fit any CTAP2 message (CTAPHID's
/// maximum is 7609 bytes).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_depth: usize,
    pub max_items: usize,
    pub max_len: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_depth: 8, max_items: 2048, max_len: 7609 }
    }
}

/// Decode exactly one item with the default limits.
pub fn decode(input: &[u8]) -> Result<Value, Error> {
    decode_with(input, Limits::default())
}

pub fn decode_with(input: &[u8], limits: Limits) -> Result<Value, Error> {
    if input.len() > limits.max_len {
        return Err(Error::TooLong);
    }
    let mut d = Decoder { input, pos: 0, items: 0, limits };
    let v = d.item(0)?;
    if d.pos != input.len() {
        return Err(Error::TrailingBytes);
    }
    Ok(v)
}

/// Decode one item from the front of `input` and return it with the bytes
/// it took. For authenticator data, whose extensions map is followed by
/// nothing but is not length-prefixed.
pub fn decode_prefix(input: &[u8], limits: Limits) -> Result<(Value, usize), Error> {
    let input = &input[..input.len().min(limits.max_len)];
    let mut d = Decoder { input, pos: 0, items: 0, limits };
    let v = d.item(0)?;
    Ok((v, d.pos))
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    items: usize,
    limits: Limits,
}

impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        if end > self.input.len() {
            return Err(Error::Truncated);
        }
        let s = &self.input[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    /// The argument of a head whose low five bits are `info`.
    fn arg(&mut self, info: u8) -> Result<u64, Error> {
        let v = match info {
            0..=23 => return Ok(info as u64),
            24 => {
                let v = self.byte()? as u64;
                if v < 24 {
                    return Err(Error::NotShortest);
                }
                return Ok(v);
            }
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return Err(Error::Unsupported), // 28-30 reserved, 31 indefinite
        };
        let min = match info {
            25 => 0x100,
            26 => 0x1_0000,
            _ => 0x1_0000_0000,
        };
        if v < min {
            return Err(Error::NotShortest);
        }
        Ok(v)
    }

    fn len(&mut self, info: u8) -> Result<usize, Error> {
        let n = self.arg(info)?;
        // A length can never exceed what is left of the input.
        if n > (self.input.len() - self.pos) as u64 {
            return Err(Error::Truncated);
        }
        Ok(n as usize)
    }

    fn item(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > self.limits.max_depth {
            return Err(Error::TooDeep);
        }
        self.items += 1;
        if self.items > self.limits.max_items {
            return Err(Error::TooManyItems);
        }
        let head = self.byte()?;
        let (major, info) = (head >> 5, head & 0x1f);
        Ok(match major {
            0 => Value::Uint(self.arg(info)?),
            1 => Value::Nint(self.arg(info)?),
            2 => {
                let n = self.len(info)?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            3 => {
                let n = self.len(info)?;
                let s = self.take(n)?;
                Value::Text(std::str::from_utf8(s).map_err(|_| Error::BadUtf8)?.to_owned())
            }
            4 => {
                let n = self.len(info)?;
                let mut v = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    v.push(self.item(depth + 1)?);
                }
                Value::Array(v)
            }
            5 => {
                let n = self.len(info)?;
                let mut v: Vec<(Value, Value)> = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let k = self.item(depth + 1)?;
                    if v.iter().any(|(e, _)| *e == k) {
                        return Err(Error::DuplicateKey);
                    }
                    let val = self.item(depth + 1)?;
                    v.push((k, val));
                }
                Value::Map(v)
            }
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                _ => return Err(Error::Unsupported),
            },
            _ => return Err(Error::Unsupported), // 6: tags
        })
    }
}

/// Encode canonically (CTAP2 §8: shortest forms, map keys sorted by their
/// encoding, length first, then bytewise).
pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(v, &mut out);
    out
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= 0xff {
        out.extend_from_slice(&[m | 24, n as u8]);
    } else if n <= 0xffff {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xffff_ffff {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_into(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Uint(n) => head(out, 0, *n),
        Value::Nint(n) => head(out, 1, *n),
        Value::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Text(s) => {
            head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(a) => {
            head(out, 4, a.len() as u64);
            for e in a {
                encode_into(e, out);
            }
        }
        Value::Map(m) => {
            let mut entries: Vec<(Vec<u8>, &Value)> = m.iter().map(|(k, v)| (encode(k), v)).collect();
            entries.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
            head(out, 5, entries.len() as u64);
            for (k, v) in entries {
                out.extend_from_slice(&k);
                encode_into(v, out);
            }
        }
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Null => out.push(0xf6),
    }
}

impl Value {
    /// An integer key or value: `Uint` for n >= 0, `Nint` otherwise.
    pub fn int(n: i64) -> Value {
        if n >= 0 {
            Value::Uint(n as u64)
        } else {
            Value::Nint((-1 - n) as u64)
        }
    }

    pub fn text(s: &str) -> Value {
        Value::Text(s.to_owned())
    }

    pub fn bytes(b: &[u8]) -> Value {
        Value::Bytes(b.to_vec())
    }

    /// The value as a signed integer, if it is one that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Uint(n) if n <= i64::MAX as u64 => Some(n as i64),
            Value::Nint(n) if n <= i64::MAX as u64 => Some(-1 - n as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::Uint(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    /// The entry of a map under integer key `k`.
    pub fn get_int(&self, k: i64) -> Option<&Value> {
        self.as_map()?.iter().find(|(key, _)| key.as_i64() == Some(k)).map(|(_, v)| v)
    }

    /// The entry of a map under text key `k`.
    pub fn get_text(&self, k: &str) -> Option<&Value> {
        self.as_map()?.iter().find(|(key, _)| key.as_text() == Some(k)).map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rfc8949_examples() {
        // Appendix A of RFC 8949, the ones in our subset.
        let cases: &[(&str, Value)] = &[
            ("00", Value::Uint(0)),
            ("17", Value::Uint(23)),
            ("1818", Value::Uint(24)),
            ("1903e8", Value::Uint(1000)),
            ("1b000000e8d4a51000", Value::Uint(1_000_000_000_000)),
            ("20", Value::int(-1)),
            ("3903e7", Value::int(-1000)),
            ("4401020304", Value::bytes(&[1, 2, 3, 4])),
            ("6449455446", Value::text("IETF")),
            ("62c3bc", Value::text("\u{fc}")),
            ("83010203", Value::Array(vec![Value::Uint(1), Value::Uint(2), Value::Uint(3)])),
            ("f4", Value::Bool(false)),
            ("f5", Value::Bool(true)),
            ("f6", Value::Null),
        ];
        for (h, v) in cases {
            assert_eq!(decode(&hex(h)).as_ref(), Ok(v), "{h}");
            assert_eq!(encode(v), hex(h), "{h}");
        }
    }

    #[test]
    fn canonical_key_order() {
        // CTAP2 canonical order: shorter encodings first, then bytewise.
        let m = Value::Map(vec![
            (Value::text("authData"), Value::Null),
            (Value::text("fmt"), Value::Null),
            (Value::int(-1), Value::Null),
            (Value::Uint(10), Value::Null),
            (Value::Uint(1), Value::Null),
            (Value::text("attStmt"), Value::Null),
        ]);
        let e = encode(&m);
        let back = decode(&e).unwrap();
        let keys: Vec<_> = back.as_map().unwrap().iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            vec![Value::Uint(1), Value::Uint(10), Value::int(-1), Value::text("fmt"), Value::text("attStmt"), Value::text("authData")]
        );
    }

    #[test]
    fn refusals() {
        assert_eq!(decode(&hex("1817")), Err(Error::NotShortest));
        assert_eq!(decode(&hex("190017")), Err(Error::NotShortest));
        assert_eq!(decode(&hex("c11a514b67b0")), Err(Error::Unsupported)); // tag
        assert_eq!(decode(&hex("f93c00")), Err(Error::Unsupported)); // half float
        assert_eq!(decode(&hex("5f42010243030405ff")), Err(Error::Unsupported)); // indefinite
        assert_eq!(decode(&hex("f7")), Err(Error::Unsupported)); // undefined
        assert_eq!(decode(&hex("0000")), Err(Error::TrailingBytes));
        assert_eq!(decode(&hex("62c3")), Err(Error::Truncated));
        assert_eq!(decode(&hex("62c328")), Err(Error::BadUtf8));
        assert_eq!(decode(&hex("a201000100")), Err(Error::DuplicateKey));
        // A length that claims more than the input holds fails before any
        // allocation.
        assert_eq!(decode(&hex("5bffffffffffffffff")), Err(Error::Truncated));
        assert_eq!(decode(&hex("9bffffffffffffffff")), Err(Error::Truncated));
    }

    #[test]
    fn depth_and_count_caps() {
        let mut deep = vec![0x81u8; 9];
        deep.push(0x00);
        assert_eq!(decode(&deep), Err(Error::TooDeep));
        let mut ok = vec![0x81u8; 8];
        ok.push(0x00);
        assert!(decode(&ok).is_ok());

        let mut many = vec![0x99, 0x08, 0x00];
        many.extend(std::iter::repeat(0u8).take(0x800));
        assert_eq!(decode_with(&many, Limits { max_len: 1 << 16, ..Limits::default() }), Err(Error::TooManyItems));
        assert_eq!(decode(&vec![0u8; 8000]), Err(Error::TooLong));
    }

    #[test]
    fn prefix() {
        let (v, n) = decode_prefix(&hex("a10102ffff"), Limits::default()).unwrap();
        assert_eq!(v.get_int(1), Some(&Value::Uint(2)));
        assert_eq!(n, 3);
    }
}
