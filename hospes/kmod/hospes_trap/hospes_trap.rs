/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * hospes_trap.rs -- the Rust half of hospes_trap.ko: a process's table of
 * trap ranges, and the checks on it (docs/spec/hospes.md §13 R1, step K1).
 *
 * A process registers ranges of its own address space (a foreign library's
 * text). A system call instruction executed inside one does not run: the
 * kernel raises SIGSYS instead (hospes_trap.c, <sys/systrap.h>).
 *
 * The table is append-only: a range is written, then the count is published
 * with release order, so the syscall path reads it without a lock. The
 * caller serialises additions (the process lock). Nothing here allocates,
 * locks, sleeps or panics on any input.
 */
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Ranges per process. A foreign-library host has one or two libraries.
pub const MAX_RANGES: usize = 16;

// errno values (sys/errno.h).
const EPERM: i32 = 1;
const EINVAL: i32 = 22;
const ENOSPC: i32 = 28;

#[repr(C)]
pub struct Range {
    start: AtomicUsize,
    end: AtomicUsize,
}

/// Shared with hospes_trap.c, which allocates it zeroed: an empty table.
#[repr(C)]
pub struct Table {
    count: AtomicU32,
    locked: AtomicU32,
    ranges: [Range; MAX_RANGES],
}

impl Table {
    /// Was the system call whose instruction ends just before `pc` (the
    /// address the thread returns to) made from inside a registered range?
    pub fn hit(&self, pc: usize) -> bool {
        let n = (self.count.load(Ordering::Acquire) as usize).min(MAX_RANGES);
        let Some(at) = pc.checked_sub(1) else { return false };
        self.ranges[..n]
            .iter()
            .any(|r| at >= r.start.load(Ordering::Relaxed) && at < r.end.load(Ordering::Relaxed))
    }

    /// Add `[start, start + len)`; `user_max` is the end of user space. The
    /// caller holds the process lock. Returns 0 or an errno.
    pub fn add(&self, start: usize, len: usize, user_max: usize) -> i32 {
        if self.locked.load(Ordering::Relaxed) != 0 {
            return EPERM;
        }
        let Some(end) = start.checked_add(len) else { return EINVAL };
        if len == 0 || end > user_max {
            return EINVAL;
        }
        let n = self.count.load(Ordering::Relaxed) as usize;
        if n >= MAX_RANGES {
            return ENOSPC;
        }
        self.ranges[n].start.store(start, Ordering::Relaxed);
        self.ranges[n].end.store(end, Ordering::Relaxed);
        self.count.store(n as u32 + 1, Ordering::Release);
        0
    }

    /// No more ranges after this. The host locks its table once the library
    /// is mapped, before any of the library's code runs.
    pub fn lock(&self) {
        self.locked.store(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }
}

// The C entry points. `no_mangle` is the only thing the unsafe_code lint
// sees in this module.
#[allow(unsafe_code)]
mod ffi {
    use super::Table;

    #[unsafe(no_mangle)]
    pub extern "C" fn hospes_trap_table_size() -> usize {
        core::mem::size_of::<Table>()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn hospes_trap_hit(t: &Table, pc: usize) -> bool {
        t.hit(pc)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn hospes_trap_add(t: &Table, start: usize, len: usize, user_max: usize) -> i32 {
        t.add(start, len, user_max)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn hospes_trap_lock(t: &Table) {
        t.lock()
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn hospes_trap_count(t: &Table) -> u32 {
        t.count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Table {
        Table { count: AtomicU32::new(0), locked: AtomicU32::new(0), ranges: core::array::from_fn(|_| Range { start: AtomicUsize::new(0), end: AtomicUsize::new(0) }) }
    }

    #[test]
    fn hits_only_inside() {
        let t = empty();
        assert!(!t.hit(0x1000));
        assert_eq!(t.add(0x1000, 0x100, 1 << 47), 0);
        // pc is just after the instruction: one past the range's last byte still hits.
        assert!(t.hit(0x1001));
        assert!(t.hit(0x1100));
        assert!(!t.hit(0x1000));
        assert!(!t.hit(0x1101));
        assert!(!t.hit(0));
    }

    #[test]
    fn bad_ranges_refused() {
        let t = empty();
        assert_eq!(t.add(0x1000, 0, 1 << 47), EINVAL);
        assert_eq!(t.add(usize::MAX - 1, 4, usize::MAX), EINVAL);
        assert_eq!(t.add(1 << 47, 0x1000, 1 << 47), EINVAL);
        for i in 0..MAX_RANGES {
            assert_eq!(t.add(0x1000 * (i + 1), 0x10, 1 << 47), 0);
        }
        assert_eq!(t.add(0x100000, 0x10, 1 << 47), ENOSPC);
        assert_eq!(t.count(), MAX_RANGES as u32);
    }

    #[test]
    fn locked_table_takes_no_more() {
        let t = empty();
        assert_eq!(t.add(0x1000, 0x10, 1 << 47), 0);
        t.lock();
        assert_eq!(t.add(0x2000, 0x10, 1 << 47), EPERM);
        assert!(t.hit(0x1008));
    }
}
