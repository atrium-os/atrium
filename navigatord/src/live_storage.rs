//! A live page's `localStorage` (navigator-islands.md I6.3, D10 as
//! decided 2026-11-25): navigatord keeps it, one store per (jar, site),
//! as Ladybird keeps storage in its browser process rather than the page's.
//! The engine never sees a file. It loads its site's store once, from
//! [`limen::web_island::LIVE_STORAGE`] on its fetch channel, and sends each
//! change there as a POST of [`Op`]s. A recorded trail's store is a file
//! under navigatord's store directory, on the persona's encrypted volume,
//! so removing the persona removes it (Tessera E5); an Unrecorded trail's
//! is kept in memory only and dropped with its jar.
//!
//! Everything from the engine is hostile: every length is bounded before it
//! is read, the whole store is held to [`QUOTA`], and a batch that would
//! break any bound changes nothing.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// The most bytes (keys and values, UTF-8) one site keeps: LibWeb's own
/// `localStorage` quota.
pub const QUOTA: usize = 5 * 1024 * 1024;
/// The most items one site keeps, so a loaded store is at most
/// [`LOAD_MAX`] bytes however small its items.
pub const ITEMS_MAX: usize = 65_536;
/// The longest store the engine loads: the quota and each item's lengths.
pub const LOAD_MAX: usize = QUOTA + 8 * ITEMS_MAX;
/// The most bytes one batch of changes carries (Nuntius's POST bound).
pub const BATCH_MAX: usize = nuntius::BODY_MAX;

/// Whose store: the trail's jar (`None`: the persona's own, kept on disk)
/// and the page's registrable site.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub jar: Option<u64>,
    pub site: String,
}

impl Key {
    /// The store of the page at `url` in `jar`, if its site is one a file
    /// may be named for (lower-case letters, digits, dots and hyphens).
    pub fn of(jar: Option<u64>, url: &str) -> Option<Key> {
        let site = crate::site_of(url)?;
        let named = !site.is_empty() && site.len() <= 253 && !site.starts_with('.')
            && site.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        named.then_some(Key { jar, site })
    }
}

/// One change, as the engine sends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Set(String, String),
    /// More of a value too long for one batch: added to the end of `key`'s.
    Append(String, String),
    Remove(String),
    Clear,
}

const SET: u8 = 1;
const REMOVE: u8 = 2;
const CLEAR: u8 = 3;
const APPEND: u8 = 4;

type Map = BTreeMap<String, String>;

fn size(map: &Map) -> usize {
    map.iter().map(|(k, v)| k.len() + v.len()).sum()
}

fn put(out: &mut Vec<u8>, s: &str) {
    out.extend((s.len() as u32).to_le_bytes());
    out.extend(s.as_bytes());
}

/// A reader over bytes from the engine or a file: each length is checked
/// against what is left before anything is taken.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn done(&self) -> bool { self.0.is_empty() }
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }
    fn text(&mut self) -> Option<String> {
        let (len, rest) = self.0.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        if len > rest.len() { return None; }
        let (text, rest) = rest.split_at(len);
        self.0 = rest;
        String::from_utf8(text.to_vec()).ok()
    }
}

/// The store as the engine loads it: each key and value, length-prefixed.
pub fn encode(map: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = Vec::with_capacity(size(map) + 8 * map.len());
    for (k, v) in map {
        put(&mut out, k);
        put(&mut out, v);
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Map> {
    if bytes.len() > LOAD_MAX { return None; }
    let mut r = Reader(bytes);
    let mut map = Map::new();
    while !r.done() {
        let (k, v) = (r.text()?, r.text()?);
        map.insert(k, v);
    }
    (size(&map) <= QUOTA && map.len() <= ITEMS_MAX).then_some(map)
}

/// A batch of changes, as the engine sends it.
pub fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let mut out = vec![];
    for op in ops {
        match op {
            Op::Set(k, v) => { out.push(SET); put(&mut out, k); put(&mut out, v); }
            Op::Append(k, v) => { out.push(APPEND); put(&mut out, k); put(&mut out, v); }
            Op::Remove(k) => { out.push(REMOVE); put(&mut out, k); }
            Op::Clear => out.push(CLEAR),
        }
    }
    out
}

pub fn decode_ops(bytes: &[u8]) -> Option<Vec<Op>> {
    if bytes.len() > BATCH_MAX { return None; }
    let mut r = Reader(bytes);
    let mut ops = vec![];
    while !r.done() {
        ops.push(match r.byte()? {
            SET => Op::Set(r.text()?, r.text()?),
            APPEND => Op::Append(r.text()?, r.text()?),
            REMOVE => Op::Remove(r.text()?),
            CLEAR => Op::Clear,
            _ => return None,
        });
    }
    Some(ops)
}

/// Every live page's store, for this navigatord (one persona).
pub struct Stores {
    /// `<store dir>/live-storage`, for the persona's own jar.
    dir: Option<PathBuf>,
    /// Loaded stores, and every Unrecorded one.
    open: HashMap<Key, Map>,
}

impl Stores {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Stores { dir, open: HashMap::new() }
    }

    fn file(&self, key: &Key) -> Option<PathBuf> {
        match key.jar {
            None => self.dir.as_ref().map(|d| d.join(format!("{}.store", key.site))),
            Some(_) => None,
        }
    }

    fn map(&mut self, key: &Key) -> &mut Map {
        if !self.open.contains_key(key) {
            let map = self.file(key).and_then(|f| match std::fs::read(&f) {
                Ok(b) => decode(&b).or_else(|| {
                    eprintln!("navigatord: {} is out of shape; the site starts empty", f.display());
                    None
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    eprintln!("navigatord: {}: {e}; the site starts empty", f.display());
                    None
                }
            }).unwrap_or_default();
            self.open.insert(key.clone(), map);
        }
        self.open.get_mut(key).expect("inserted above")
    }

    /// The site's store, as the engine loads it.
    pub fn load(&mut self, key: &Key) -> Vec<u8> {
        encode(self.map(key))
    }

    /// Apply the engine's batch: all of it, or (out of shape, or over the
    /// quota) none of it. A recorded store is written before this returns.
    pub fn apply(&mut self, key: &Key, batch: &[u8]) -> Result<(), String> {
        let ops = decode_ops(batch).ok_or("the changes are out of shape")?;
        let mut next = self.map(key).clone();
        for op in ops {
            match op {
                Op::Set(k, v) => { next.insert(k, v); }
                Op::Append(k, more) => match next.get_mut(&k) {
                    Some(v) if v.len() + more.len() <= QUOTA => v.push_str(&more),
                    Some(_) => return Err("over the site's storage quota".into()),
                    None => return Err("more of a value that was never set".into()),
                },
                Op::Remove(k) => { next.remove(&k); }
                Op::Clear => next.clear(),
            }
        }
        if size(&next) > QUOTA || next.len() > ITEMS_MAX { return Err("over the site's storage quota".into()); }
        if let Some(file) = self.file(key) {
            write(&file, &encode(&next))?;
        }
        self.open.insert(key.clone(), next);
        Ok(())
    }

    /// The person forgot the site in that jar: its store goes too.
    pub fn forget(&mut self, key: &Key) {
        self.open.remove(key);
        if let Some(f) = self.file(key) {
            if let Err(e) = std::fs::remove_file(&f) {
                if e.kind() != std::io::ErrorKind::NotFound { eprintln!("navigatord: {}: {e}", f.display()); }
            }
        }
    }

    /// An Unrecorded jar ended: every store in it goes.
    pub fn forget_jar(&mut self, jar: u64) {
        self.open.retain(|k, _| k.jar != Some(jar));
    }
}

/// Write `bytes` to `file` whole or not at all: a fresh 0600 file beside
/// it (never one already there), synced, then renamed over it.
pub(crate) fn write(file: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = file.parent().ok_or("no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}.{n}", file.file_name().and_then(|n| n.to_str()).unwrap_or("store"), std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result.map_err(|e| format!("{}: {e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(jar: Option<u64>) -> Key { Key::of(jar, "https://app.example/app.html").unwrap() }

    /// A fresh directory of this test's own.
    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("navigatord-live-storage-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
        fn path(&self) -> &std::path::Path { &self.0 }
    }
    impl Drop for Dir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn a_site_keeps_its_store_across_runs_and_navigatords() {
        let d = Dir::new("kept");
        let mut s = Stores::new(Some(d.path().into()));
        assert_eq!(s.load(&key(None)), b"");
        s.apply(&key(None), &encode_ops(&[Op::Set("visits".into(), "1".into()), Op::Set("名".into(), "値".into())])).unwrap();
        s.apply(&key(None), &encode_ops(&[Op::Set("visits".into(), "2".into())])).unwrap();
        // A new navigatord reads it from the file.
        let mut again = Stores::new(Some(d.path().into()));
        let map = decode(&again.load(&key(None))).unwrap();
        assert_eq!(map.get("visits").map(String::as_str), Some("2"));
        assert_eq!(map.get("名").map(String::as_str), Some("値"));
        // A sibling host shares the site's store; another site does not.
        assert_eq!(Key::of(None, "https://api.app.example/x"), Some(key(None)));
        assert_eq!(again.load(&Key::of(None, "https://other.example/").unwrap()), b"");
        // The file is the owner's alone.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.path().join("app.example.store")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Forgetting the site removes it.
        again.forget(&key(None));
        assert!(!d.path().join("app.example.store").exists());
        assert_eq!(Stores::new(Some(d.path().into())).load(&key(None)), b"");
    }

    #[test]
    fn an_unrecorded_jar_is_kept_in_memory_and_apart() {
        let d = Dir::new("unrecorded");
        let mut s = Stores::new(Some(d.path().into()));
        s.apply(&key(Some(7)), &encode_ops(&[Op::Set("k".into(), "v".into())])).unwrap();
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0, "nothing on disk");
        assert_eq!(s.load(&key(None)), b"", "the persona's own jar does not see it");
        assert_ne!(s.load(&key(Some(7))), b"");
        s.forget_jar(7);
        assert_eq!(s.load(&key(Some(7))), b"");
    }

    #[test]
    fn a_batch_over_a_bound_or_out_of_shape_changes_nothing() {
        let mut s = Stores::new(None);
        s.apply(&key(None), &encode_ops(&[Op::Set("a".into(), "1".into())])).unwrap();
        let before = s.load(&key(None));
        // Over the quota by appending, in pieces that each fit a batch.
        let piece = "x".repeat(BATCH_MAX - 64);
        s.apply(&key(None), &encode_ops(&[Op::Set("big".into(), piece.clone())])).unwrap();
        let mut grew = 0;
        while s.apply(&key(None), &encode_ops(&[Op::Append("big".into(), piece.clone())])).is_ok() { grew += 1; assert!(grew < 1000); }
        let full = s.load(&key(None));
        assert!(decode(&full).is_some_and(|m| size(&m) <= QUOTA));
        // Every refusal below leaves the store as it was.
        let bad: Vec<Vec<u8>> = vec![
            encode_ops(&[Op::Remove("a".into()), Op::Append("big".into(), piece.clone())]),
            encode_ops(&[Op::Append("never".into(), "x".into())]),
            vec![9],
            vec![SET, 255, 255, 255, 255],
            [encode_ops(&[Op::Clear]), vec![SET, 1, 0, 0, 0, 0xff, 0, 0, 0, 0]].concat(),
            vec![0; BATCH_MAX + 1],
        ];
        for b in bad {
            assert!(s.apply(&key(None), &b).is_err());
            assert_eq!(s.load(&key(None)), full);
        }
        s.apply(&key(None), &encode_ops(&[Op::Clear])).unwrap();
        assert_eq!(s.load(&key(None)), b"");
        assert_ne!(before, full);
    }

    #[test]
    fn a_site_keeps_at_most_its_item_count() {
        let mut s = Stores::new(None);
        let mut n = 0usize;
        let refused = loop {
            let ops: Vec<Op> = (0..2000).map(|i| Op::Set(format!("{}", n + i), String::new())).collect();
            match s.apply(&key(None), &encode_ops(&ops)) {
                Ok(()) => n += 2000,
                Err(e) => break e,
            }
            assert!(n <= ITEMS_MAX);
        };
        assert!(refused.contains("quota"));
        assert!(s.load(&key(None)).len() <= LOAD_MAX);
    }

    #[test]
    fn a_damaged_file_starts_empty() {
        let d = Dir::new("damaged");
        std::fs::write(d.path().join("app.example.store"), [1, 0, 0, 0]).unwrap();
        assert_eq!(Stores::new(Some(d.path().into())).load(&key(None)), b"");
    }

    #[test]
    fn a_site_a_file_cannot_be_named_for_has_no_store() {
        assert!(Key::of(None, "https://app.example/").is_some());
        assert!(Key::of(None, "https://[::1]/").is_none());
    }
}

/// A live page's storage batches and loaded stores, fuzzed (I6.6): never a
/// panic, and what is taken encodes back to the same bytes.
#[cfg(test)]
mod storage_fuzz {
    use super::*;

    #[test]
    fn hostile_batches_and_stores_are_refused_or_read_back_exactly() {
        let mut s: u64 = 0x51_7cc1_b727_220a;
        let mut next = move || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
        let honest = encode_ops(&[Op::Set("k".into(), "v".into()), Op::Append("k".into(), "w".into()), Op::Remove("j".into()), Op::Clear]);
        let (mut ops_taken, mut maps_taken) = (0, 0);
        for _ in 0..50_000 {
            let mut b = honest.clone();
            for _ in 0..1 + next() % 4 {
                let at = (next() as usize) % (b.len() + 1);
                match next() % 3 {
                    0 => b.truncate(at),
                    1 => b.insert(at, next() as u8),
                    _ => if at < b.len() { b[at] = next() as u8 },
                }
            }
            if let Some(ops) = decode_ops(&b) { ops_taken += 1; assert_eq!(encode_ops(&ops), b); }
            if let Some(map) = decode(&b) {
                maps_taken += 1;
                assert!(size(&map) <= QUOTA && map.len() <= ITEMS_MAX);
            }
        }
        assert!(ops_taken > 100 && maps_taken > 0, "{ops_taken} {maps_taken}");
    }
}
