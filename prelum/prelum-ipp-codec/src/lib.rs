//! IPP/2.x message encoding and decoding (RFC 8010), written from the
//! specification for Prelum (docs/spec/printing.md §6.3).
//!
//! The decoder is for bytes from a printer, which are hostile: every length and
//! count is checked against [`Limits`] before anything is allocated, nesting is
//! bounded, and nothing panics on any input (a fuzz target checks that). The
//! encoder writes what Prelum sends; [`ops`] builds the requests Prelum uses and
//! [`printer`] turns a `Get-Printer-Attributes` answer into checked values.

#![forbid(unsafe_code)]

pub mod ops;
pub mod printer;

use std::fmt;

/// Delimiter and value tags (RFC 8010 §3.5).
pub mod tag {
    pub const OPERATION: u8 = 0x01;
    pub const JOB: u8 = 0x02;
    pub const END: u8 = 0x03;
    pub const PRINTER: u8 = 0x04;
    pub const UNSUPPORTED_GROUP: u8 = 0x05;
    pub const SUBSCRIPTION: u8 = 0x06;
    pub const EVENT_NOTIFICATION: u8 = 0x07;
    pub const RESOURCE: u8 = 0x08;
    pub const DOCUMENT: u8 = 0x09;
    pub const SYSTEM: u8 = 0x0A;

    pub const UNSUPPORTED: u8 = 0x10;
    pub const UNKNOWN: u8 = 0x12;
    pub const NO_VALUE: u8 = 0x13;
    pub const INTEGER: u8 = 0x21;
    pub const BOOLEAN: u8 = 0x22;
    pub const ENUM: u8 = 0x23;
    pub const OCTET_STRING: u8 = 0x30;
    pub const DATE_TIME: u8 = 0x31;
    pub const RESOLUTION: u8 = 0x32;
    pub const RANGE_OF_INTEGER: u8 = 0x33;
    pub const BEG_COLLECTION: u8 = 0x34;
    pub const TEXT_WITH_LANGUAGE: u8 = 0x35;
    pub const NAME_WITH_LANGUAGE: u8 = 0x36;
    pub const END_COLLECTION: u8 = 0x37;
    pub const TEXT: u8 = 0x41;
    pub const NAME: u8 = 0x42;
    pub const KEYWORD: u8 = 0x44;
    pub const URI: u8 = 0x45;
    pub const URI_SCHEME: u8 = 0x46;
    pub const CHARSET: u8 = 0x47;
    pub const NATURAL_LANGUAGE: u8 = 0x48;
    pub const MIME_MEDIA_TYPE: u8 = 0x49;
    pub const MEMBER_ATTR_NAME: u8 = 0x4A;
    pub const EXTENSION: u8 = 0x7F;
}

/// Bounds for decoding. The defaults fit any real printer's answer to
/// `Get-Printer-Attributes` (the largest seen are under 100 KiB) with room.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes of the attribute part (header to end-of-attributes tag).
    pub max_bytes: usize,
    /// Attributes, collection members included.
    pub max_attributes: usize,
    /// Values, over the whole message.
    pub max_values: usize,
    /// Collections inside collections.
    pub max_depth: usize,
    /// Groups.
    pub max_groups: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_bytes: 1 << 20, max_attributes: 8192, max_values: 65536, max_depth: 8, max_groups: 64 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The message ended inside a field.
    Truncated,
    /// A bound in [`Limits`] was passed; names which.
    TooLarge(&'static str),
    /// Collections nested past [`Limits::max_depth`].
    TooDeep,
    /// A value before any group, an additional value with nothing before it, and
    /// similar structural faults.
    Malformed(&'static str),
    /// A fixed-size value with the wrong length.
    BadLength { tag: u8 },
    /// The extension tag (0x7F) or a reserved delimiter tag.
    UnsupportedTag(u8),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "message truncated"),
            Error::TooLarge(what) => write!(f, "too many {what}"),
            Error::TooDeep => write!(f, "collections nested too deep"),
            Error::Malformed(what) => write!(f, "malformed: {what}"),
            Error::BadLength { tag } => write!(f, "bad length for value tag {tag:#04x}"),
            Error::UnsupportedTag(t) => write!(f, "unsupported tag {t:#04x}"),
        }
    }
}

impl std::error::Error for Error {}

/// One attribute value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Integer(i32),
    Boolean(bool),
    Enum(i32),
    OctetString(Vec<u8>),
    DateTime([u8; 11]),
    /// `units`: 3 = dots per inch, 4 = dots per centimetre.
    Resolution { x: i32, y: i32, units: u8 },
    Range { lower: i32, upper: i32 },
    TextWithLanguage { language: String, text: String },
    NameWithLanguage { language: String, name: String },
    Text(String),
    Name(String),
    Keyword(String),
    Uri(String),
    UriScheme(String),
    Charset(String),
    NaturalLanguage(String),
    MimeMediaType(String),
    Collection(Vec<Member>),
    Unsupported,
    Unknown,
    NoValue,
    /// A value tag this codec doesn't interpret, kept as bytes.
    Other { tag: u8, bytes: Vec<u8> },
}

/// One member of a collection value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub values: Vec<Value>,
}

impl Value {
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Integer(i) | Value::Enum(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }
    /// Any string-like value's text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s)
            | Value::Name(s)
            | Value::Keyword(s)
            | Value::Uri(s)
            | Value::UriScheme(s)
            | Value::Charset(s)
            | Value::NaturalLanguage(s)
            | Value::MimeMediaType(s) => Some(s),
            Value::TextWithLanguage { text, .. } => Some(text),
            Value::NameWithLanguage { name, .. } => Some(name),
            _ => None,
        }
    }
    pub fn as_collection(&self) -> Option<&[Member]> {
        match self {
            Value::Collection(m) => Some(m),
            _ => None,
        }
    }
    fn tag(&self) -> u8 {
        match self {
            Value::Integer(_) => tag::INTEGER,
            Value::Boolean(_) => tag::BOOLEAN,
            Value::Enum(_) => tag::ENUM,
            Value::OctetString(_) => tag::OCTET_STRING,
            Value::DateTime(_) => tag::DATE_TIME,
            Value::Resolution { .. } => tag::RESOLUTION,
            Value::Range { .. } => tag::RANGE_OF_INTEGER,
            Value::TextWithLanguage { .. } => tag::TEXT_WITH_LANGUAGE,
            Value::NameWithLanguage { .. } => tag::NAME_WITH_LANGUAGE,
            Value::Text(_) => tag::TEXT,
            Value::Name(_) => tag::NAME,
            Value::Keyword(_) => tag::KEYWORD,
            Value::Uri(_) => tag::URI,
            Value::UriScheme(_) => tag::URI_SCHEME,
            Value::Charset(_) => tag::CHARSET,
            Value::NaturalLanguage(_) => tag::NATURAL_LANGUAGE,
            Value::MimeMediaType(_) => tag::MIME_MEDIA_TYPE,
            Value::Collection(_) => tag::BEG_COLLECTION,
            Value::Unsupported => tag::UNSUPPORTED,
            Value::Unknown => tag::UNKNOWN,
            Value::NoValue => tag::NO_VALUE,
            Value::Other { tag, .. } => *tag,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub values: Vec<Value>,
}

impl Attribute {
    pub fn new(name: &str, value: Value) -> Self {
        Attribute { name: name.to_string(), values: vec![value] }
    }
    pub fn many(name: &str, values: Vec<Value>) -> Self {
        Attribute { name: name.to_string(), values }
    }
    pub fn first(&self) -> Option<&Value> {
        self.values.first()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub tag: u8,
    pub attributes: Vec<Attribute>,
}

/// An IPP request or response. `code` is the operation id in a request and
/// the status code in a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub version: (u8, u8),
    pub code: u16,
    pub request_id: u32,
    pub groups: Vec<Group>,
}

impl Message {
    /// First attribute named `name` in the first group with `group_tag`.
    pub fn attribute(&self, group_tag: u8, name: &str) -> Option<&Attribute> {
        self.groups
            .iter()
            .filter(|g| g.tag == group_tag)
            .flat_map(|g| g.attributes.iter())
            .find(|a| a.name == name)
    }

    pub fn group(&self, group_tag: u8) -> Option<&Group> {
        self.groups.iter().find(|g| g.tag == group_tag)
    }

    /// Status class of a response: successful-ok and the other 0x00xx codes.
    pub fn is_success(&self) -> bool {
        self.code < 0x0100
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.push(self.version.0);
        out.push(self.version.1);
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.request_id.to_be_bytes());
        for g in &self.groups {
            out.push(g.tag);
            for a in &g.attributes {
                encode_attribute(&mut out, &a.name, &a.values);
            }
        }
        out.push(tag::END);
        out
    }

    /// Decodes a message. Returns it and the offset where document data (if
    /// any) starts.
    pub fn decode(buf: &[u8], limits: &Limits) -> Result<(Message, usize), Error> {
        Decoder { buf, pos: 0, limits, attributes: 0, values: 0 }.message()
    }
}

/// Status codes Prelum acts on (RFC 8011 §5.4.15 and extensions).
pub mod status {
    pub const SUCCESSFUL_OK: u16 = 0x0000;
    pub const SUCCESSFUL_OK_IGNORED_OR_SUBSTITUTED: u16 = 0x0001;
    pub const SUCCESSFUL_OK_CONFLICTING: u16 = 0x0002;
    pub const CLIENT_ERROR_BAD_REQUEST: u16 = 0x0400;
    pub const CLIENT_ERROR_FORBIDDEN: u16 = 0x0401;
    pub const CLIENT_ERROR_NOT_AUTHENTICATED: u16 = 0x0402;
    pub const CLIENT_ERROR_NOT_AUTHORIZED: u16 = 0x0403;
    pub const CLIENT_ERROR_NOT_POSSIBLE: u16 = 0x0404;
    pub const CLIENT_ERROR_NOT_FOUND: u16 = 0x0406;
    pub const CLIENT_ERROR_DOCUMENT_FORMAT_NOT_SUPPORTED: u16 = 0x040A;
    pub const CLIENT_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED: u16 = 0x040B;
    pub const SERVER_ERROR_BUSY: u16 = 0x0507;
    pub const SERVER_ERROR_JOB_CANCELED: u16 = 0x0508;
}

fn put_u16(out: &mut Vec<u8>, v: usize) {
    // Callers pass lengths of values Prelum built; anything over u16 is cut,
    // never wrapped.
    let v = v.min(u16::MAX as usize) as u16;
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let n = bytes.len().min(u16::MAX as usize);
    put_u16(out, n);
    out.extend_from_slice(&bytes[..n]);
}

fn encode_attribute(out: &mut Vec<u8>, name: &str, values: &[Value]) {
    for (i, v) in values.iter().enumerate() {
        let n = if i == 0 { name } else { "" };
        encode_value(out, n, v);
    }
    if values.is_empty() {
        encode_value(out, name, &Value::NoValue);
    }
}

fn encode_value(out: &mut Vec<u8>, name: &str, v: &Value) {
    out.push(v.tag());
    put_field(out, name.as_bytes());
    match v {
        Value::Integer(i) | Value::Enum(i) => {
            put_u16(out, 4);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Boolean(b) => {
            put_u16(out, 1);
            out.push(*b as u8);
        }
        Value::OctetString(b) | Value::Other { bytes: b, .. } => put_field(out, b),
        Value::DateTime(d) => put_field(out, d),
        Value::Resolution { x, y, units } => {
            put_u16(out, 9);
            out.extend_from_slice(&x.to_be_bytes());
            out.extend_from_slice(&y.to_be_bytes());
            out.push(*units);
        }
        Value::Range { lower, upper } => {
            put_u16(out, 8);
            out.extend_from_slice(&lower.to_be_bytes());
            out.extend_from_slice(&upper.to_be_bytes());
        }
        Value::TextWithLanguage { language: l, text: t } | Value::NameWithLanguage { language: l, name: t } => {
            let l = &l.as_bytes()[..l.len().min(0x7fff)];
            let t = &t.as_bytes()[..t.len().min(0x7fff)];
            put_u16(out, 4 + l.len() + t.len());
            put_field(out, l);
            put_field(out, t);
        }
        Value::Text(s)
        | Value::Name(s)
        | Value::Keyword(s)
        | Value::Uri(s)
        | Value::UriScheme(s)
        | Value::Charset(s)
        | Value::NaturalLanguage(s)
        | Value::MimeMediaType(s) => put_field(out, s.as_bytes()),
        Value::Collection(members) => {
            put_u16(out, 0);
            for m in members {
                out.push(tag::MEMBER_ATTR_NAME);
                put_u16(out, 0);
                put_field(out, m.name.as_bytes());
                for mv in &m.values {
                    encode_value(out, "", mv);
                }
                if m.values.is_empty() {
                    encode_value(out, "", &Value::NoValue);
                }
            }
            out.push(tag::END_COLLECTION);
            put_u16(out, 0);
            put_u16(out, 0);
        }
        Value::Unsupported | Value::Unknown | Value::NoValue => put_u16(out, 0),
    }
}

struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    limits: &'a Limits,
    attributes: usize,
    values: usize,
}

impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        if end > self.buf.len() {
            return Err(Error::Truncated);
        }
        if end > self.limits.max_bytes {
            return Err(Error::TooLarge("bytes"));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn field(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u16()? as usize;
        self.take(n)
    }

    fn message(mut self) -> Result<(Message, usize), Error> {
        let h = self.take(8)?;
        let version = (h[0], h[1]);
        let code = u16::from_be_bytes([h[2], h[3]]);
        let request_id = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
        let mut groups: Vec<Group> = Vec::new();
        loop {
            let t = self.u8()?;
            if t == tag::END {
                break;
            }
            if t < 0x10 {
                if t == 0x00 {
                    return Err(Error::UnsupportedTag(t));
                }
                if groups.len() >= self.limits.max_groups {
                    return Err(Error::TooLarge("groups"));
                }
                groups.push(Group { tag: t, attributes: Vec::new() });
                continue;
            }
            let group = groups.last_mut().ok_or(Error::Malformed("value before any group"))?;
            let name = self.field()?;
            if name.is_empty() {
                let attr = group.attributes.last_mut().ok_or(Error::Malformed("additional value with no attribute"))?;
                let v = Self::value_into(&mut self, t, 0)?;
                attr.values.push(v);
            } else {
                self.count_attribute()?;
                let name = text(name);
                let v = Self::value_into(&mut self, t, 0)?;
                group.attributes.push(Attribute { name, values: vec![v] });
            }
        }
        Ok((Message { version, code, request_id, groups }, self.pos))
    }

    fn count_attribute(&mut self) -> Result<(), Error> {
        self.attributes += 1;
        if self.attributes > self.limits.max_attributes {
            return Err(Error::TooLarge("attributes"));
        }
        Ok(())
    }

    // An associated function so the borrow of a group in `message` can stay
    // live while the decoder reads on.
    fn value_into(d: &mut Decoder<'a>, t: u8, depth: usize) -> Result<Value, Error> {
        d.values += 1;
        if d.values > d.limits.max_values {
            return Err(Error::TooLarge("values"));
        }
        if t == tag::EXTENSION {
            return Err(Error::UnsupportedTag(t));
        }
        let bytes = d.field()?;
        let fixed = |n: usize| if bytes.len() == n { Ok(()) } else { Err(Error::BadLength { tag: t }) };
        let be32 = |b: &[u8]| i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        Ok(match t {
            tag::INTEGER => {
                fixed(4)?;
                Value::Integer(be32(bytes))
            }
            tag::ENUM => {
                fixed(4)?;
                Value::Enum(be32(bytes))
            }
            tag::BOOLEAN => {
                fixed(1)?;
                Value::Boolean(bytes[0] != 0)
            }
            tag::DATE_TIME => {
                fixed(11)?;
                let mut a = [0u8; 11];
                a.copy_from_slice(bytes);
                Value::DateTime(a)
            }
            tag::RESOLUTION => {
                fixed(9)?;
                Value::Resolution { x: be32(&bytes[0..4]), y: be32(&bytes[4..8]), units: bytes[8] }
            }
            tag::RANGE_OF_INTEGER => {
                fixed(8)?;
                Value::Range { lower: be32(&bytes[0..4]), upper: be32(&bytes[4..8]) }
            }
            tag::TEXT_WITH_LANGUAGE | tag::NAME_WITH_LANGUAGE => {
                let (l, s) = split_lang(bytes).ok_or(Error::BadLength { tag: t })?;
                if t == tag::TEXT_WITH_LANGUAGE {
                    Value::TextWithLanguage { language: text(l), text: text(s) }
                } else {
                    Value::NameWithLanguage { language: text(l), name: text(s) }
                }
            }
            tag::TEXT => Value::Text(text(bytes)),
            tag::NAME => Value::Name(text(bytes)),
            tag::KEYWORD => Value::Keyword(text(bytes)),
            tag::URI => Value::Uri(text(bytes)),
            tag::URI_SCHEME => Value::UriScheme(text(bytes)),
            tag::CHARSET => Value::Charset(text(bytes)),
            tag::NATURAL_LANGUAGE => Value::NaturalLanguage(text(bytes)),
            tag::MIME_MEDIA_TYPE => Value::MimeMediaType(text(bytes)),
            tag::OCTET_STRING => Value::OctetString(bytes.to_vec()),
            tag::UNSUPPORTED => Value::Unsupported,
            tag::UNKNOWN => Value::Unknown,
            tag::NO_VALUE => Value::NoValue,
            tag::BEG_COLLECTION => Value::Collection(d.collection(depth + 1)?),
            tag::END_COLLECTION | tag::MEMBER_ATTR_NAME => return Err(Error::Malformed("collection tag outside a collection")),
            _ => Value::Other { tag: t, bytes: bytes.to_vec() },
        })
    }

    fn collection(&mut self, depth: usize) -> Result<Vec<Member>, Error> {
        if depth > self.limits.max_depth {
            return Err(Error::TooDeep);
        }
        let mut members: Vec<Member> = Vec::new();
        loop {
            let t = self.u8()?;
            if t < 0x10 {
                return Err(Error::Malformed("group tag inside a collection"));
            }
            let name = self.field()?;
            if !name.is_empty() {
                return Err(Error::Malformed("named value inside a collection"));
            }
            match t {
                tag::END_COLLECTION => {
                    let _ = self.field()?;
                    return Ok(members);
                }
                tag::MEMBER_ATTR_NAME => {
                    self.count_attribute()?;
                    let n = self.field()?;
                    if n.is_empty() {
                        return Err(Error::Malformed("empty member name"));
                    }
                    members.push(Member { name: text(n), values: Vec::new() });
                }
                _ => {
                    let v = Self::value_into(self, t, depth)?;
                    let m = members.last_mut().ok_or(Error::Malformed("collection value before a member name"))?;
                    m.values.push(v);
                }
            }
        }
    }
}

fn split_lang(b: &[u8]) -> Option<(&[u8], &[u8])> {
    if b.len() < 2 {
        return None;
    }
    let ln = u16::from_be_bytes([b[0], b[1]]) as usize;
    let rest = b.get(2..)?;
    let l = rest.get(..ln)?;
    let rest = rest.get(ln..)?;
    if rest.len() < 2 {
        return None;
    }
    let tn = u16::from_be_bytes([rest[0], rest[1]]) as usize;
    let t = rest.get(2..2 + tn)?;
    if rest.len() != 2 + tn {
        return None;
    }
    Some((l, t))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Text from a printer, made safe to show: control characters and bidi
/// overrides dropped, cut to at most `max` bytes on a character boundary
/// (spec §6.3: free text is at most 200 bytes of printable UTF-8).
pub fn display_text(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let bad = c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}');
        if bad {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        Message {
            version: (2, 0),
            code: 0x000B,
            request_id: 7,
            groups: vec![
                Group {
                    tag: tag::OPERATION,
                    attributes: vec![
                        Attribute::new("attributes-charset", Value::Charset("utf-8".into())),
                        Attribute::new("attributes-natural-language", Value::NaturalLanguage("en".into())),
                        Attribute::new("printer-uri", Value::Uri("ipp://p.local/ipp/print".into())),
                        Attribute::many("requested-attributes", vec![Value::Keyword("all".into()), Value::Keyword("media-col-database".into())]),
                    ],
                },
                Group {
                    tag: tag::JOB,
                    attributes: vec![
                        Attribute::new(
                            "media-col",
                            Value::Collection(vec![
                                Member {
                                    name: "media-size".into(),
                                    values: vec![Value::Collection(vec![
                                        Member { name: "x-dimension".into(), values: vec![Value::Integer(21000)] },
                                        Member { name: "y-dimension".into(), values: vec![Value::Integer(29700)] },
                                    ])],
                                },
                                Member { name: "media-type".into(), values: vec![Value::Keyword("stationery".into())] },
                            ]),
                        ),
                        Attribute::new("printer-resolution", Value::Resolution { x: 600, y: 600, units: 3 }),
                        Attribute::new("page-ranges", Value::Range { lower: 1, upper: 3 }),
                        Attribute::new("job-name", Value::NameWithLanguage { language: "en".into(), name: "Report".into() }),
                        Attribute::new("copies", Value::Integer(2)),
                        Attribute::new("print-color-mode", Value::Keyword("monochrome".into())),
                        Attribute::new("x", Value::Boolean(true)),
                    ],
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let m = sample();
        let mut b = m.encode();
        b.extend_from_slice(b"%PDF-");
        let (d, off) = Message::decode(&b, &Limits::default()).unwrap();
        assert_eq!(d, m);
        assert_eq!(&b[off..], b"%PDF-");
    }

    #[test]
    fn every_prefix_fails_cleanly() {
        let b = sample().encode();
        for n in 0..b.len() {
            assert!(Message::decode(&b[..n], &Limits::default()).is_err(), "prefix {n}");
        }
    }

    #[test]
    fn depth_is_bounded() {
        let mut v = Value::Integer(1);
        for _ in 0..20 {
            v = Value::Collection(vec![Member { name: "m".into(), values: vec![v] }]);
        }
        let m = Message { version: (2, 0), code: 0, request_id: 1, groups: vec![Group { tag: tag::JOB, attributes: vec![Attribute::new("deep", v)] }] };
        assert_eq!(Message::decode(&m.encode(), &Limits::default()).unwrap_err(), Error::TooDeep);
    }

    #[test]
    fn counts_are_bounded() {
        let vals = (0..100).map(Value::Integer).collect();
        let m = Message { version: (2, 0), code: 0, request_id: 1, groups: vec![Group { tag: tag::PRINTER, attributes: vec![Attribute::many("a", vals)] }] };
        let lim = Limits { max_values: 50, ..Limits::default() };
        assert_eq!(Message::decode(&m.encode(), &lim).unwrap_err(), Error::TooLarge("values"));
    }

    #[test]
    fn structural_faults() {
        // value before a group
        let mut b = vec![2, 0, 0, 0, 0, 0, 0, 1, tag::INTEGER, 0, 1, b'a', 0, 4, 0, 0, 0, 1, 3];
        assert!(matches!(Message::decode(&b, &Limits::default()), Err(Error::Malformed(_))));
        // bad integer length
        b = vec![2, 0, 0, 0, 0, 0, 0, 1, 1, tag::INTEGER, 0, 1, b'a', 0, 3, 0, 0, 1, 3];
        assert_eq!(Message::decode(&b, &Limits::default()).unwrap_err(), Error::BadLength { tag: tag::INTEGER });
        // extension tag
        b = vec![2, 0, 0, 0, 0, 0, 0, 1, 1, 0x7F, 0, 1, b'a', 0, 4, 0, 0, 0, 1, 3];
        assert_eq!(Message::decode(&b, &Limits::default()).unwrap_err(), Error::UnsupportedTag(0x7F));
    }

    #[test]
    fn display_text_cleans() {
        assert_eq!(display_text("Office\u{202E}evil\x07 printer", 200), "Officeevil printer");
        assert_eq!(display_text("ééé", 5), "éé");
    }
}
