//! A live page's IndexedDB (navigator-islands.md I6.7, D15 as decided
//! 2026-11-25): navigatord keeps it as it keeps `localStorage`
//! ([`crate::live_storage`]), one store per (jar, site). The engine loads
//! its site's store once, from [`limen::web_island::LIVE_IDB`], and at each
//! transaction's commit sends what the transaction changed there, as a run
//! of numbered parts. The run is applied when its last part arrives, all of
//! it or none: a refusal aborts the transaction in the engine with
//! `QuotaExceededError`.
//!
//! navigatord does not read IndexedDB keys or values: names, keys, values
//! and a store's or index's description (key path, auto-increment, unique,
//! multi-entry) are the engine's bytes. It keeps their structure (databases,
//! object stores, records, indexes and index records) so that a change to
//! something that isn't there is refused, and it holds the whole to
//! [`QUOTA`].
//!
//! A recorded trail's store is two files under navigatord's store
//! directory, on the persona's encrypted volume: `<site>.idb`, the image,
//! and `<site>.idb-log`, the commits since, each appended and synced before
//! the engine is answered. Both carry a generation; a log whose generation
//! is not the image's is already in it. An Unrecorded trail's store is in
//! memory only, and all of those together are held to [`MEMORY_MAX`].
//!
//! Everything from the engine is hostile: every length is bounded before it
//! is read.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;

pub use crate::live_storage::Key;
pub use limen::web_island::idb::*;
use limen::web_island::idb::tag;

/// What every Unrecorded store together may hold.
pub const MEMORY_MAX: usize = 4 * QUOTA;
/// What runs not yet finished hold, across every site together.
pub const PENDING_MAX: usize = 2 * RUN_MAX;
/// What file-backed stores stay loaded together; past it, the others are
/// dropped and read again when next asked for.
pub const LOADED_MAX: usize = 3 * QUOTA;
/// What a database, store or index costs beyond its bytes.
const THING: usize = 64;
/// What a record or index record costs beyond its bytes.
const ENTRY: usize = 16;

#[derive(Clone, Debug, Default, PartialEq)]
struct Index {
    about: Vec<u8>,
    records: BTreeSet<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Store {
    about: Vec<u8>,
    key_generator: u64,
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    indexes: BTreeMap<Vec<u8>, Index>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Database {
    version: u64,
    stores: BTreeMap<Vec<u8>, Store>,
}

type Databases = BTreeMap<Vec<u8>, Database>;

fn index_size(name: &[u8], i: &Index) -> usize {
    THING + name.len() + i.about.len() + i.records.iter().map(|(k, p)| ENTRY + k.len() + p.len()).sum::<usize>()
}

fn store_size(name: &[u8], s: &Store) -> usize {
    THING + name.len() + s.about.len()
        + s.records.iter().map(|(k, v)| ENTRY + k.len() + v.len()).sum::<usize>()
        + s.indexes.iter().map(|(n, i)| index_size(n, i)).sum::<usize>()
}

fn database_size(name: &[u8], d: &Database) -> usize {
    THING + name.len() + d.stores.iter().map(|(n, s)| store_size(n, s)).sum::<usize>()
}

/// What a store counts against [`QUOTA`].
fn size(dbs: &Databases) -> usize {
    dbs.iter().map(|(n, d)| database_size(n, d)).sum()
}

/// A store as changes from empty: what the engine loads, and the image.
fn encode(dbs: &Databases) -> Vec<u8> {
    let mut ops = vec![];
    for (dn, d) in dbs {
        ops.push(Op::Version(dn.clone(), d.version));
        for (sn, s) in &d.stores {
            ops.push(Op::CreateStore(sn.clone(), s.about.clone()));
            if s.key_generator != 0 { ops.push(Op::KeyGenerator(s.key_generator)); }
            ops.extend(s.records.iter().map(|(k, v)| Op::Put(k.clone(), v.clone())));
            for (iname, i) in &s.indexes {
                ops.push(Op::CreateIndex(iname.clone(), i.about.clone()));
                ops.extend(i.records.iter().map(|(k, p)| Op::IndexPut(k.clone(), p.clone())));
            }
        }
    }
    encode_ops(&ops)
}

/// How to put back one thing a change replaced, for a run that fails.
enum Undo {
    Database(Vec<u8>, Option<Database>),
    Store(Vec<u8>, Vec<u8>, Option<Store>),
    Index(Vec<u8>, Vec<u8>, Vec<u8>, Option<Index>),
    /// A rename, put back by moving the store (or index) home again: no copy.
    RenameStore(Vec<u8>, Vec<u8>, Vec<u8>),
    RenameIndex(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
    /// A clear: the records it took.
    Records(Vec<u8>, Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>),
    Record(Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>),
    /// An append: the value's length before it.
    Truncate(Vec<u8>, Vec<u8>, Vec<u8>, usize),
    IndexRecord(Vec<u8>, Vec<u8>, Vec<u8>, (Vec<u8>, Vec<u8>), bool),
    KeyGenerator(Vec<u8>, Vec<u8>, u64),
    Version(Vec<u8>, u64),
}

/// The selection the changes act on.
#[derive(Default)]
struct At {
    db: Option<Vec<u8>>,
    store: Option<Vec<u8>>,
    index: Option<Vec<u8>>,
}

fn apply_ops(dbs: &mut Databases, ops: Vec<Op>, undo: &mut Vec<Undo>) -> Result<(), String> {
    let mut at = At::default();
    for op in ops {
        apply_op(dbs, op, &mut at, undo)?;
    }
    Ok(())
}

fn apply_op(dbs: &mut Databases, op: Op, at: &mut At, undo: &mut Vec<Undo>) -> Result<(), String> {
    let missing = |what: &str| Err::<(), String>(format!("a change to a {what} that isn't there"));
    let db_name = |at: &At| at.db.clone().ok_or_else(|| "no database selected".to_string());
    let store_names = |at: &At| -> Result<(Vec<u8>, Vec<u8>), String> {
        Ok((at.db.clone().ok_or("no database selected")?, at.store.clone().ok_or("no object store selected")?))
    };
    match op {
        Op::Database(n) => {
            if !dbs.contains_key(&n) { return missing("database"); }
            *at = At { db: Some(n), ..At::default() };
        }
        Op::Version(n, v) => {
            match dbs.get_mut(&n) {
                Some(d) => { undo.push(Undo::Version(n.clone(), d.version)); d.version = v; }
                None => {
                    undo.push(Undo::Database(n.clone(), None));
                    dbs.insert(n.clone(), Database { version: v, stores: BTreeMap::new() });
                }
            }
            *at = At { db: Some(n), ..At::default() };
        }
        Op::DeleteDatabase(n) => {
            let old = dbs.remove(&n);
            if old.is_none() { return missing("database"); }
            undo.push(Undo::Database(n.clone(), old));
            if at.db.as_ref() == Some(&n) { *at = At::default(); }
        }
        Op::Store(n) => {
            let db = db_name(at)?;
            if !dbs.get(&db).is_some_and(|d| d.stores.contains_key(&n)) { return missing("object store"); }
            at.store = Some(n);
            at.index = None;
        }
        Op::CreateStore(n, about) => {
            let db = db_name(at)?;
            let d = dbs.get_mut(&db).ok_or("the selected database went")?;
            let old = d.stores.insert(n.clone(), Store { about, ..Store::default() });
            undo.push(Undo::Store(db, n.clone(), old));
            at.store = Some(n);
            at.index = None;
        }
        Op::DeleteStore(n) => {
            let db = db_name(at)?;
            let d = dbs.get_mut(&db).ok_or("the selected database went")?;
            let old = d.stores.remove(&n);
            if old.is_none() { return missing("object store"); }
            undo.push(Undo::Store(db, n.clone(), old));
            if at.store.as_ref() == Some(&n) { at.store = None; at.index = None; }
        }
        Op::RenameStore(from, to) => {
            let db = db_name(at)?;
            let d = dbs.get_mut(&db).ok_or("the selected database went")?;
            if d.stores.contains_key(&to) { return Err("a rename onto a store that is there".into()); }
            let s = d.stores.remove(&from).ok_or("a rename of an object store that isn't there")?;
            undo.push(Undo::RenameStore(db, from.clone(), to.clone()));
            d.stores.insert(to.clone(), s);
            if at.store.as_ref() == Some(&from) { at.store = Some(to); }
        }
        Op::KeyGenerator(v) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            undo.push(Undo::KeyGenerator(db, sn, s.key_generator));
            s.key_generator = v;
        }
        Op::Put(k, v) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            let old = s.records.insert(k.clone(), v);
            undo.push(Undo::Record(db, sn, k, old));
        }
        Op::Append(k, more) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            let v = s.records.get_mut(&k).ok_or("more of a value that was never put")?;
            if v.len() + more.len() > QUOTA { return Err("over the site's IndexedDB quota".into()); }
            undo.push(Undo::Truncate(db, sn, k, v.len()));
            v.extend(more);
        }
        Op::Delete(k) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            if let Some(old) = s.records.remove(&k) { undo.push(Undo::Record(db, sn, k, Some(old))); }
        }
        Op::Clear => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            let records = std::mem::take(&mut s.records);
            undo.push(Undo::Records(db, sn, records));
        }
        Op::Index(n) => {
            let (db, sn) = store_names(at)?;
            if !dbs.get(&db).and_then(|d| d.stores.get(&sn)).is_some_and(|s| s.indexes.contains_key(&n)) { return missing("index"); }
            at.index = Some(n);
        }
        Op::CreateIndex(n, about) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            let old = s.indexes.insert(n.clone(), Index { about, records: BTreeSet::new() });
            undo.push(Undo::Index(db, sn, n.clone(), old));
            at.index = Some(n);
        }
        Op::DeleteIndex(n) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            let old = s.indexes.remove(&n);
            if old.is_none() { return missing("index"); }
            undo.push(Undo::Index(db, sn, n.clone(), old));
            if at.index.as_ref() == Some(&n) { at.index = None; }
        }
        Op::RenameIndex(from, to) => {
            let (db, sn) = store_names(at)?;
            let s = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).ok_or("the selected store went")?;
            if s.indexes.contains_key(&to) { return Err("a rename onto an index that is there".into()); }
            let i = s.indexes.remove(&from).ok_or("a rename of an index that isn't there")?;
            undo.push(Undo::RenameIndex(db, sn, from.clone(), to.clone()));
            s.indexes.insert(to.clone(), i);
            if at.index.as_ref() == Some(&from) { at.index = Some(to); }
        }
        Op::IndexPut(k, p) => {
            let (db, sn) = store_names(at)?;
            let n = at.index.clone().ok_or("no index selected")?;
            let i = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).and_then(|s| s.indexes.get_mut(&n)).ok_or("the selected index went")?;
            let added = i.records.insert((k.clone(), p.clone()));
            if added { undo.push(Undo::IndexRecord(db, sn, n, (k, p), false)); }
        }
        Op::IndexDelete(k, p) => {
            let (db, sn) = store_names(at)?;
            let n = at.index.clone().ok_or("no index selected")?;
            let i = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).and_then(|s| s.indexes.get_mut(&n)).ok_or("the selected index went")?;
            let pair = (k, p);
            if i.records.remove(&pair) { undo.push(Undo::IndexRecord(db, sn, n, pair, true)); }
        }
    }
    Ok(())
}

/// Put back, newest first, what a failed run changed.
fn revert(dbs: &mut Databases, undo: Vec<Undo>) {
    fn set<K: Ord, V>(map: &mut BTreeMap<K, V>, k: K, v: Option<V>) {
        match v { Some(v) => { map.insert(k, v); } None => { map.remove(&k); } }
    }
    for u in undo.into_iter().rev() {
        match u {
            Undo::Database(n, old) => set(dbs, n, old),
            Undo::Version(n, v) => if let Some(d) = dbs.get_mut(&n) { d.version = v },
            Undo::Store(db, n, old) => if let Some(d) = dbs.get_mut(&db) { set(&mut d.stores, n, old) },
            Undo::RenameStore(db, from, to) => if let Some(d) = dbs.get_mut(&db) {
                if let Some(s) = d.stores.remove(&to) { d.stores.insert(from, s); }
            },
            Undo::RenameIndex(db, sn, from, to) => if let Some(s) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)) {
                if let Some(i) = s.indexes.remove(&to) { s.indexes.insert(from, i); }
            },
            Undo::Records(db, sn, records) => if let Some(s) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)) { s.records = records },
            Undo::Index(db, sn, n, old) => if let Some(s) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)) { set(&mut s.indexes, n, old) },
            Undo::Record(db, sn, k, old) => if let Some(s) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)) { set(&mut s.records, k, old) },
            Undo::Truncate(db, sn, k, len) => if let Some(v) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).and_then(|s| s.records.get_mut(&k)) { v.truncate(len) },
            Undo::KeyGenerator(db, sn, v) => if let Some(s) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)) { s.key_generator = v },
            Undo::IndexRecord(db, sn, n, pair, was) => if let Some(i) = dbs.get_mut(&db).and_then(|d| d.stores.get_mut(&sn)).and_then(|s| s.indexes.get_mut(&n)) {
                if was { i.records.insert(pair); } else { i.records.remove(&pair); }
            },
        }
    }
}

/// A run being received: its number, the next part's, and its changes.
struct Pending {
    run: u64,
    next: u32,
    ops: Vec<u8>,
}

struct Site {
    dbs: Databases,
    size: usize,
    /// The generation of the image and log on disk.
    generation: u64,
    /// The log's length on disk, and the image's.
    log_len: usize,
    image_len: usize,
    pending: Option<Pending>,
}

const IMAGE_MAGIC: &[u8; 8] = b"AIDBimg1";
const LOG_MAGIC: &[u8; 8] = b"AIDBlog1";
/// A log is folded into a new image once it is longer than the image and
/// than this.
const LOG_FOLD: usize = 1024 * 1024;

/// Which of a site's stores: its IndexedDB, or its Cache API's (I6.7
/// second part), kept alike and held to one quota between them, as the
/// Storage standard gives an origin one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind { Idb, Cache }

impl Kind {
    fn other(self) -> Kind { match self { Kind::Idb => Kind::Cache, Kind::Cache => Kind::Idb } }
    fn ext(self) -> &'static str { match self { Kind::Idb => "idb", Kind::Cache => "cache" } }
}

type Slot = (Key, Kind);

/// Every live page's IndexedDB and Cache API stores, for this navigatord
/// (one persona): the bounds on what is loaded, pending and in memory are
/// across both.
pub struct Stores {
    /// `<store dir>/live-idb`, for the persona's own jar.
    dir: Option<PathBuf>,
    open: HashMap<Slot, Site>,
}

impl Stores {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Stores { dir, open: HashMap::new() }
    }

    fn files(&self, (key, kind): &Slot) -> Option<(PathBuf, PathBuf)> {
        match key.jar {
            None => self.dir.as_ref().map(|d| (d.join(format!("{}.{}", key.site, kind.ext())), d.join(format!("{}.{}-log", key.site, kind.ext())))),
            Some(_) => None,
        }
    }

    /// The site as it is on disk: the image, then the log of its
    /// generation. A log cut short at its end loses only the commit that
    /// was never answered; anything else out of shape starts the site
    /// empty, logged.
    fn read(&self, slot: &Slot) -> Site {
        let key = &slot.0;
        let empty = || Site { dbs: Databases::new(), size: 0, generation: 0, log_len: 0, image_len: 0, pending: None };
        let Some((image, log)) = self.files(slot) else { return empty() };
        let read = |f: &PathBuf| match std::fs::File::open(f) {
            Ok(file) => {
                use std::io::Read;
                let mut b = vec![];
                match file.take(LOAD_MAX as u64 + 17).read_to_end(&mut b) {
                    Ok(_) => Ok(Some(b)),
                    Err(e) => Err(format!("{}: {e}", f.display())),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", f.display())),
        };
        let loaded = (|| -> Result<Site, String> {
            let mut site = empty();
            if let Some(b) = read(&image)? {
                let (head, body) = b.split_at_checked(16).ok_or("the image is cut short")?;
                if &head[..8] != IMAGE_MAGIC { return Err("the image is not one".into()); }
                site.generation = u64::from_le_bytes(head[8..16].try_into().map_err(|_| "no generation")?);
                let ops = decode_ops(body, LOAD_MAX).ok_or("the image is out of shape")?;
                apply_ops(&mut site.dbs, ops, &mut vec![])?;
                site.image_len = b.len();
            }
            if let Some(b) = read(&log)? {
                let (head, mut rest) = b.split_at_checked(16).ok_or("the log is cut short")?;
                if &head[..8] != LOG_MAGIC { return Err("the log is not one".into()); }
                let generation = u64::from_le_bytes(head[8..16].try_into().map_err(|_| "no generation")?);
                if generation == site.generation {
                    site.log_len = 16;
                    // Replay up to the first commit cut short or out of
                    // shape (a write torn by a crash, a damaged block):
                    // the image and the commits before it are kept.
                    while let Some((len, more)) = rest.split_first_chunk::<4>() {
                        let len = u32::from_le_bytes(*len) as usize;
                        let Some((entry, more)) = more.split_at_checked(len) else { break };
                        let Some(ops) = decode_ops(entry, RUN_MAX) else { break };
                        let mut undo = vec![];
                        if let Err(why) = apply_ops(&mut site.dbs, ops, &mut undo) {
                            revert(&mut site.dbs, undo);
                            eprintln!("navigatord: {}'s IndexedDB log: {why}", key.site);
                            break;
                        }
                        site.log_len += 4 + len;
                        rest = more;
                    }
                    if !rest.is_empty() {
                        eprintln!("navigatord: {}'s IndexedDB log ends in {} bytes not kept; the commits before them are", key.site, rest.len());
                        // Never append after them: the next commit writes
                        // the next generation whole.
                        site.log_len = usize::MAX / 2;
                    }
                }
            }
            site.size = size(&site.dbs);
            if site.size > QUOTA { return Err("over the quota".into()); }
            Ok(site)
        })();
        match loaded {
            Ok(site) => site,
            Err(why) => {
                eprintln!("navigatord: {}'s {:?} store: {why}; the site starts empty", key.site, slot.1);
                empty()
            }
        }
    }

    fn site(&mut self, slot: &Slot) -> &mut Site {
        if !self.open.contains_key(slot) {
            let site = self.read(slot);
            self.open.insert(slot.clone(), site);
        }
        self.open.get_mut(slot).expect("inserted above")
    }

    /// Drop file-backed sites other than `key` while those loaded hold more
    /// than [`LOADED_MAX`]; they are read again when next asked for.
    fn trim(&mut self, slot: &Slot) {
        let mut loaded: usize = self.open.iter().filter(|(k, _)| k.0.jar.is_none()).map(|(_, s)| s.size).sum();
        if loaded <= LOADED_MAX { return; }
        let others: Vec<Slot> = self.open.iter().filter(|(k, s)| k.0.jar.is_none() && *k != slot && s.pending.is_none()).map(|(k, _)| k.clone()).collect();
        for k in others {
            if loaded <= LOADED_MAX { break; }
            if let Some(s) = self.open.remove(&k) { loaded -= s.size; }
        }
    }

    /// The site's IndexedDB, as the engine loads it.
    pub fn load(&mut self, key: &Key) -> Vec<u8> { self.load_kind(key, Kind::Idb) }

    /// What the site's IndexedDB and Cache API stores hold together, in
    /// the bytes their one quota counts.
    pub fn used(&mut self, key: &Key) -> usize {
        let mut n = 0;
        for kind in [Kind::Idb, Kind::Cache] {
            let slot = (key.clone(), kind);
            n += self.site(&slot).size;
            self.trim(&slot);
        }
        n
    }

    /// The site's store of `kind`, as the engine loads it.
    pub fn load_kind(&mut self, key: &Key, kind: Kind) -> Vec<u8> {
        let slot = (key.clone(), kind);
        let out = encode(&self.site(&slot).dbs);
        self.trim(&slot);
        out
    }

    /// One part of a commit to the site's IndexedDB.
    pub fn apply(&mut self, key: &Key, part: &[u8]) -> Result<(), String> { self.apply_kind(key, Kind::Idb, part) }

    /// One part of a commit's run. A part 0 starts a run (and drops any
    /// run not finished); each later part must be the next of that run.
    /// The last part applies the whole run, or none of it, and a recorded
    /// site's commit is on disk before this returns.
    pub fn apply_kind(&mut self, key: &Key, kind: Kind, part: &[u8]) -> Result<(), String> {
        let slot = (key.clone(), kind);
        if part.len() > PART_MAX { return Err("a part over its bound".into()); }
        let (head, body) = part.split_at_checked(HEADER).ok_or("a part with no header")?;
        let run = u64::from_le_bytes(head[0..8].try_into().map_err(|_| "no run")?);
        let n = u32::from_le_bytes(head[8..12].try_into().map_err(|_| "no part")?);
        let last = match head[12] { 0 => false, 1 => true, _ => return Err("a part out of shape".into()) };
        let memory_used: usize = self.open.iter().filter(|(k, _)| k.0.jar.is_some() && *k != &slot).map(|(_, s)| s.size).sum();
        let pending_elsewhere: usize = self.open.iter().filter(|(k, _)| *k != &slot)
            .filter_map(|(_, s)| s.pending.as_ref()).map(|p| p.ops.len()).sum();
        let files = self.files(&slot);
        // The site's other store counts against the same quota.
        let beside = if last { self.site(&(key.clone(), kind.other())).size } else { 0 };
        let site = self.site(&slot);
        let mut pending = match site.pending.take() {
            _ if n == 0 => Pending { run, next: 0, ops: vec![] },
            Some(p) if p.run == run && p.next == n => p,
            _ => return Err("a part out of order".into()),
        };
        if pending.ops.len() + body.len() > RUN_MAX { return Err("a commit over its bound".into()); }
        if pending_elsewhere + pending.ops.len() + body.len() > PENDING_MAX { return Err("too many commits being received at once".into()); }
        pending.ops.extend_from_slice(body);
        pending.next = n.checked_add(1).ok_or("too many parts")?;
        if !last {
            site.pending = Some(pending);
            return Ok(());
        }
        let ops = decode_ops(&pending.ops, RUN_MAX).ok_or("the changes are out of shape")?;
        let mut undo = vec![];
        let applied = apply_ops(&mut site.dbs, ops, &mut undo).and_then(|()| {
            let next = size(&site.dbs);
            if next + beside > QUOTA { return Err("over the site's storage quota".to_string()); }
            if files.is_none() && memory_used + next > MEMORY_MAX { return Err("over what Unrecorded trails may keep".to_string()); }
            if let Some((image, log)) = &files {
                append(site, image, log, &pending.ops)?;
            }
            site.size = next;
            Ok(())
        });
        if applied.is_err() { revert(&mut site.dbs, undo); }
        applied?;
        self.trim(&slot);
        Ok(())
    }

    /// The person forgot the site in that jar: both its stores go too.
    pub fn forget(&mut self, key: &Key) {
        for kind in [Kind::Idb, Kind::Cache] {
            let slot = (key.clone(), kind);
            self.open.remove(&slot);
            if let Some((image, log)) = self.files(&slot) {
                for f in [image, log] {
                    if let Err(e) = std::fs::remove_file(&f) {
                        if e.kind() != std::io::ErrorKind::NotFound { eprintln!("navigatord: {}: {e}", f.display()); }
                    }
                }
            }
        }
    }

    /// An Unrecorded jar ended: every store in it goes.
    pub fn forget_jar(&mut self, jar: u64) {
        self.open.retain(|k, _| k.0.jar != Some(jar));
    }
}

/// Keep one commit: appended to the log and synced, or, once the log has
/// grown past the image, the whole store written as the next generation's
/// image with an empty log beside it.
fn append(site: &mut Site, image: &PathBuf, log: &PathBuf, ops: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    if site.log_len + 4 + ops.len() > LOG_FOLD.max(site.image_len) {
        let generation = site.generation + 1;
        let mut img = IMAGE_MAGIC.to_vec();
        img.extend(generation.to_le_bytes());
        img.extend(encode(&site.dbs));
        crate::live_storage::write(image, &img)?;
        // The image is in place and holds this commit, and the old log is
        // of an older generation, so the commit is kept whatever follows.
        site.generation = generation;
        site.image_len = img.len();
        let mut head = LOG_MAGIC.to_vec();
        head.extend(generation.to_le_bytes());
        site.log_len = match crate::live_storage::write(log, &head) {
            Ok(()) => head.len(),
            Err(e) => {
                eprintln!("navigatord: {e}; the next commit writes the image again");
                usize::MAX / 2
            }
        };
        return Ok(());
    }
    if site.log_len == 0 {
        let mut head = LOG_MAGIC.to_vec();
        head.extend(site.generation.to_le_bytes());
        crate::live_storage::write(log, &head)?;
        site.log_len = head.len();
    }
    let mut entry = Vec::with_capacity(4 + ops.len());
    entry.extend((ops.len() as u32).to_le_bytes());
    entry.extend(ops);
    // Ok, or the error and whether the log was cut back to before it.
    let result: Result<(), (std::io::Error, bool)> = match std::fs::OpenOptions::new().append(true).custom_flags(libc::O_NOFOLLOW).open(log) {
        Err(e) => Err((e, true)),
        Ok(mut f) => match f.write_all(&entry).and_then(|()| f.sync_data()) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Take back what may have reached the log, so the failed
                // commit doesn't come back on the next read.
                let cut = u64::try_from(site.log_len).is_ok_and(|len| f.set_len(len).and_then(|()| f.sync_data()).is_ok());
                Err((e, cut))
            }
        },
    };
    match result {
        Ok(()) => { site.log_len += entry.len(); Ok(()) }
        Err((e, cut)) => {
            // Not cut back: the log may end in some of this commit. The next
            // read drops an end cut short or out of shape, and the next
            // commit writes the next generation whole without it. Only a
            // crash before that next commit, with the whole of this one in
            // the log, brings back a commit the engine was told failed.
            if !cut { site.log_len = usize::MAX / 2; }
            Err(format!("{}: {e}", log.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(jar: Option<u64>) -> Key { Key::of(jar, "https://app.example/app.html").unwrap() }
    fn b(s: &str) -> Vec<u8> { s.as_bytes().to_vec() }

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("navigatord-idb-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            Dir(d)
        }
    }
    impl Drop for Dir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    fn commit(s: &mut Stores, k: &Key, run: u64, ops: &[Op]) -> Result<(), String> {
        for p in parts(run, ops) { s.apply(k, &p)?; }
        Ok(())
    }

    fn schema() -> Vec<Op> {
        vec![
            Op::Version(b("mail"), 1),
            Op::CreateStore(b("messages"), b("keyPath=id")),
            Op::CreateIndex(b("by-date"), b("keyPath=date")),
        ]
    }

    fn visit(n: u64) -> Vec<Op> {
        vec![Op::Database(b("mail")), Op::Store(b("messages")), Op::Put(b("visits"), n.to_le_bytes().to_vec()),
             Op::Index(b("by-date")), Op::IndexPut(n.to_le_bytes().to_vec(), b("visits"))]
    }

    fn visits(s: &mut Stores, k: &Key) -> Option<u64> {
        let mut dbs = Databases::new();
        apply_ops(&mut dbs, decode_ops(&s.load(k), LOAD_MAX).unwrap(), &mut vec![]).unwrap();
        let v = dbs.get(&b("mail"))?.stores.get(&b("messages"))?.records.get(&b("visits"))?;
        Some(u64::from_le_bytes(v[..8].try_into().unwrap()))
    }

    #[test]
    fn a_site_keeps_its_databases_across_runs_and_navigatords_through_log_and_image() {
        let d = Dir::new("keep");
        let k = key(None);
        let mut s = Stores::new(Some(d.0.clone()));
        commit(&mut s, &k, 1, &schema()).unwrap();
        for n in 1..=3 { commit(&mut s, &k, 10 + n, &visit(n)).unwrap(); }
        assert_eq!(visits(&mut s, &k), Some(3));
        assert!(d.0.join("app.example.idb-log").exists(), "commits go to the log");
        // A fresh navigatord reads the log back.
        let mut again = Stores::new(Some(d.0.clone()));
        assert_eq!(visits(&mut again, &k), Some(3));
        assert_eq!(again.load(&k), s.load(&k));
        // Past the fold, the image takes the log in and the log starts again.
        let big = vec![7u8; 600 * 1024];
        for n in 0..3u64 {
            commit(&mut again, &k, 100 + n, &[Op::Database(b("mail")), Op::Store(b("messages")), Op::Put(n.to_le_bytes().to_vec(), big.clone())]).unwrap();
        }
        let image = std::fs::read(d.0.join("app.example.idb")).unwrap();
        assert_eq!(&image[..8], IMAGE_MAGIC);
        let mut third = Stores::new(Some(d.0.clone()));
        assert_eq!(third.load(&k), again.load(&k), "image and log read back as one");
        use std::os::unix::fs::MetadataExt;
        for f in ["app.example.idb", "app.example.idb-log"] {
            assert_eq!(std::fs::metadata(d.0.join(f)).unwrap().mode() & 0o777, 0o600, "{f}");
        }
        third.forget(&k);
        assert!(!d.0.join("app.example.idb").exists() && !d.0.join("app.example.idb-log").exists());
        assert_eq!(Stores::new(Some(d.0.clone())).load(&k), b"");
    }

    #[test]
    fn a_log_cut_short_loses_only_its_last_commit_and_an_old_generation_log_is_skipped() {
        let d = Dir::new("cut");
        let k = key(None);
        let mut s = Stores::new(Some(d.0.clone()));
        commit(&mut s, &k, 1, &schema()).unwrap();
        commit(&mut s, &k, 2, &visit(1)).unwrap();
        commit(&mut s, &k, 3, &visit(2)).unwrap();
        let log = d.0.join("app.example.idb-log");
        let bytes = std::fs::read(&log).unwrap();
        std::fs::write(&log, &bytes[..bytes.len() - 3]).unwrap();
        assert_eq!(visits(&mut Stores::new(Some(d.0.clone())), &k), Some(1));
        // A log of an older generation than the image is already in it.
        let mut img = IMAGE_MAGIC.to_vec();
        img.extend(1u64.to_le_bytes());
        img.extend(encode_ops(&schema()));
        std::fs::write(d.0.join("app.example.idb"), img).unwrap();
        assert_eq!(visits(&mut Stores::new(Some(d.0.clone())), &k), None, "the generation-0 log is not replayed onto generation 1");
        // Garbage starts the site empty.
        std::fs::write(d.0.join("app.example.idb"), b"AIDBimg1\0\0\0\0\0\0\0\0\xff\xff").unwrap();
        assert_eq!(Stores::new(Some(d.0.clone())).load(&k), b"");
    }

    #[test]
    fn renames_are_put_back_by_moving_not_copying() {
        let mut s = Stores::new(None);
        let k = key(Some(1));
        let mut ops = schema();
        ops.extend((0..64u32).map(|i| Op::Put(i.to_le_bytes().to_vec(), vec![1; 32 * 1024])));
        commit(&mut s, &k, 1, &ops).unwrap();
        let before = s.load(&k);
        // 2 MiB of records renamed back and forth: a copy per rename would
        // be tens of gigabytes.
        let mut ops = vec![Op::Database(b("mail")), Op::Store(b("messages")), Op::Index(b("by-date"))];
        for _ in 0..20_000 {
            ops.extend([Op::RenameStore(b("messages"), b("m2")), Op::RenameStore(b("m2"), b("messages")),
                        Op::RenameIndex(b("by-date"), b("d2")), Op::RenameIndex(b("d2"), b("by-date")), Op::Clear]);
        }
        ops.push(Op::Store(b("gone")));
        assert!(commit(&mut s, &k, 2, &ops).is_err());
        assert_eq!(s.load(&k), before, "every rename and clear put back");
    }

    #[test]
    fn runs_being_received_are_held_to_a_bound_across_sites() {
        let mut s = Stores::new(None);
        let body = vec![0u8; PART_MAX - HEADER];
        let part = |run: u64, n: u32| { let mut p = run.to_le_bytes().to_vec(); p.extend(n.to_le_bytes()); p.push(0); p.extend(&body); p };
        let per_run = RUN_MAX / body.len();
        for jar in [1, 2] {
            for n in 0..per_run as u32 { s.apply(&key(Some(jar)), &part(jar, n)).unwrap(); }
            // Topped up to the run's bound exactly.
            let mut last = part(jar, per_run as u32);
            last.truncate(HEADER + RUN_MAX - per_run * body.len());
            s.apply(&key(Some(jar)), &last).unwrap();
        }
        assert!(s.apply(&key(Some(3)), &part(3, 0)).is_err(), "a third run past the bound");
        s.forget_jar(1);
        s.apply(&key(Some(3)), &part(3, 0)).unwrap();
    }

    #[test]
    fn a_log_commit_out_of_shape_keeps_the_image_and_the_commits_before_it() {
        let d = Dir::new("bad");
        let k = key(None);
        let mut s = Stores::new(Some(d.0.clone()));
        commit(&mut s, &k, 1, &schema()).unwrap();
        commit(&mut s, &k, 2, &visit(1)).unwrap();
        let log = d.0.join("app.example.idb-log");
        // A whole entry that names a store that isn't there, then a good one.
        let mut bytes = std::fs::read(&log).unwrap();
        for ops in [vec![Op::Database(b("mail")), Op::Put(b("x"), b("y")), Op::Store(b("nope"))], visit(9)] {
            let e = encode_ops(&ops);
            bytes.extend((e.len() as u32).to_le_bytes());
            bytes.extend(e);
        }
        std::fs::write(&log, &bytes).unwrap();
        let mut again = Stores::new(Some(d.0.clone()));
        assert_eq!(visits(&mut again, &k), Some(1), "the commits before it, and none after");
        // The next commit is not appended after the bad entry, where a read
        // would never reach it.
        commit(&mut again, &k, 3, &visit(2)).unwrap();
        assert_eq!(visits(&mut Stores::new(Some(d.0.clone())), &k), Some(2));
    }

    #[test]
    fn a_sites_cache_api_store_is_apart_but_shares_its_quota_and_its_forgetting() {
        let d = Dir::new("kinds");
        let k = key(None);
        let mut s = Stores::new(Some(d.0.clone()));
        let fill = |name: &str| parts(0, &[Op::Version(b(name), 0), Op::CreateStore(b("entries"), vec![]), Op::Put(b("1"), vec![7; 30 * 1024 * 1024])]);
        for p in fill("idb") { s.apply_kind(&k, Kind::Idb, &p).unwrap(); }
        assert_eq!(s.load_kind(&k, Kind::Cache), b"", "apart");
        // 30 MiB of IndexedDB and 30 of cache would be past the site's 50.
        let mut refused = false;
        for p in fill("cache") { if s.apply_kind(&k, Kind::Cache, &p).is_err() { refused = true; break; } }
        assert!(refused, "one quota for both");
        assert_eq!(s.load_kind(&k, Kind::Cache), b"");
        for p in parts(1, &[Op::Version(b("small"), 0)]) { s.apply_kind(&k, Kind::Cache, &p).unwrap(); }
        assert!(d.0.join("app.example.idb-log").exists() && d.0.join("app.example.cache-log").exists());
        s.forget(&k);
        assert!(!d.0.join("app.example.idb-log").exists() && !d.0.join("app.example.cache-log").exists());
        let mut again = Stores::new(Some(d.0.clone()));
        assert_eq!((again.load_kind(&k, Kind::Idb), again.load_kind(&k, Kind::Cache)), (vec![], vec![]));
    }

    #[test]
    fn a_part_is_what_nuntius_carries_in_one_post() {
        assert_eq!(PART_MAX, nuntius::BODY_MAX);
    }

    #[test]
    fn a_failed_commit_changes_nothing() {
        let mut s = Stores::new(None);
        let k = key(Some(4));
        commit(&mut s, &k, 1, &schema()).unwrap();
        commit(&mut s, &k, 2, &visit(1)).unwrap();
        let before = s.load(&k);
        // A good start, then a change to a store that isn't there.
        let mut bad = visit(2);
        bad.push(Op::DeleteStore(b("drafts")));
        assert!(commit(&mut s, &k, 3, &bad).is_err());
        assert_eq!(s.load(&k), before);
        // Renames, deletes and a clear all come back when the run fails late.
        let bad = vec![Op::Database(b("mail")), Op::RenameStore(b("messages"), b("old")), Op::Store(b("old")),
                       Op::Clear, Op::DeleteIndex(b("by-date")), Op::KeyGenerator(9), Op::DeleteDatabase(b("mail")),
                       Op::Version(b("mail"), 7), Op::Store(b("nope"))];
        assert!(commit(&mut s, &k, 4, &bad).is_err());
        assert_eq!(s.load(&k), before);
        // Over the quota: refused, and nothing kept.
        let big = vec![1u8; QUOTA];
        assert!(commit(&mut s, &k, 5, &[Op::Database(b("mail")), Op::Store(b("messages")), Op::Put(b("x"), big)]).is_err());
        assert_eq!(s.load(&k), before);
        assert_eq!(visits(&mut s, &k), Some(1));
    }

    #[test]
    fn parts_must_come_in_order_of_one_run_and_only_the_last_applies() {
        let mut s = Stores::new(None);
        let k = key(Some(1));
        commit(&mut s, &k, 1, &schema()).unwrap();
        let mut ops = vec![Op::Database(b("mail")), Op::Store(b("messages"))];
        ops.push(Op::Put(b("long"), vec![3u8; 3 * PART_MAX]));
        let p = parts(9, &ops);
        assert!(p.len() >= 4 && p.iter().all(|p| p.len() <= PART_MAX));
        let before = s.load(&k);
        s.apply(&k, &p[0]).unwrap();
        s.apply(&k, &p[1]).unwrap();
        assert_eq!(s.load(&k), before, "nothing until the last part");
        assert!(s.apply(&k, &p[3]).is_err(), "a skipped part");
        assert!(s.apply(&k, &p[2]).is_err(), "the run was dropped");
        // A new run drops a run not finished.
        s.apply(&k, &p[0]).unwrap();
        for q in &p[1..] { s.apply(&k, q).unwrap(); }
        let mut dbs = Databases::new();
        apply_ops(&mut dbs, decode_ops(&s.load(&k), LOAD_MAX).unwrap(), &mut vec![]).unwrap();
        assert_eq!(dbs[&b("mail")].stores[&b("messages")].records[&b("long")], vec![3u8; 3 * PART_MAX]);
    }

    #[test]
    fn unrecorded_jars_are_apart_in_memory_and_go_with_their_jar() {
        let d = Dir::new("jar");
        let mut s = Stores::new(Some(d.0.clone()));
        commit(&mut s, &key(Some(1)), 1, &schema()).unwrap();
        commit(&mut s, &key(Some(1)), 2, &visit(5)).unwrap();
        assert_eq!(visits(&mut s, &key(Some(2))), None);
        assert_eq!(visits(&mut s, &key(None)), None);
        assert!(!d.0.exists(), "nothing written for an Unrecorded jar");
        s.forget_jar(1);
        assert_eq!(visits(&mut s, &key(Some(1))), None);
    }

    #[test]
    fn hostile_parts_are_refused_and_stores_read_back_exactly() {
        let mut s = Stores::new(None);
        let k = key(Some(1));
        for p in [&b""[..], &[0u8; 12], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]] {
            assert!(s.apply(&k, p).is_err(), "{p:?}");
        }
        let mut long = vec![0u8; HEADER];
        long[12] = 1;
        long.push(tag::DATABASE);
        long.extend(u32::MAX.to_le_bytes());
        assert!(s.apply(&k, &long).is_err());
        let mut name = vec![0u8; HEADER];
        name[12] = 1;
        name.extend(encode_ops(&[Op::Version(vec![b'a'; NAME_MAX + 1], 1)]));
        assert!(s.apply(&k, &name).is_err(), "a name over its bound");
        assert!(commit(&mut s, &k, 2, &[Op::Put(b("k"), b("v"))]).is_err(), "no store selected");
        assert!(commit(&mut s, &k, 3, &[Op::Version(b("d"), 1), Op::IndexPut(b("a"), b("b"))]).is_err(), "no index selected");
        // Every kind of change reads back as itself.
        let all = vec![Op::Version(b("d"), 3), Op::CreateStore(b("s"), b("about")), Op::KeyGenerator(42),
                       Op::Put(b("k1"), b("v1")), Op::Put(b("k2"), b("v")), Op::Append(b("k2"), b("2")),
                       Op::Delete(b("k1")), Op::CreateIndex(b("i"), b("unique")), Op::IndexPut(b("v2"), b("k2")),
                       Op::IndexPut(b("x"), b("y")), Op::IndexDelete(b("x"), b("y")), Op::RenameIndex(b("i"), b("j")),
                       Op::Database(b("d")), Op::CreateStore(b("t"), b("")), Op::RenameStore(b("t"), b("u")),
                       Op::Store(b("u")), Op::Put(b("z"), b("z")), Op::Clear];
        assert_eq!(decode_ops(&encode_ops(&all), RUN_MAX), Some(all.clone()));
        commit(&mut s, &k, 4, &all).unwrap();
        let mut dbs = Databases::new();
        apply_ops(&mut dbs, decode_ops(&s.load(&k), LOAD_MAX).unwrap(), &mut vec![]).unwrap();
        let st = &dbs[&b("d")].stores[&b("s")];
        assert_eq!((dbs[&b("d")].version, st.key_generator, st.records.len()), (3, 42, 1));
        assert_eq!(st.records[&b("k2")], b("v2"));
        assert_eq!(st.indexes[&b("j")].records.iter().cloned().collect::<Vec<_>>(), vec![(b("v2"), b("k2"))]);
        assert!(dbs[&b("d")].stores[&b("u")].records.is_empty());
        assert_eq!(size(&dbs), s.open[&(k.clone(), Kind::Idb)].size);
    }
}
