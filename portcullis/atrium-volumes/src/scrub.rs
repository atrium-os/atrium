//! Scrubs after a move-in (tessera-encryption.md §13.1 E4.7; Shankar
//! 2026-11-19, "scrub them, as a next step"). A plain folder moved into its
//! persona's key group leaves its old plaintext on disk: in free space, in
//! packs it shared with files that still live, pinned by retained
//! snapshots, and (names) in the journal's old records. The kernel's scrub
//! (`TESSERA_IOC_SCRUB`) clears all four.
//!
//! It writes the whole free space, so it runs on a thread of its own and
//! login doesn't wait for it. The ask is made durable before the parked
//! copy is deleted (`want`), so a crash or a reboot in between still gets
//! its scrub: the queue is a file, one directory per line, read again at
//! start and at every kick. A scrub that fails stays queued for the next
//! kick (the next move-in or the next start).

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use log::{info, warn};

use crate::ffi::ScrubReport;

pub const DEFAULT_PATH: &str = "/var/db/atrium/volumes.scrub";

/// What persona.rs needs: remember a volume to scrub, then start.
pub trait ScrubQueue {
    /// Durably remember that the volume holding `dir` needs a scrub.
    fn want(&self, dir: &Path) -> io::Result<()>;
    /// Start on everything remembered.
    fn kick(&self);
}

pub struct Scrubber {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
    tx:   Sender<()>,
}

type RunFn = fn(&Path) -> io::Result<ScrubReport>;

impl Scrubber {
    /// The worker starts at once on whatever an earlier run left queued.
    pub fn start(path: PathBuf, run: RunFn) -> Scrubber {
        let (tx, rx) = mpsc::channel();
        let lock = Arc::new(Mutex::new(()));
        let (p, l) = (path.clone(), lock.clone());
        let _ = thread::Builder::new()
            .name("scrub".into())
            .spawn(move || worker(&p, &l, rx, run));
        let s = Scrubber { path, lock, tx };
        s.kick();
        s
    }
}

impl ScrubQueue for Scrubber {
    fn want(&self, dir: &Path) -> io::Result<()> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut q = read(&self.path)?;
        if !q.iter().any(|d| d == dir) {
            q.push(dir.to_path_buf());
            write(&self.path, &q)?;
        }
        Ok(())
    }

    fn kick(&self) {
        let _ = self.tx.send(());
    }
}

fn worker(path: &Path, lock: &Mutex<()>, rx: Receiver<()>, run: RunFn) {
    while rx.recv().is_ok() {
        while rx.try_recv().is_ok() {}
        let queued = {
            let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
            match read(path) {
                Ok(q) => q,
                Err(e) => {
                    warn!("scrub: {}: {e}", path.display());
                    continue;
                }
            }
        };
        let done = run_queue(&queued, run);
        if done.is_empty() {
            continue;
        }
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        match read(path) {
            Ok(mut q) => {
                q.retain(|d| !done.contains(d));
                if let Err(e) = write(path, &q) {
                    warn!("scrub: {}: {e}", path.display());
                }
            }
            Err(e) => warn!("scrub: {}: {e}", path.display()),
        }
    }
}

/// Scrub each volume once. Returns the entries that are done: scrubbed,
/// on a volume scrubbed for another entry, or gone.
fn run_queue(queued: &[PathBuf], run: RunFn) -> Vec<PathBuf> {
    let mut done = Vec::new();
    let mut scrubbed: Vec<u64> = Vec::new();
    for d in queued {
        let dev = match fs::metadata(d) {
            Ok(m) => m.dev(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                warn!("scrub: {} is gone; dropped from the queue", d.display());
                done.push(d.clone());
                continue;
            }
            Err(e) => {
                warn!("scrub: {}: {e}", d.display());
                continue;
            }
        };
        if scrubbed.contains(&dev) {
            done.push(d.clone());
            continue;
        }
        match run(d) {
            Ok(r) => {
                info!("scrub: {}: {} snapshot(s) retired, {} pack(s) freed, {} rebuilt \
                       ({} blob(s) dropped), {} free sector(s) and {} journal block(s) zeroed",
                    d.display(), r.snapshots_retired, r.packs_freed, r.packs_compacted,
                    r.blobs_dropped, r.zeroed_sectors, r.journal_blocks);
                if r.packs_young > 0 {
                    // Inside the GC grace period: those stay until the
                    // next scrub.
                    warn!("scrub: {}: {} partly-dead pack(s) too young to rebuild; kept queued",
                        d.display(), r.packs_young);
                    continue;
                }
                scrubbed.push(dev);
                done.push(d.clone());
            }
            Err(e) => warn!("scrub: {}: {e}; kept queued", d.display()),
        }
    }
    done
}

fn read(path: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s.lines().filter(|l| l.starts_with('/')).map(PathBuf::from).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn write(path: &Path, q: &[PathBuf]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp)?;
        for d in q {
            writeln!(f, "{}", d.display())?;
        }
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    static RUNS: AtomicU32 = AtomicU32::new(0);
    static FAIL: AtomicU32 = AtomicU32::new(0);

    fn fake(_d: &Path) -> io::Result<ScrubReport> {
        RUNS.fetch_add(1, Ordering::SeqCst);
        if FAIL.load(Ordering::SeqCst) > 0 {
            FAIL.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        Ok(ScrubReport::default())
    }

    fn wait_empty(p: &Path) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            if read(p).unwrap().is_empty() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    // One test, so the counters aren't shared between threads of the
    // test harness.
    #[test]
    fn queued_scrubs_run_once_per_volume_survive_a_restart_and_retry() {
        let d = tempfile::tempdir().unwrap();
        let q = d.path().join("db/scrub");
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();

        // Left from an earlier run: picked up at start. a and b are on
        // one volume, so one scrub does for both; a gone entry is dropped.
        write(&q, &[a.clone(), b.clone(), d.path().join("gone")]).unwrap();
        let s = Scrubber::start(q.clone(), fake);
        assert!(wait_empty(&q));
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // want is durable before the kick, and idempotent.
        s.want(&a).unwrap();
        s.want(&a).unwrap();
        assert_eq!(read(&q).unwrap(), vec![a.clone()]);
        s.kick();
        assert!(wait_empty(&q));
        assert_eq!(RUNS.load(Ordering::SeqCst), 2);

        // A failed scrub stays queued until the next kick.
        FAIL.store(1, Ordering::SeqCst);
        s.want(&b).unwrap();
        s.kick();
        let t0 = Instant::now();
        while RUNS.load(Ordering::SeqCst) < 3 && t0.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(read(&q).unwrap(), vec![b.clone()]);
        s.kick();
        assert!(wait_empty(&q));
        assert_eq!(RUNS.load(Ordering::SeqCst), 4);
    }
}
