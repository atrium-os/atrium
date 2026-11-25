//! The origins a persona chose to run live every time (navigator-islands.md
//! D8, I6.6): "Always for this site" on the run-live prompt adds the page's
//! origin, and the chip's "Ask before running live" takes it away. A page
//! of a remembered origin, opened in a recorded trail, runs live without a
//! press; an Unrecorded trail never remembers and is never run unasked.
//!
//! The list is a file under navigatord's store directory, on the persona's
//! encrypted volume, so it is per persona and goes with it (Tessera E5).
//! It is read again on every check, so another window of the same persona,
//! served by another navigatord, sees a change at once. A change holds an
//! exclusive lock on a sibling file from its read to its write, so a revoke
//! in one navigatord and a remember in another can't lose either. Without
//! a store it is kept in memory only.
//! A file that doesn't read as a list of origins counts as empty: an
//! unreadable list never runs anything unasked.

use std::path::PathBuf;

/// A page's origin as navigatord writes one: `scheme://host[:port]`, from
/// a parsed URL, lower-case and in a narrow alphabet; `None` otherwise.
pub fn origin(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    let o = match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    };
    o.bytes().all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b':' | b'/' | b'-' | b'[' | b']' | b'_')).then_some(o)
}

/// Write `bytes` to `file` whole or not at all: a fresh 0600 file beside
/// it (never one already there), synced, then renamed over it.
pub fn write(file: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
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


/// The most origins remembered; remembering one more forgets the oldest.
pub const REMEMBERED_MAX: usize = 256;
/// The longest origin kept.
pub const ORIGIN_MAX: usize = 300;
/// The longest file read: every line at its longest.
const FILE_MAX: u64 = (REMEMBERED_MAX * (ORIGIN_MAX + 1)) as u64;

/// Whether `o` is an origin as navigatord writes one: `https://host` or
/// `https://host:port`, nothing else.
fn origin_ok(o: &str) -> bool {
    o.len() <= ORIGIN_MAX && o.starts_with("https://") && origin(o).as_deref() == Some(o)
}

pub struct Remembered {
    file: Option<PathBuf>,
    /// Without a file: the list itself.
    memory: Vec<String>,
}

impl Remembered {
    pub fn new(file: Option<PathBuf>) -> Remembered { Remembered { file, memory: Vec::new() } }

    fn load(&self) -> Vec<String> {
        let Some(f) = &self.file else { return self.memory.clone() };
        let read = || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(f)?.take(FILE_MAX + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        let bytes = match read() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => { eprintln!("navigatord: {}: {e}; no site runs live unasked", f.display()); return Vec::new(); }
        };
        let list = (bytes.len() as u64 <= FILE_MAX).then(|| std::str::from_utf8(&bytes).ok()).flatten()
            .map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
            .filter(|l| l.len() <= REMEMBERED_MAX && l.iter().all(|o| origin_ok(o)));
        list.unwrap_or_else(|| {
            eprintln!("navigatord: {} is not a list of origins; no site runs live unasked", f.display());
            Vec::new()
        })
    }

    /// Change the list under the lock: no other navigatord's change lands
    /// between this read and this write.
    fn change(&mut self, f: impl FnOnce(&mut Vec<String>) -> bool) -> Result<(), String> {
        let _lock = match &self.file {
            None => None,
            Some(file) => {
                use std::os::unix::fs::OpenOptionsExt;
                let name = format!(".{}.lock", file.file_name().and_then(|n| n.to_str()).unwrap_or("live-origins"));
                let path = file.with_file_name(name);
                if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?; }
                let l = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                l.lock().map_err(|e| format!("{}: {e}", path.display()))?;
                Some(l)
            }
        };
        let mut list = self.load();
        if !f(&mut list) { return Ok(()); }
        self.save(list)
    }

    fn save(&mut self, list: Vec<String>) -> Result<(), String> {
        match &self.file {
            None => { self.memory = list; Ok(()) }
            Some(f) => {
                let mut text = list.join("\n");
                if !text.is_empty() { text.push('\n'); }
                write(f, text.as_bytes())
            }
        }
    }

    /// Every remembered origin, the newest first (the Settings pane's list).
    pub fn list(&self) -> Vec<String> {
        let mut l = self.load();
        l.reverse();
        l
    }

    /// Ask again before running any site live.
    pub fn forget_all(&mut self) -> Result<(), String> {
        self.change(|list| { let had = !list.is_empty(); list.clear(); had })
    }

    /// Whether the page at `url` is of an origin the person chose to run
    /// live every time.
    pub fn holds(&self, url: &str) -> bool {
        origin(url).is_some_and(|o| self.load().contains(&o))
    }

    /// Remember the origin of the page at `url`: last, so it is the newest.
    pub fn remember(&mut self, url: &str) -> Result<(), String> {
        let o = origin(url).filter(|o| origin_ok(o)).ok_or("not an HTTPS origin")?;
        self.change(|list| {
            list.retain(|x| *x != o);
            list.push(o);
            if list.len() > REMEMBERED_MAX { list.drain(..list.len() - REMEMBERED_MAX); }
            true
        })
    }

    /// Ask again before running the page at `url`'s origin live.
    pub fn forget(&mut self, url: &str) -> Result<(), String> {
        let Some(o) = origin(url) else { return Ok(()) };
        self.change(|list| {
            let before = list.len();
            list.retain(|x| *x != o);
            list.len() != before
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("navigatord-live-remembered-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
    }
    impl Drop for Dir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn an_origin_is_remembered_across_navigatords_until_forgotten() {
        let d = Dir::new("keep");
        let file = d.0.join("live-origins");
        let mut a = Remembered::new(Some(file.clone()));
        assert!(!a.holds("https://app.example/app.html"));
        a.remember("https://app.example/app.html?x=1").unwrap();
        let b = Remembered::new(Some(file.clone()));
        assert!(b.holds("https://app.example/other.html"), "another window of the persona sees it");
        assert!(!b.holds("https://api.app.example/"), "an origin, not a site");
        assert!(!b.holds("https://app.example:8443/"), "the port is part of it");
        assert!(!b.holds("http://app.example/"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "https://app.example\n");
        a.remember("https://b.example/").unwrap();
        assert_eq!(b.list(), vec!["https://b.example".to_string(), "https://app.example".to_string()], "newest first");
        a.forget("https://app.example/").unwrap();
        assert!(!b.holds("https://app.example/app.html"));
        assert!(a.remember("http://app.example/").is_err());
        assert!(a.remember("not a url").is_err());
        a.forget_all().unwrap();
        assert!(b.list().is_empty());
    }

    #[test]
    fn changes_from_two_navigatords_at_once_are_none_of_them_lost() {
        let d = Dir::new("race");
        let file = d.0.join("live-origins");
        let threads: Vec<_> = (0..8).map(|t| {
            let file = file.clone();
            std::thread::spawn(move || {
                let mut m = Remembered::new(Some(file));
                for i in 0..10 { m.remember(&format!("https://t{t}-{i}.example/")).unwrap(); }
                for i in 0..10 { if i % 2 == 0 { m.forget(&format!("https://t{t}-{i}.example/")).unwrap(); } }
            })
        }).collect();
        for t in threads { t.join().unwrap(); }
        let m = Remembered::new(Some(file));
        for t in 0..8 {
            for i in 0..10 {
                assert_eq!(m.holds(&format!("https://t{t}-{i}.example/")), i % 2 == 1, "t{t}-{i}");
            }
        }
    }

    #[test]
    fn the_list_is_bounded_and_the_oldest_goes() {
        let mut m = Remembered::new(None);
        for i in 0..REMEMBERED_MAX + 1 { m.remember(&format!("https://s{i}.example/")).unwrap(); }
        assert!(!m.holds("https://s0.example/"));
        assert!(m.holds("https://s1.example/") && m.holds(&format!("https://s{REMEMBERED_MAX}.example/")));
        m.remember("https://s1.example/").unwrap();
        m.remember("https://new.example/").unwrap();
        assert!(m.holds("https://s1.example/"), "remembering again makes it the newest");
        assert!(!m.holds("https://s2.example/"));
    }

    #[test]
    fn a_list_that_does_not_read_runs_nothing_unasked() {
        let d = Dir::new("bad");
        let file = d.0.join("live-origins");
        let m = Remembered::new(Some(file.clone()));
        for bad in [&b"https://app.example\nhttps://app.example/path\n"[..], b"https://app.example\njavascript:x\n",
                    b"\xff\xfe", b"https://app.example\nhttp://app.example\n"] {
            std::fs::write(&file, bad).unwrap();
            assert!(!m.holds("https://app.example/"), "{bad:?}");
        }
        std::fs::write(&file, "https://app.example\n".repeat(REMEMBERED_MAX + 1)).unwrap();
        assert!(!m.holds("https://app.example/"), "too many lines");
        std::fs::write(&file, vec![b'a'; FILE_MAX as usize + 1]).unwrap();
        assert!(!m.holds("https://app.example/"), "too long");
        std::fs::write(&file, "https://app.example\n").unwrap();
        assert!(m.holds("https://app.example/"), "the control");
    }
}
