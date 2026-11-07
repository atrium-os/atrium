//! SDP client, sans-I/O (Core 6.0 Vol 3 Part B).
//!
//! Vinculum asks a device one question per service: a
//! ServiceSearchAttributeRequest for a UUID, following continuations. The
//! answer is untrusted bytes from a peer, so the data-element parser is
//! bounded: nesting depth [`MAX_DEPTH`], element count [`MAX_ELEMENTS`],
//! total response [`MAX_RESPONSE`], continuation rounds [`MAX_ROUNDS`].
//! It is fuzzed (`fuzz/fuzz_targets/sdp.rs`).
#![forbid(unsafe_code)]

pub const PSM: u16 = 0x0001;
pub const MAX_DEPTH: usize = 8;
pub const MAX_ELEMENTS: usize = 4096;
pub const MAX_RESPONSE: usize = 64 * 1024;
pub const MAX_ROUNDS: usize = 32;

pub mod uuid {
    pub const HID: u16 = 0x1124;
    pub const PNP_INFO: u16 = 0x1200;
}

pub mod attr {
    pub const SERVICE_NAME: u16 = 0x0100; // primary language base 0x0100 + 0
    pub const HID_DESCRIPTOR_LIST: u16 = 0x0206;
    pub const VENDOR_ID: u16 = 0x0201;
    pub const PRODUCT_ID: u16 = 0x0202;
    pub const VERSION: u16 = 0x0203;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Element {
    Nil,
    Uint(u64),
    /// 128-bit unsigned (rare; kept whole).
    Uint128(u128),
    Int(i64),
    /// 16- and 32-bit UUIDs are widened onto the Bluetooth base UUID.
    Uuid(u128),
    Text(Vec<u8>),
    Bool(bool),
    Seq(Vec<Element>),
    Alt(Vec<Element>),
    Url(Vec<u8>),
}

const BASE_UUID: u128 = 0x0000_0000_0000_1000_8000_0080_5F9B_34FB;

pub fn uuid16(v: u16) -> u128 {
    BASE_UUID | ((v as u128) << 96)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated,
    TooDeep,
    TooMany,
    TooLong,
    BadType,
    /// The device answered with an SDP Error Response.
    Refused(u16),
    BadPdu,
}

struct Parser<'a> {
    b: &'a [u8],
    count: usize,
}

impl<'a> Parser<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.b.len() < n {
            return Err(Error::Truncated);
        }
        let (a, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(a)
    }

    fn element(&mut self, depth: usize) -> Result<Element, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.count += 1;
        if self.count > MAX_ELEMENTS {
            return Err(Error::TooMany);
        }
        let d = self.take(1)?[0];
        let (ty, size_idx) = (d >> 3, d & 7);
        let len = match size_idx {
            0 => 1,
            1 => 2,
            2 => 4,
            3 => 8,
            4 => 16,
            5 => self.take(1)?[0] as usize,
            6 => u16::from_be_bytes(self.take(2)?.try_into().expect("2")) as usize,
            _ => u32::from_be_bytes(self.take(4)?.try_into().expect("4")) as usize,
        };
        if ty == 0 {
            // Nil has size index 0 and no data.
            return if size_idx == 0 { Ok(Element::Nil) } else { Err(Error::BadType) };
        }
        if len > MAX_RESPONSE {
            return Err(Error::TooLong);
        }
        let data = self.take(len)?;
        let int = |data: &[u8]| -> Result<u64, Error> {
            if size_idx > 3 {
                return Err(Error::BadType);
            }
            Ok(data.iter().fold(0u64, |a, b| a << 8 | *b as u64))
        };
        Ok(match ty {
            1 if size_idx == 4 => Element::Uint128(u128::from_be_bytes(data.try_into().expect("16"))),
            1 => Element::Uint(int(data)?),
            2 => {
                let v = int(data)?;
                let bits = 8 * data.len() as u32;
                Element::Int(((v << (64 - bits)) as i64) >> (64 - bits))
            }
            3 => match size_idx {
                1 => Element::Uuid(uuid16(u16::from_be_bytes(data.try_into().expect("2")))),
                2 => Element::Uuid(BASE_UUID | ((u32::from_be_bytes(data.try_into().expect("4")) as u128) << 96)),
                4 => Element::Uuid(u128::from_be_bytes(data.try_into().expect("16"))),
                _ => return Err(Error::BadType),
            },
            4 if size_idx >= 5 => Element::Text(data.to_vec()),
            5 if size_idx == 0 => Element::Bool(data[0] != 0),
            6 | 7 if size_idx >= 5 => {
                let mut inner = Parser { b: data, count: self.count };
                let mut v = Vec::new();
                while !inner.b.is_empty() {
                    v.push(inner.element(depth + 1)?);
                }
                self.count = inner.count;
                if ty == 6 {
                    Element::Seq(v)
                } else {
                    Element::Alt(v)
                }
            }
            8 if size_idx >= 5 => Element::Url(data.to_vec()),
            _ => return Err(Error::BadType),
        })
    }
}

/// Parses one data element that fills `b` exactly.
pub fn parse(b: &[u8]) -> Result<Element, Error> {
    let mut p = Parser { b, count: 0 };
    let e = p.element(0)?;
    if !p.b.is_empty() {
        return Err(Error::TooLong);
    }
    Ok(e)
}

fn de_seq(body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 3);
    if body.len() < 256 {
        v.push(0x35);
        v.push(body.len() as u8);
    } else {
        v.push(0x36);
        v.extend_from_slice(&(body.len() as u16).to_be_bytes());
    }
    v.extend_from_slice(body);
    v
}

/// A record: attribute id → value.
pub type Record = Vec<(u16, Element)>;

/// One ServiceSearchAttribute query, across continuations.
pub struct Query {
    uuid: u16,
    attrs: Vec<u16>,
    tid: u16,
    acc: Vec<u8>,
    rounds: usize,
}

impl Query {
    /// Asks for `attrs` of every record with `uuid`. Returns the query and
    /// the first request PDU to send on the SDP channel.
    pub fn new(uuid: u16, attrs: &[u16]) -> (Query, Vec<u8>) {
        let mut q = Query { uuid, attrs: attrs.to_vec(), tid: 0, acc: Vec::new(), rounds: 0 };
        let r = q.request(&[]);
        (q, r)
    }

    fn request(&mut self, cont: &[u8]) -> Vec<u8> {
        self.tid = self.tid.wrapping_add(1);
        let mut params = de_seq(&[0x19, (self.uuid >> 8) as u8, self.uuid as u8]);
        params.extend_from_slice(&0x0400u16.to_be_bytes()); // max attribute bytes per response
        let mut ids = Vec::new();
        for a in &self.attrs {
            ids.extend_from_slice(&[0x09, (a >> 8) as u8, *a as u8]);
        }
        params.extend(de_seq(&ids));
        params.push(cont.len() as u8);
        params.extend_from_slice(cont);
        let mut v = vec![0x06];
        v.extend_from_slice(&self.tid.to_be_bytes());
        v.extend_from_slice(&(params.len() as u16).to_be_bytes());
        v.extend(params);
        v
    }

    /// A response PDU. `Ok(Err(req))`: send `req` (a continuation);
    /// `Ok(Ok(records))`: done.
    #[allow(clippy::type_complexity)]
    pub fn on_response(&mut self, pdu: &[u8]) -> Result<Result<Vec<Record>, Vec<u8>>, Error> {
        if pdu.len() < 5 {
            return Err(Error::BadPdu);
        }
        let (id, tid, plen) = (pdu[0], u16::from_be_bytes([pdu[1], pdu[2]]), u16::from_be_bytes([pdu[3], pdu[4]]) as usize);
        if tid != self.tid || pdu.len() != 5 + plen {
            return Err(Error::BadPdu);
        }
        let p = &pdu[5..];
        match id {
            0x01 if p.len() >= 2 => return Err(Error::Refused(u16::from_be_bytes([p[0], p[1]]))),
            0x07 => {}
            _ => return Err(Error::BadPdu),
        }
        if p.len() < 2 {
            return Err(Error::BadPdu);
        }
        let n = u16::from_be_bytes([p[0], p[1]]) as usize;
        if p.len() < 2 + n + 1 {
            return Err(Error::BadPdu);
        }
        let cl = p[2 + n] as usize;
        if cl > 16 || p.len() != 3 + n + cl {
            return Err(Error::BadPdu);
        }
        if self.acc.len() + n > MAX_RESPONSE {
            return Err(Error::TooLong);
        }
        self.acc.extend_from_slice(&p[2..2 + n]);
        if cl > 0 {
            self.rounds += 1;
            if self.rounds > MAX_ROUNDS {
                return Err(Error::TooMany);
            }
            let cont = p[3 + n..].to_vec();
            return Ok(Err(self.request(&cont)));
        }
        let lists = parse(&self.acc)?;
        let Element::Seq(records) = lists else { return Err(Error::BadType) };
        let mut out = Vec::new();
        for r in records {
            let Element::Seq(items) = r else { return Err(Error::BadType) };
            if items.len() % 2 != 0 {
                return Err(Error::BadType);
            }
            let mut rec = Vec::new();
            let mut it = items.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                let Element::Uint(k) = k else { return Err(Error::BadType) };
                rec.push((k as u16, v));
            }
            out.push(rec);
        }
        Ok(Ok(out))
    }
}

pub fn get(r: &Record, id: u16) -> Option<&Element> {
    r.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
}

/// The HID report descriptor from a HID record's HIDDescriptorList
/// (`Seq[Seq[Uint8 0x22, Text descriptor]]`).
pub fn hid_descriptor(r: &Record) -> Option<Vec<u8>> {
    let Element::Seq(list) = get(r, attr::HID_DESCRIPTOR_LIST)? else { return None };
    list.iter().find_map(|d| match d {
        Element::Seq(pair) => match pair.as_slice() {
            [Element::Uint(0x22), Element::Text(bytes)] => Some(bytes.clone()),
            _ => None,
        },
        _ => None,
    })
}

pub fn text(r: &Record, id: u16) -> Option<String> {
    match get(r, id)? {
        Element::Text(t) => Some(String::from_utf8_lossy(t).trim_end_matches('\0').to_string()),
        _ => None,
    }
}

pub fn uint(r: &Record, id: u16) -> Option<u64> {
    match get(r, id)? {
        Element::Uint(v) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(tid: u16, lists: &[u8], cont: &[u8]) -> Vec<u8> {
        let mut p = (lists.len() as u16).to_be_bytes().to_vec();
        p.extend_from_slice(lists);
        p.push(cont.len() as u8);
        p.extend_from_slice(cont);
        let mut v = vec![0x07];
        v.extend_from_slice(&tid.to_be_bytes());
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend(p);
        v
    }

    fn hid_record(desc: &[u8]) -> Vec<u8> {
        let mut text = vec![0x25, desc.len() as u8];
        text.extend_from_slice(desc);
        let mut pair = vec![0x08, 0x22];
        pair.extend(text);
        let list = de_seq(&de_seq(&pair));
        let mut items = vec![0x09, 0x02, 0x06];
        items.extend(list);
        items.extend_from_slice(&[0x09, 0x01, 0x00, 0x25, 3, b'K', b'b', b'd']);
        de_seq(&de_seq(&items))
    }

    #[test]
    fn query_with_continuation_extracts_the_descriptor() {
        let desc = [0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0xC0];
        let lists = hid_record(&desc);
        let (mut q, req) = Query::new(uuid::HID, &[attr::HID_DESCRIPTOR_LIST, attr::SERVICE_NAME]);
        assert_eq!(&req[..3], &[0x06, 0, 1]);
        let (a, b) = lists.split_at(5);
        let next = q.on_response(&response(1, a, &[0xAB])).unwrap().unwrap_err();
        assert_eq!(*next.last().unwrap(), 0xAB, "continuation state echoed");
        let recs = q.on_response(&response(2, b, &[])).unwrap().unwrap();
        assert_eq!(hid_descriptor(&recs[0]).unwrap(), desc);
        assert_eq!(text(&recs[0], attr::SERVICE_NAME).unwrap(), "Kbd");
    }

    #[test]
    fn hostile_input_is_bounded() {
        // Deep nesting.
        let mut e = vec![0x08, 0x01];
        for _ in 0..20 {
            e = de_seq(&e);
        }
        assert_eq!(parse(&e), Err(Error::TooDeep));
        // A length past the end.
        assert_eq!(parse(&[0x35, 0x09, 0x08, 0x01]), Err(Error::Truncated));
        // Wrong transaction id, bad lengths, refusals.
        let (mut q, _) = Query::new(uuid::HID, &[attr::HID_DESCRIPTOR_LIST]);
        assert_eq!(q.on_response(&response(9, &[0x35, 0], &[])), Err(Error::BadPdu));
        assert_eq!(q.on_response(&[0x01, 0, 1, 0, 2, 0, 3]), Err(Error::Refused(3)));
        assert_eq!(q.on_response(&response(1, &[0x35, 0], &[0; 17])), Err(Error::BadPdu));
        // Endless continuations.
        let (mut q, _) = Query::new(uuid::HID, &[attr::HID_DESCRIPTOR_LIST]);
        let mut tid = 1;
        let mut r = Ok(Err(vec![]));
        for _ in 0..40 {
            r = q.on_response(&response(tid, &[0x08], &[1]));
            if r.is_err() {
                break;
            }
            tid += 1;
        }
        assert_eq!(r, Err(Error::TooMany));
        assert_eq!(parse(&[0x09, 0x12, 0x34]), Ok(Element::Uint(0x1234)));
        assert_eq!(parse(&[0x10, 0xFF]), Ok(Element::Int(-1)));
        assert_eq!(parse(&[0x19, 0x11, 0x24]), Ok(Element::Uuid(uuid16(0x1124))));
    }
}
