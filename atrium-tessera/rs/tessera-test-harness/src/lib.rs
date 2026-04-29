//! In-memory block-device simulator + crash-injection helpers.
//!
//! Phase-3 infrastructure that lets us drive tessera-core without
//! touching real storage or the kmod. Supports:
//!
//!   - `MemDisk`: a flat array of 4 KiB sectors with valid/used flags
//!     and a simple bump-allocator backing for `tessera_block_io_t`.
//!   - `WriteRecorder`: wraps a MemDisk and journals every successful
//!     write_block as a (sector, payload) pair so a test can later
//!     materialise the disk's state at any prefix-of-writes — i.e.
//!     "what if we crashed here?".
//!
//! All callbacks are `extern "C" fn` so they slot directly into
//! `tessera_block_io_t`. The MemDisk is held by raw pointer in the
//! vtable's `ctx` field; callers must keep the owning struct alive
//! for the duration of any C-side use.

use core::ffi::c_void;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

use tessera_sys::tessera_block_io_t;

pub const SECTOR: usize = 4096;
pub type Block = [u8; SECTOR];

/* ── MemDisk ─────────────────────────────────────────────────────── */

pub struct MemDisk {
    pub blocks: Vec<Block>,
    pub used:   Vec<bool>,
    next_alloc: u64,
}

impl MemDisk {
    pub fn new(num_sectors: usize) -> Self {
        Self {
            blocks: vec![[0u8; SECTOR]; num_sectors],
            used:   vec![false; num_sectors],
            next_alloc: 1,
        }
    }

    /// Mark every sector in `[start, start+len)` as live without
    /// initialising contents — useful for image files where the host
    /// has pre-zeroed the backing.
    pub fn mark_live(&mut self, start: u64, len: u64) {
        for s in start..start + len {
            if (s as usize) < self.used.len() {
                self.used[s as usize] = true;
            }
        }
    }

    pub fn block_io(disk: *mut MemDisk) -> tessera_block_io_t {
        tessera_block_io_t {
            read_block:  Some(md_read),
            write_block: Some(md_write),
            alloc:       Some(md_alloc),
            free:        Some(md_free),
            ctx:         disk as *mut c_void,
        }
    }
}

extern "C" fn md_read(ctx: *mut c_void, s: u64, out: *mut u8) -> i32 {
    let d = unsafe { &*(ctx as *const MemDisk) };
    let i = s as usize;
    if i >= d.blocks.len() || !d.used[i] { return -1; }
    let dst = unsafe { std::slice::from_raw_parts_mut(out, SECTOR) };
    dst.copy_from_slice(&d.blocks[i]);
    0
}

extern "C" fn md_write(ctx: *mut c_void, s: u64, buf: *const u8) -> i32 {
    let d = unsafe { &mut *(ctx as *mut MemDisk) };
    let i = s as usize;
    if i >= d.blocks.len() { return -1; }
    let src = unsafe { std::slice::from_raw_parts(buf, SECTOR) };
    d.blocks[i].copy_from_slice(src);
    d.used[i] = true;
    0
}

extern "C" fn md_alloc(ctx: *mut c_void, n: u64, out: *mut u64) -> i32 {
    let d = unsafe { &mut *(ctx as *mut MemDisk) };
    if n != 1 { return -1; }
    for i in d.next_alloc..d.blocks.len() as u64 {
        if !d.used[i as usize] {
            d.used[i as usize] = true;
            d.next_alloc = i + 1;
            unsafe { *out = i; }
            return 0;
        }
    }
    for i in 1..d.next_alloc {
        if !d.used[i as usize] {
            d.used[i as usize] = true;
            unsafe { *out = i; }
            return 0;
        }
    }
    -1
}

extern "C" fn md_free(ctx: *mut c_void, s: u64, n: u64) -> i32 {
    let d = unsafe { &mut *(ctx as *mut MemDisk) };
    let i = s as usize;
    if n != 1 || i >= d.blocks.len() { return -1; }
    d.used[i] = false;
    if (i as u64) < d.next_alloc { d.next_alloc = i as u64; }
    0
}

/* ── WriteRecorder ───────────────────────────────────────────────── */

/// Wraps a MemDisk so every successful write_block also pushes a copy
/// of (sector, payload) onto an internal log. `replay_through(n)`
/// produces a fresh MemDisk that has only seen the first `n` writes.
pub struct WriteRecorder {
    pub disk:   MemDisk,
    pub writes: RefCell<Vec<(u64, Block)>>,
    /// Snapshot of the disk *before* any writes were recorded — used
    /// as the base when materialising a partial-replay state.
    pub baseline: Vec<Block>,
    pub baseline_used: Vec<bool>,
}

impl WriteRecorder {
    pub fn new(num_sectors: usize) -> Self {
        let disk = MemDisk::new(num_sectors);
        Self {
            baseline:      disk.blocks.clone(),
            baseline_used: disk.used.clone(),
            writes:        RefCell::new(Vec::new()),
            disk,
        }
    }

    /// Re-baseline: take the current disk state as the "before any
    /// recorded writes" reference. Useful when you want to record only
    /// the writes from a specific operation onward (e.g. record the
    /// formatted volume, then re-baseline, then start recording the
    /// transaction's writes).
    pub fn rebaseline(&mut self) {
        self.baseline      = self.disk.blocks.clone();
        self.baseline_used = self.disk.used.clone();
        self.writes.borrow_mut().clear();
    }

    pub fn block_io(rec: *mut WriteRecorder) -> tessera_block_io_t {
        tessera_block_io_t {
            read_block:  Some(rec_read),
            write_block: Some(rec_write),
            alloc:       Some(rec_alloc),
            free:        Some(rec_free),
            ctx:         rec as *mut c_void,
        }
    }

    /// Materialise the disk state as if only the first `n_writes`
    /// recorded writes had been issued. Always callable safely as long
    /// as `self` outlives the returned MemDisk's data.
    pub fn replay_through(&self, n_writes: usize) -> MemDisk {
        let mut d = MemDisk::new(self.baseline.len());
        d.blocks.copy_from_slice(&self.baseline);
        d.used.copy_from_slice(&self.baseline_used);
        d.next_alloc = self.disk.next_alloc;
        let log = self.writes.borrow();
        let upto = n_writes.min(log.len());
        for (s, payload) in log.iter().take(upto) {
            let i = *s as usize;
            if i < d.blocks.len() {
                d.blocks[i] = *payload;
                d.used[i] = true;
            }
        }
        d
    }

    pub fn write_count(&self) -> usize {
        self.writes.borrow().len()
    }
}

extern "C" fn rec_read(ctx: *mut c_void, s: u64, out: *mut u8) -> i32 {
    let r = unsafe { &*(ctx as *const WriteRecorder) };
    md_read(&r.disk as *const _ as *mut c_void, s, out)
}

extern "C" fn rec_write(ctx: *mut c_void, s: u64, buf: *const u8) -> i32 {
    let r = unsafe { &mut *(ctx as *mut WriteRecorder) };
    let rc = md_write(&mut r.disk as *mut _ as *mut c_void, s, buf);
    if rc == 0 {
        let mut payload = [0u8; SECTOR];
        let src = unsafe { std::slice::from_raw_parts(buf, SECTOR) };
        payload.copy_from_slice(src);
        r.writes.borrow_mut().push((s, payload));
    }
    rc
}

extern "C" fn rec_alloc(ctx: *mut c_void, n: u64, out: *mut u64) -> i32 {
    let r = unsafe { &mut *(ctx as *mut WriteRecorder) };
    md_alloc(&mut r.disk as *mut _ as *mut c_void, n, out)
}

extern "C" fn rec_free(ctx: *mut c_void, s: u64, n: u64) -> i32 {
    let r = unsafe { &mut *(ctx as *mut WriteRecorder) };
    md_free(&mut r.disk as *mut _ as *mut c_void, s, n)
}

/* ── deterministic PRNG ─────────────────────────────────────────── */

/// xorshift64. Identical state across builds → tests are reproducible
/// from the seed alone, no proptest infrastructure needed.
pub struct Xorshift64(pub AtomicU64);

impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        Self(AtomicU64::new(if seed == 0 { 1 } else { seed }))
    }
    pub fn next(&self) -> u64 {
        let mut s = self.0.load(Ordering::Relaxed);
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        self.0.store(s, Ordering::Relaxed);
        s
    }
    pub fn range(&self, n: u64) -> u64 { self.next() % n }
}
