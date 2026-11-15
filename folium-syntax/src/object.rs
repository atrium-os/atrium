//! PDF objects (ISO 32000-2 §7.3).

use std::fmt;
use std::sync::Arc;

/// An indirect reference: object number and generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref {
    pub num: u32,
    pub gen: u16,
}

impl Ref {
    pub const fn new(num: u32, gen: u16) -> Self { Ref { num, gen } }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "{} {} R", self.num, self.gen) }
}

/// A name, `#xx` escapes decoded.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(pub Arc<[u8]>);

impl Name {
    pub fn new(b: &[u8]) -> Self { Name(Arc::from(b)) }
    pub fn as_bytes(&self) -> &[u8] { &self.0 }
    pub fn is(&self, s: &str) -> bool { &*self.0 == s.as_bytes() }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "/{}", String::from_utf8_lossy(&self.0)) }
}

/// A dictionary. Keys are kept in file order; a repeated key keeps the
/// last value.
#[derive(Clone, Default, PartialEq)]
pub struct Dict(pub Vec<(Name, Object)>);

impl Dict {
    pub fn new() -> Self { Dict(Vec::new()) }
    pub fn get(&self, key: &str) -> Option<&Object> {
        self.0.iter().rev().find(|(k, _)| k.is(key)).map(|(_, v)| v)
    }
    pub fn insert(&mut self, key: &str, v: Object) {
        if let Some(e) = self.0.iter_mut().find(|(k, _)| k.is(key)) {
            e.1 = v;
        } else {
            self.0.push((Name::new(key.as_bytes()), v));
        }
    }
    pub fn len(&self) -> usize { self.0.len() }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    pub fn iter(&self) -> impl Iterator<Item = (&Name, &Object)> { self.0.iter().map(|(k, v)| (k, v)) }
    /// `/Type`'s name, if it has one.
    pub fn type_is(&self, t: &str) -> bool { matches!(self.get("Type"), Some(Object::Name(n)) if n.is(t)) }
    pub fn name(&self, key: &str) -> Option<&Name> { self.get(key).and_then(Object::as_name) }
    pub fn int(&self, key: &str) -> Option<i64> { self.get(key).and_then(Object::as_int) }
}

impl fmt::Debug for Dict {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("<<")?;
        for (k, v) in &self.0 {
            write!(f, " {k:?} {v:?}")?;
        }
        f.write_str(" >>")
    }
}

/// Where a stream's bytes are: in the file (read on demand), or already
/// in memory (an inline image, a test).
#[derive(Clone, PartialEq)]
pub enum StreamData {
    File { offset: u64, len: u64 },
    Bytes(Arc<[u8]>),
}

#[derive(Clone, PartialEq)]
pub struct Stream {
    pub dict: Dict,
    pub data: StreamData,
    /// The indirect object it is, which names its decryption key.
    pub owner: Option<Ref>,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.data {
            StreamData::File { offset, len } => write!(f, "{:?} stream@{offset}+{len}", self.dict),
            StreamData::Bytes(b) => write!(f, "{:?} stream[{}]", self.dict, b.len()),
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    String(Arc<[u8]>),
    Name(Name),
    Array(Vec<Object>),
    Dict(Dict),
    Stream(Stream),
    Ref(Ref),
}

impl Object {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Object::Int(i) => Some(*i),
            Object::Real(r) if r.fract() == 0.0 && r.abs() < 9.0e15 => Some(*r as i64),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Object::Int(i) => Some(*i as f64),
            Object::Real(r) if r.is_finite() => Some(*r),
            _ => None,
        }
    }
    pub fn as_name(&self) -> Option<&Name> { if let Object::Name(n) = self { Some(n) } else { None } }
    pub fn as_str(&self) -> Option<&[u8]> { if let Object::String(s) = self { Some(s) } else { None } }
    pub fn as_array(&self) -> Option<&[Object]> { if let Object::Array(a) = self { Some(a) } else { None } }
    pub fn as_ref(&self) -> Option<Ref> { if let Object::Ref(r) = self { Some(*r) } else { None } }
    pub fn as_bool(&self) -> Option<bool> { if let Object::Bool(b) = self { Some(*b) } else { None } }
    /// A dictionary, or a stream's dictionary.
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }
    pub fn as_stream(&self) -> Option<&Stream> { if let Object::Stream(s) = self { Some(s) } else { None } }
    pub fn is_null(&self) -> bool { matches!(self, Object::Null) }
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Object::Null => f.write_str("null"),
            Object::Bool(b) => write!(f, "{b}"),
            Object::Int(i) => write!(f, "{i}"),
            Object::Real(r) => write!(f, "{r}"),
            Object::String(s) => write!(f, "({})", String::from_utf8_lossy(s)),
            Object::Name(n) => write!(f, "{n:?}"),
            Object::Array(a) => f.debug_list().entries(a).finish(),
            Object::Dict(d) => write!(f, "{d:?}"),
            Object::Stream(s) => write!(f, "{s:?}"),
            Object::Ref(r) => write!(f, "{r}"),
        }
    }
}
