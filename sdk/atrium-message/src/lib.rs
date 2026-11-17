//! Typed, bounded messages (app-composition.md §5.2, decision C5).
//!
//! What an app's core, its workers and services send each other. A type
//! derives [`Message`]:
//!
//! ```
//! use atrium_message::Message;
//!
//! #[derive(Message, Debug, PartialEq)]
//! #[message(version = 1)]
//! struct Thumbnail {
//!     #[message(min = 1, max = 4096)]
//!     width: u32,
//!     #[message(min = 1, max = 4096)]
//!     height: u32,
//!     #[message(max_len = 67108864)]
//!     pixels: Vec<u8>,
//!     #[message(max_len = 256, no_control)]
//!     title: String,
//! }
//!
//! let t = Thumbnail { width: 2, height: 1, pixels: vec![1, 2, 3, 4, 5, 6, 7, 8], title: "a".into() };
//! let frame = t.to_frame();
//! assert_eq!(Thumbnail::from_frame(&frame, 1 << 20).unwrap(), t);
//! ```
//!
//! and gets:
//!
//! - an encoding that is [postcard](https://docs.rs/postcard)'s wire format
//!   (varints, zigzag for signed, length-prefixed strings and sequences,
//!   variant index for enums), so other tools can read it;
//! - a decoder that **checks while it decodes**: every length and count
//!   against its bound before anything is allocated, integers against their
//!   range, strings as UTF-8 (and free of control characters where asked),
//!   nesting depth, and that the input is used up exactly. A hostile frame
//!   is an `Err`, never a panic or a huge allocation;
//! - a schema ([`Message::schema_json`]) from which `atrium-schema`
//!   generates the same types, encoder and decoder in C.
//!
//! Every string, byte string and sequence has a bound: the one its field
//! names, or [`DEFAULT_MAX_LEN`]. A frame is the version as a varint, then
//! the message; `from_frame` refuses another version and frames over the
//! caller's maximum.

pub use atrium_message_derive::Message;

pub mod schema;

pub use schema::{Registry, Ty};

/// The bound on a string's bytes or a sequence's items when its field
/// names none.
pub const DEFAULT_MAX_LEN: u64 = 65_536;
/// How deep messages may nest (a message in a message in ...).
pub const MAX_DEPTH: u32 = 64;

/// Why bytes were refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input ended inside a value.
    Truncated,
    /// Bytes were left after the message.
    TrailingBytes,
    /// A varint longer than its type allows, or with bits beyond it.
    BadVarint,
    /// A bool that was neither 0 nor 1, or an option tag that was neither.
    BadTag,
    /// A string or sequence longer than its bound.
    TooLong { field: &'static str, len: u64, max: u64 },
    /// An integer outside its field's range.
    OutOfRange { field: &'static str },
    /// A string that isn't UTF-8.
    BadUtf8 { field: &'static str },
    /// A control character where the field allows none.
    ControlChar { field: &'static str },
    /// An enum variant that doesn't exist.
    BadVariant { ty: &'static str, index: u64 },
    /// Nesting deeper than [`MAX_DEPTH`].
    TooDeep,
    /// A frame of another version.
    Version { got: u64, want: u32 },
    /// A frame larger than the caller allows.
    FrameTooLarge { len: usize, max: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => write!(f, "message ends early"),
            Error::TrailingBytes => write!(f, "bytes after the message"),
            Error::BadVarint => write!(f, "malformed integer"),
            Error::BadTag => write!(f, "malformed bool or option"),
            Error::TooLong { field, len, max } => write!(f, "{field}: {len} is over its bound {max}"),
            Error::OutOfRange { field } => write!(f, "{field}: out of range"),
            Error::BadUtf8 { field } => write!(f, "{field}: not UTF-8"),
            Error::ControlChar { field } => write!(f, "{field}: a control character"),
            Error::BadVariant { ty, index } => write!(f, "{ty}: no variant {index}"),
            Error::TooDeep => write!(f, "nested too deep"),
            Error::Version { got, want } => write!(f, "version {got}, expected {want}"),
            Error::FrameTooLarge { len, max } => write!(f, "frame of {len} bytes, at most {max}"),
        }
    }
}

impl std::error::Error for Error {}

/// What a field allows (from its `#[message(...)]`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Bounds {
    pub field:        &'static str,
    /// Strings: bytes. Sequences: items.
    pub max_len:      Option<u64>,
    /// For a sequence of strings or sequences: each item's bound.
    pub item_max_len: Option<u64>,
    pub min:          Option<i128>,
    pub max:          Option<i128>,
    /// Strings: no control characters (Unicode Cc) other than tab.
    pub no_control:   bool,
}

impl Bounds {
    pub const fn named(field: &'static str) -> Self {
        Bounds { field, max_len: None, item_max_len: None, min: None, max: None, no_control: false }
    }
    fn len(&self) -> u64 { self.max_len.unwrap_or(DEFAULT_MAX_LEN) }
    /// The bounds of a sequence's items.
    pub fn items(&self) -> Bounds {
        Bounds { max_len: self.item_max_len, item_max_len: None, ..*self }
    }
}

/// Reads a message, keeping count of depth.
pub struct Reader<'a> {
    buf:   &'a [u8],
    pos:   usize,
    depth: u32,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self { Reader { buf, pos: 0, depth: 0 } }
    pub fn remaining(&self) -> usize { self.buf.len() - self.pos }
    pub fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.remaining() {
            return Err(Error::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    /// A varint of at most `bits` bits (postcard: LEB128, no excess bits).
    pub fn varint(&mut self, bits: u32) -> Result<u64, Error> {
        let max_bytes = bits.div_ceil(7);
        let mut v: u64 = 0;
        for i in 0..max_bytes {
            let b = self.byte()?;
            let chunk = (b & 0x7f) as u64;
            let shift = 7 * i;
            if i == max_bytes - 1 {
                // The last byte may carry only the bits that are left.
                let left = bits - shift;
                if b & 0x80 != 0 || (left < 7 && chunk >> left != 0) {
                    return Err(Error::BadVarint);
                }
            }
            v |= chunk << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Error::BadVarint)
    }
    /// Into a nested message.
    pub fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH { Err(Error::TooDeep) } else { Ok(()) }
    }
    pub fn leave(&mut self) { self.depth -= 1; }
    pub fn finish(&self) -> Result<(), Error> {
        if self.pos == self.buf.len() { Ok(()) } else { Err(Error::TrailingBytes) }
    }
}

pub fn put_varint(w: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            w.push(b);
            return;
        }
        w.push(b | 0x80);
    }
}

/// A value that can be a field. The derive implements it for message
/// types; it is implemented here for the primitives, `String`, `Vec`,
/// `Option`, `Box` and arrays.
pub trait Wire: Sized {
    fn put(&self, w: &mut Vec<u8>);
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error>;
    /// Its schema type (registering any message types it names).
    fn ty(reg: &mut Registry, b: &Bounds) -> Ty;
}

/// A message: something sent whole, as bytes or a versioned frame.
pub trait Message: Wire {
    const NAME: &'static str;
    const VERSION: u32;

    fn to_bytes(&self) -> Vec<u8> {
        let mut w = Vec::new();
        self.put(&mut w);
        w
    }
    fn from_bytes(b: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(b);
        let v = Self::get(&mut r, &Bounds::named(Self::NAME))?;
        r.finish()?;
        Ok(v)
    }
    /// The version, then the message.
    fn to_frame(&self) -> Vec<u8> {
        let mut w = Vec::new();
        put_varint(&mut w, Self::VERSION as u64);
        self.put(&mut w);
        w
    }
    /// A frame of this version and at most `max` bytes, used up exactly.
    fn from_frame(b: &[u8], max: usize) -> Result<Self, Error> {
        if b.len() > max {
            return Err(Error::FrameTooLarge { len: b.len(), max });
        }
        let mut r = Reader::new(b);
        let v = r.varint(32)?;
        if v != Self::VERSION as u64 {
            return Err(Error::Version { got: v, want: Self::VERSION });
        }
        let m = Self::get(&mut r, &Bounds::named(Self::NAME))?;
        r.finish()?;
        Ok(m)
    }
    /// The schema file for this message and every type it uses.
    fn schema_json() -> String {
        let mut reg = Registry::default();
        Self::ty(&mut reg, &Bounds::named(Self::NAME));
        reg.to_json(Self::NAME, Self::VERSION)
    }
}

fn range(v: i128, b: &Bounds) -> Result<(), Error> {
    if b.min.is_some_and(|m| v < m) || b.max.is_some_and(|m| v > m) {
        Err(Error::OutOfRange { field: b.field })
    } else {
        Ok(())
    }
}

macro_rules! unsigned {
    ($t:ty, $bits:expr, $name:expr) => {
        impl Wire for $t {
            fn put(&self, w: &mut Vec<u8>) { put_varint(w, *self as u64) }
            fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
                let v = r.varint($bits)? as $t;
                range(v as i128, b)?;
                Ok(v)
            }
            fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim($name, b) }
        }
    };
}
macro_rules! signed {
    ($t:ty, $u:ty, $bits:expr, $name:expr) => {
        impl Wire for $t {
            fn put(&self, w: &mut Vec<u8>) {
                let z = ((*self << 1) ^ (*self >> ($bits - 1))) as $u;
                put_varint(w, z as u64)
            }
            fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
                let z = r.varint($bits)? as $u;
                let v = ((z >> 1) as $t) ^ -((z & 1) as $t);
                range(v as i128, b)?;
                Ok(v)
            }
            fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim($name, b) }
        }
    };
}
unsigned!(u16, 16, "u16");
unsigned!(u32, 32, "u32");
unsigned!(u64, 64, "u64");
signed!(i16, u16, 16, "i16");
signed!(i32, u32, 32, "i32");
signed!(i64, u64, 64, "i64");

impl Wire for u8 {
    fn put(&self, w: &mut Vec<u8>) { w.push(*self) }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
        let v = r.byte()?;
        range(v as i128, b)?;
        Ok(v)
    }
    fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim("u8", b) }
}

impl Wire for i8 {
    fn put(&self, w: &mut Vec<u8>) { w.push(*self as u8) }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
        let v = r.byte()? as i8;
        range(v as i128, b)?;
        Ok(v)
    }
    fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim("i8", b) }
}

impl Wire for bool {
    fn put(&self, w: &mut Vec<u8>) { w.push(*self as u8) }
    fn get(r: &mut Reader<'_>, _: &Bounds) -> Result<Self, Error> {
        match r.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::BadTag),
        }
    }
    fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim("bool", b) }
}

impl Wire for f32 {
    fn put(&self, w: &mut Vec<u8>) { w.extend_from_slice(&self.to_le_bytes()) }
    fn get(r: &mut Reader<'_>, _: &Bounds) -> Result<Self, Error> {
        Ok(f32::from_le_bytes(r.take(4)?.try_into().unwrap()))
    }
    fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim("f32", b) }
}

impl Wire for f64 {
    fn put(&self, w: &mut Vec<u8>) { w.extend_from_slice(&self.to_le_bytes()) }
    fn get(r: &mut Reader<'_>, _: &Bounds) -> Result<Self, Error> {
        Ok(f64::from_le_bytes(r.take(8)?.try_into().unwrap()))
    }
    fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::prim("f64", b) }
}

fn length(r: &mut Reader<'_>, b: &Bounds) -> Result<usize, Error> {
    let n = r.varint(64)?;
    let max = b.len();
    if n > max {
        return Err(Error::TooLong { field: b.field, len: n, max });
    }
    Ok(n as usize)
}

impl Wire for String {
    fn put(&self, w: &mut Vec<u8>) {
        put_varint(w, self.len() as u64);
        w.extend_from_slice(self.as_bytes());
    }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
        let n = length(r, b)?;
        let s = std::str::from_utf8(r.take(n)?).map_err(|_| Error::BadUtf8 { field: b.field })?;
        if b.no_control && s.chars().any(|c| c.is_control() && c != '\t') {
            return Err(Error::ControlChar { field: b.field });
        }
        Ok(s.to_string())
    }
    fn ty(_: &mut Registry, b: &Bounds) -> Ty { Ty::string(b) }
}

impl<T: Wire> Wire for Vec<T> {
    fn put(&self, w: &mut Vec<u8>) {
        put_varint(w, self.len() as u64);
        for x in self {
            x.put(w);
        }
    }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
        let n = length(r, b)?;
        // Every item is at least a byte, except zero-sized ones: never
        // reserve more than the input could hold.
        let mut v = Vec::with_capacity(n.min(r.remaining()));
        let ib = b.items();
        for _ in 0..n {
            v.push(T::get(r, &ib)?);
        }
        Ok(v)
    }
    fn ty(reg: &mut Registry, b: &Bounds) -> Ty { Ty::vec(T::ty(reg, &b.items()), b.len()) }
}

impl<T: Wire> Wire for Option<T> {
    fn put(&self, w: &mut Vec<u8>) {
        match self {
            None => w.push(0),
            Some(x) => {
                w.push(1);
                x.put(w);
            }
        }
    }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
        match r.byte()? {
            0 => Ok(None),
            1 => Ok(Some(T::get(r, b)?)),
            _ => Err(Error::BadTag),
        }
    }
    fn ty(reg: &mut Registry, b: &Bounds) -> Ty { Ty::option(T::ty(reg, b)) }
}

impl<T: Wire> Wire for Box<T> {
    fn put(&self, w: &mut Vec<u8>) { (**self).put(w) }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> { Ok(Box::new(T::get(r, b)?)) }
    fn ty(reg: &mut Registry, b: &Bounds) -> Ty { T::ty(reg, b) }
}

impl<T: Wire, const N: usize> Wire for [T; N] {
    fn put(&self, w: &mut Vec<u8>) {
        for x in self {
            x.put(w);
        }
    }
    fn get(r: &mut Reader<'_>, b: &Bounds) -> Result<Self, Error> {
        let mut v = Vec::with_capacity(N.min(r.remaining()));
        for _ in 0..N {
            v.push(T::get(r, b)?);
        }
        v.try_into().map_err(|_| Error::Truncated)
    }
    fn ty(reg: &mut Registry, b: &Bounds) -> Ty { Ty::array(T::ty(reg, b), N) }
}

#[doc(hidden)]
pub mod __private {
    pub use super::{Bounds, Error, Reader, Registry, Ty, Wire};
    pub use crate::put_varint;
    pub use crate::schema::{Field, TypeDef, Variant};
}
