//! The unwinder for C++ exceptions thrown inside the library (libgcc_s's `_Unwind_*` ABI,
//! which Widevine's statically linked libc++abi calls). The `unwinding` crate implements
//! the two-phase DWARF unwind; we only tell it where frames are described: the library's
//! own `PT_GNU_EH_FRAME` (`.eh_frame_hdr`), registered at load, and our own executable's,
//! registered at init from the auxiliary vector: the unwinder starts inside its own
//! `_Unwind_RaiseException` and steps out of our frames first (hence `-C
//! force-unwind-tables` in scripts/hospes-build.sh). An exception that is not caught in the
//! library unwinds into our frames, which have no handler, so the search ends there and
//! libc++abi calls `std::terminate`, which ends the process (spec §4 "Crashes").
use core::sync::atomic::{AtomicUsize, Ordering};
use unwinding::custom_eh_frame_finder::{set_custom_eh_frame_finder, EhFrameFinder, FrameInfo, FrameInfoKind};

/// One loaded library: its text range, load base and `.eh_frame_hdr` address.
struct Lib {
    start: AtomicUsize,
    end: AtomicUsize,
    base: AtomicUsize,
    hdr: AtomicUsize,
}

struct Finder {
    n: AtomicUsize,
    libs: [Lib; 8],
}

static FINDER: Finder = Finder {
    n: AtomicUsize::new(0),
    libs: [const { Lib { start: AtomicUsize::new(0), end: AtomicUsize::new(0), base: AtomicUsize::new(0), hdr: AtomicUsize::new(0) } }; 8],
};

unsafe impl EhFrameFinder for Finder {
    fn find(&self, pc: usize) -> Option<FrameInfo> {
        let n = self.n.load(Ordering::Acquire).min(self.libs.len());
        self.libs[..n].iter().find_map(|l| {
            let hdr = l.hdr.load(Ordering::Acquire);
            (hdr != 0 && l.start.load(Ordering::Acquire) <= pc && pc < l.end.load(Ordering::Acquire))
                .then(|| FrameInfo { text_base: Some(l.base.load(Ordering::Acquire)), kind: FrameInfoKind::EhFrameHdr(hdr) })
        })
    }
}

/// Describe `[start, end)` (the library's text) with the `.eh_frame_hdr` at `hdr`.
pub(crate) fn register(start: usize, end: usize, base: usize, hdr: usize) {
    static ONCE: AtomicUsize = AtomicUsize::new(0);
    if ONCE.swap(1, Ordering::AcqRel) == 0 {
        let _ = set_custom_eh_frame_finder(&FINDER);
    }
    let i = FINDER.n.load(Ordering::Acquire);
    if i >= FINDER.libs.len() {
        crate::die("too many unwind ranges");
    }
    let l = &FINDER.libs[i];
    l.start.store(start, Ordering::Release);
    l.end.store(end, Ordering::Release);
    l.base.store(base, Ordering::Release);
    l.hdr.store(hdr, Ordering::Release);
    FINDER.n.store(i + 1, Ordering::Release);
}

/// Register our own executable's frames, found through `AT_PHDR` / `AT_PHNUM`.
pub(crate) fn register_self(auxv: &[(usize, usize)]) {
    const AT_PHDR: usize = 3;
    const AT_PHNUM: usize = 5;
    const PT_PHDR: u32 = 6;
    const PT_LOAD: u32 = 1;
    const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
    let get = |k| auxv.iter().find(|e| e.0 == k).map(|e| e.1);
    let (Some(phdr), Some(n)) = (get(AT_PHDR), get(AT_PHNUM)) else { return };
    #[repr(C)]
    struct Phdr {
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_paddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    }
    // Safety: the kernel's description of our own program headers.
    let ph = unsafe { core::slice::from_raw_parts(phdr as *const Phdr, n) };
    let base = ph.iter().find(|p| p.p_type == PT_PHDR).map_or(0, |p| phdr - p.p_vaddr as usize);
    let (mut lo, mut hi) = (usize::MAX, 0);
    for p in ph.iter().filter(|p| p.p_type == PT_LOAD && p.p_flags & 1 != 0) {
        lo = lo.min(base + p.p_vaddr as usize);
        hi = hi.max(base + (p.p_vaddr + p.p_memsz) as usize);
    }
    if let Some(h) = ph.iter().find(|p| p.p_type == PT_GNU_EH_FRAME) {
        if lo < hi {
            register(lo, hi, base, base + h.p_vaddr as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Unit tests run on std, whose panics now go through these `_Unwind_*` symbols too.
    #[test]
    fn std_panics_still_unwind() {
        let r = std::panic::catch_unwind(|| panic!("expected"));
        assert!(r.is_err());
    }
}
