//! The wire format of a live page's IndexedDB (navigator-islands.md I6.7,
//! D15): the changes a committing transaction sends to navigatord at
//! [`super::LIVE_IDB`], in numbered parts, and the store as the engine loads
//! it, which is the same changes from empty. Shared by navigatord, which
//! keeps the store, and the engine, which makes and reads the changes.

/// The most bytes one site keeps, as navigatord counts them.
pub const QUOTA: usize = 50 * 1024 * 1024;
/// The most bytes one part of a run carries: Nuntius's POST bound
/// (`nuntius::BODY_MAX`, which navigatord checks).
pub const PART_MAX: usize = 32 * 1024;
/// The most bytes of changes one run carries, all parts together.
pub const RUN_MAX: usize = QUOTA + 4 * 1024 * 1024;
/// The most changes one run carries.
pub const RUN_OPS_MAX: usize = 1 << 20;
/// The most changes a loaded store is: each record or index record costs
/// navigatord at least 16 bytes of the quota, so a full store is fewer.
pub const LOAD_OPS_MAX: usize = 4 << 20;
/// The longest store the engine loads, or a file read back.
pub const LOAD_MAX: usize = 2 * QUOTA;
/// The longest name, or description of a store or an index.
pub const NAME_MAX: usize = 4096;
/// The longest key (an IndexedDB key, encoded by the engine).
pub const KEY_MAX: usize = 16 * 1024;
/// A part's header: the run (u64), the part's number (u32), last (u8).
pub const HEADER: usize = 13;

/// One change, as the engine sends it. Record and index-record changes act
/// on the selected store or index: [`Op::Database`] selects a database
/// (and no store), [`Op::Store`] a store in it (and no index),
/// [`Op::Index`] an index in that; making one selects it too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Database(Vec<u8>),
    /// Make the database if it isn't there, set its version, select it.
    Version(Vec<u8>, u64),
    DeleteDatabase(Vec<u8>),
    Store(Vec<u8>),
    /// Make (or replace, empty) a store with its description; select it.
    CreateStore(Vec<u8>, Vec<u8>),
    DeleteStore(Vec<u8>),
    RenameStore(Vec<u8>, Vec<u8>),
    KeyGenerator(u64),
    Put(Vec<u8>, Vec<u8>),
    /// More of a value too long for one part: added to the end of `key`'s.
    Append(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Clear,
    Index(Vec<u8>),
    CreateIndex(Vec<u8>, Vec<u8>),
    DeleteIndex(Vec<u8>),
    RenameIndex(Vec<u8>, Vec<u8>),
    /// An index record: the index's key and the record's primary key.
    IndexPut(Vec<u8>, Vec<u8>),
    IndexDelete(Vec<u8>, Vec<u8>),
}

/// Each change's first byte.
pub mod tag {
    pub const DATABASE: u8 = 1;
    pub const VERSION: u8 = 2;
    pub const DELETE_DATABASE: u8 = 3;
    pub const STORE: u8 = 4;
    pub const CREATE_STORE: u8 = 5;
    pub const DELETE_STORE: u8 = 6;
    pub const RENAME_STORE: u8 = 7;
    pub const KEY_GENERATOR: u8 = 8;
    pub const PUT: u8 = 9;
    pub const APPEND: u8 = 10;
    pub const DELETE: u8 = 11;
    pub const CLEAR: u8 = 12;
    pub const INDEX: u8 = 13;
    pub const CREATE_INDEX: u8 = 14;
    pub const DELETE_INDEX: u8 = 15;
    pub const RENAME_INDEX: u8 = 16;
    pub const INDEX_PUT: u8 = 17;
    pub const INDEX_DELETE: u8 = 18;
}

fn put(out: &mut Vec<u8>, b: &[u8]) {
    out.extend((b.len() as u32).to_le_bytes());
    out.extend(b);
}

/// A reader over bytes from the engine or a file: each length is checked
/// against what is left, and against its own bound, before it is taken.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn done(&self) -> bool { self.0.is_empty() }
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }
    fn u64(&mut self) -> Option<u64> {
        let (n, rest) = self.0.split_first_chunk::<8>()?;
        self.0 = rest;
        Some(u64::from_le_bytes(*n))
    }
    fn bytes(&mut self, max: usize) -> Option<Vec<u8>> {
        let (len, rest) = self.0.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        if len > rest.len() || len > max { return None; }
        let (b, rest) = rest.split_at(len);
        self.0 = rest;
        Some(b.to_vec())
    }
    fn name(&mut self) -> Option<Vec<u8>> { self.bytes(NAME_MAX) }
    fn key(&mut self) -> Option<Vec<u8>> { self.bytes(KEY_MAX) }
}

/// Changes, as the engine sends them (a run's parts, after the header) and
/// as a store is loaded and kept.
pub fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let mut out = vec![];
    for op in ops {
        match op {
            Op::Database(n) => { out.push(tag::DATABASE); put(&mut out, n); }
            Op::Version(n, v) => { out.push(tag::VERSION); put(&mut out, n); out.extend(v.to_le_bytes()); }
            Op::DeleteDatabase(n) => { out.push(tag::DELETE_DATABASE); put(&mut out, n); }
            Op::Store(n) => { out.push(tag::STORE); put(&mut out, n); }
            Op::CreateStore(n, a) => { out.push(tag::CREATE_STORE); put(&mut out, n); put(&mut out, a); }
            Op::DeleteStore(n) => { out.push(tag::DELETE_STORE); put(&mut out, n); }
            Op::RenameStore(a, b) => { out.push(tag::RENAME_STORE); put(&mut out, a); put(&mut out, b); }
            Op::KeyGenerator(v) => { out.push(tag::KEY_GENERATOR); out.extend(v.to_le_bytes()); }
            Op::Put(k, v) => { out.push(tag::PUT); put(&mut out, k); put(&mut out, v); }
            Op::Append(k, v) => { out.push(tag::APPEND); put(&mut out, k); put(&mut out, v); }
            Op::Delete(k) => { out.push(tag::DELETE); put(&mut out, k); }
            Op::Clear => out.push(tag::CLEAR),
            Op::Index(n) => { out.push(tag::INDEX); put(&mut out, n); }
            Op::CreateIndex(n, a) => { out.push(tag::CREATE_INDEX); put(&mut out, n); put(&mut out, a); }
            Op::DeleteIndex(n) => { out.push(tag::DELETE_INDEX); put(&mut out, n); }
            Op::RenameIndex(a, b) => { out.push(tag::RENAME_INDEX); put(&mut out, a); put(&mut out, b); }
            Op::IndexPut(k, p) => { out.push(tag::INDEX_PUT); put(&mut out, k); put(&mut out, p); }
            Op::IndexDelete(k, p) => { out.push(tag::INDEX_DELETE); put(&mut out, k); put(&mut out, p); }
        }
    }
    out
}

/// The changes in `bytes`, if every one is in shape and within its bounds:
/// at most `max` bytes, and [`LOAD_OPS_MAX`] changes for a store as loaded
/// (`max` of [`LOAD_MAX`]) or [`RUN_OPS_MAX`] for anything else.
pub fn decode_ops(bytes: &[u8], max: usize) -> Option<Vec<Op>> {
    if bytes.len() > max { return None; }
    let ops_max = if max >= LOAD_MAX { LOAD_OPS_MAX } else { RUN_OPS_MAX };
    let mut r = Reader(bytes);
    let mut ops = vec![];
    while !r.done() {
        if ops.len() >= ops_max { return None; }
        ops.push(match r.byte()? {
            tag::DATABASE => Op::Database(r.name()?),
            tag::VERSION => Op::Version(r.name()?, r.u64()?),
            tag::DELETE_DATABASE => Op::DeleteDatabase(r.name()?),
            tag::STORE => Op::Store(r.name()?),
            tag::CREATE_STORE => Op::CreateStore(r.name()?, r.name()?),
            tag::DELETE_STORE => Op::DeleteStore(r.name()?),
            tag::RENAME_STORE => Op::RenameStore(r.name()?, r.name()?),
            tag::KEY_GENERATOR => Op::KeyGenerator(r.u64()?),
            tag::PUT => Op::Put(r.key()?, r.bytes(QUOTA)?),
            tag::APPEND => Op::Append(r.key()?, r.bytes(QUOTA)?),
            tag::DELETE => Op::Delete(r.key()?),
            tag::CLEAR => Op::Clear,
            tag::INDEX => Op::Index(r.name()?),
            tag::CREATE_INDEX => Op::CreateIndex(r.name()?, r.name()?),
            tag::DELETE_INDEX => Op::DeleteIndex(r.name()?),
            tag::RENAME_INDEX => Op::RenameIndex(r.name()?, r.name()?),
            tag::INDEX_PUT => Op::IndexPut(r.key()?, r.key()?),
            tag::INDEX_DELETE => Op::IndexDelete(r.key()?, r.key()?),
            _ => return None,
        });
    }
    Some(ops)
}

impl Op {
    /// The change as the engine's C ABI carries it: its tag, up to two byte
    /// strings and a number (unused ones empty or 0).
    pub fn fields(&self) -> (u8, &[u8], &[u8], u64) {
        match self {
            Op::Database(a) => (tag::DATABASE, a, &[], 0),
            Op::Version(a, n) => (tag::VERSION, a, &[], *n),
            Op::DeleteDatabase(a) => (tag::DELETE_DATABASE, a, &[], 0),
            Op::Store(a) => (tag::STORE, a, &[], 0),
            Op::CreateStore(a, b) => (tag::CREATE_STORE, a, b, 0),
            Op::DeleteStore(a) => (tag::DELETE_STORE, a, &[], 0),
            Op::RenameStore(a, b) => (tag::RENAME_STORE, a, b, 0),
            Op::KeyGenerator(n) => (tag::KEY_GENERATOR, &[], &[], *n),
            Op::Put(a, b) => (tag::PUT, a, b, 0),
            Op::Append(a, b) => (tag::APPEND, a, b, 0),
            Op::Delete(a) => (tag::DELETE, a, &[], 0),
            Op::Clear => (tag::CLEAR, &[], &[], 0),
            Op::Index(a) => (tag::INDEX, a, &[], 0),
            Op::CreateIndex(a, b) => (tag::CREATE_INDEX, a, b, 0),
            Op::DeleteIndex(a) => (tag::DELETE_INDEX, a, &[], 0),
            Op::RenameIndex(a, b) => (tag::RENAME_INDEX, a, b, 0),
            Op::IndexPut(a, b) => (tag::INDEX_PUT, a, b, 0),
            Op::IndexDelete(a, b) => (tag::INDEX_DELETE, a, b, 0),
        }
    }

    /// The change from [`Op::fields`], if they are in shape.
    pub fn from_fields(t: u8, a: &[u8], b: &[u8], n: u64) -> Option<Op> {
        let mut bytes = vec![t];
        let one = |out: &mut Vec<u8>, x: &[u8]| { out.extend((x.len() as u32).to_le_bytes()); out.extend(x); };
        match t {
            tag::CLEAR => {}
            tag::KEY_GENERATOR => bytes.extend(n.to_le_bytes()),
            tag::VERSION => { one(&mut bytes, a); bytes.extend(n.to_le_bytes()); }
            tag::DATABASE | tag::DELETE_DATABASE | tag::STORE | tag::DELETE_STORE | tag::DELETE | tag::INDEX | tag::DELETE_INDEX => one(&mut bytes, a),
            _ => { one(&mut bytes, a); one(&mut bytes, b); }
        }
        let mut ops = decode_ops(&bytes, RUN_MAX)?;
        (ops.len() == 1).then(|| ops.remove(0))
    }
}

/// The parts of one commit's run, as the engine sends them: each at most
/// [`PART_MAX`] bytes. A value longer than a part goes as a put and
/// appends.
pub fn parts(run: u64, ops: &[Op]) -> Vec<Vec<u8>> {
    let room = PART_MAX - HEADER;
    let mut bodies: Vec<Vec<u8>> = vec![vec![]];
    let push = |bytes: Vec<u8>, bodies: &mut Vec<Vec<u8>>| {
        if bodies.last().is_some_and(|b| b.len() + bytes.len() > room) { bodies.push(vec![]); }
        bodies.last_mut().expect("never empty").extend(bytes);
    };
    for op in ops {
        match op {
            Op::Put(k, v) | Op::Append(k, v) if encode_ops(std::slice::from_ref(op)).len() > room => {
                let chunk = room - 16 - k.len();
                for (i, piece) in v.chunks(chunk).enumerate() {
                    let o = match op {
                        Op::Put(..) if i == 0 => Op::Put(k.clone(), piece.to_vec()),
                        _ => Op::Append(k.clone(), piece.to_vec()),
                    };
                    push(encode_ops(&[o]), &mut bodies);
                }
            }
            _ => push(encode_ops(std::slice::from_ref(op)), &mut bodies),
        }
    }
    let count = bodies.len();
    bodies.into_iter().enumerate().map(|(i, body)| {
        let mut p = Vec::with_capacity(HEADER + body.len());
        p.extend(run.to_le_bytes());
        p.extend((i as u32).to_le_bytes());
        p.push(u8::from(i + 1 == count));
        p.extend(body);
        p
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_change_goes_through_the_c_fields_and_back() {
        let b = |s: &str| s.as_bytes().to_vec();
        let all = vec![Op::Database(b("d")), Op::Version(b("d"), 3), Op::DeleteDatabase(b("d")), Op::Store(b("s")),
                       Op::CreateStore(b("s"), b("about")), Op::DeleteStore(b("s")), Op::RenameStore(b("s"), b("t")),
                       Op::KeyGenerator(42), Op::Put(b("k"), b("v")), Op::Append(b("k"), b("")), Op::Delete(b("k")),
                       Op::Clear, Op::Index(b("i")), Op::CreateIndex(b("i"), b("u")), Op::DeleteIndex(b("i")),
                       Op::RenameIndex(b("i"), b("j")), Op::IndexPut(b("x"), b("k")), Op::IndexDelete(b("x"), b("k"))];
        for op in &all {
            let (t, a, c, n) = op.fields();
            assert_eq!(Op::from_fields(t, a, c, n).as_ref(), Some(op));
        }
        assert_eq!(decode_ops(&encode_ops(&all), RUN_MAX), Some(all));
        assert_eq!(Op::from_fields(0, b"", b"", 0), None);
        let clears = vec![tag::CLEAR; RUN_OPS_MAX + 1];
        assert_eq!(decode_ops(&clears, RUN_MAX), None, "a run of more changes than its bound");
        assert_eq!(decode_ops(&clears[1..], RUN_MAX).map(|o| o.len()), Some(RUN_OPS_MAX));
        assert_eq!(Op::from_fields(tag::DATABASE, &vec![0; NAME_MAX + 1], b"", 0), None);
    }

    #[test]
    fn a_long_value_goes_as_a_put_and_appends_in_parts_within_the_bound() {
        let v: Vec<u8> = (0..3 * PART_MAX).map(|i| i as u8).collect();
        let p = parts(5, &[Op::Store(b"s".to_vec()), Op::Put(b"k".to_vec(), v.clone()), Op::Append(b"k".to_vec(), vec![1; PART_MAX])]);
        assert!(p.iter().all(|p| p.len() <= PART_MAX));
        let mut ops = vec![];
        for (i, part) in p.iter().enumerate() {
            assert_eq!(u64::from_le_bytes(part[0..8].try_into().unwrap()), 5);
            assert_eq!(u32::from_le_bytes(part[8..12].try_into().unwrap()) as usize, i);
            assert_eq!(part[12] == 1, i + 1 == p.len());
            ops.extend(decode_ops(&part[HEADER..], PART_MAX).unwrap());
        }
        assert!(matches!(&ops[1], Op::Put(..)) && ops[2..].iter().all(|o| matches!(o, Op::Append(..))));
        let mut whole: Vec<u8> = vec![];
        for o in &ops[1..] { if let Op::Put(_, x) | Op::Append(_, x) = o { whole.extend(x) } }
        assert_eq!(whole.len(), v.len() + PART_MAX);
        assert_eq!(&whole[..v.len()], &v[..]);
        assert_eq!(parts(1, &[]).len(), 1, "an empty commit is one last part");
    }
}
