//! The Hospes allocator (docs/spec/hospes.md §4): one heap for our code and, from H2, the
//! library's `malloc` family, so one limit covers the whole process.
//!
//! Small blocks come from 64 KiB chunks, each serving one size class; large blocks get their
//! own mapping. Every chunk and large mapping is 64 KiB-aligned and starts with a header, so
//! `free(p)` finds the header at `p & !0xffff` without being told the size, as `free` must.
use crate::lock::Mutex;
use crate::sys;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

const CHUNK: usize = 64 * 1024;
const HDR: usize = 64;
const MAGIC_SMALL: u32 = 0x484f_5331; // "HOS1"
const MAGIC_LARGE: u32 = 0x484f_534c; // "HOSL"

const CLASSES: [usize; 20] = [
    16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384,
];
/// Largest alignment a large block can be given (its data must start in the first chunk).
pub const MAX_ALIGN: usize = 4096;

#[repr(C)]
struct ChunkHdr {
    magic: u32,
    class: u32,
    /// Large blocks: total mapping length; small chunks: unused.
    map_len: usize,
    /// Large blocks: usable bytes.
    usable: usize,
}

struct Free {
    next: *mut Free,
}

struct State {
    free: [*mut Free; CLASSES.len()],
}
unsafe impl Send for State {}

static STATE: Mutex<State> = Mutex::new(State { free: [ptr::null_mut(); CLASSES.len()] });
static MAPPED: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Cap total heap mappings (bytes). Allocation fails beyond it.
pub fn set_limit(bytes: usize) {
    LIMIT.store(bytes, Ordering::Relaxed);
}
pub fn mapped() -> usize {
    MAPPED.load(Ordering::Relaxed)
}

/// The largest power of two dividing `x`, capped at 4096.
fn natural(x: usize) -> usize {
    (1usize << x.trailing_zeros()).min(4096)
}

fn first_block(class: usize) -> usize {
    let n = natural(class);
    (HDR + n - 1) & !(n - 1)
}

fn class_for(size: usize, align: usize) -> Option<usize> {
    CLASSES.iter().position(|&c| c >= size && natural(c) >= align)
}

fn map_chunk(len: usize) -> Option<usize> {
    let len = sys::page_up(len);
    if MAPPED.fetch_add(len, Ordering::Relaxed) + len > LIMIT.load(Ordering::Relaxed) {
        MAPPED.fetch_sub(len, Ordering::Relaxed);
        return None;
    }
    match sys::mmap_aligned(len, CHUNK, sys::PROT_READ | sys::PROT_WRITE) {
        Ok(a) => Some(a),
        Err(_) => {
            MAPPED.fetch_sub(len, Ordering::Relaxed);
            None
        }
    }
}

/// Allocate `size` bytes aligned to `align` (a power of two). Null on failure.
pub fn alloc(size: usize, align: usize) -> *mut u8 {
    let size = size.max(1);
    let align = align.max(16);
    if align > MAX_ALIGN || size > isize::MAX as usize / 2 {
        return ptr::null_mut();
    }
    if let Some(ci) = class_for(size, align) {
        let class = CLASSES[ci];
        let mut st = STATE.lock();
        if st.free[ci].is_null() {
            let Some(c) = map_chunk(CHUNK) else { return ptr::null_mut() };
            unsafe {
                ptr::write(c as *mut ChunkHdr, ChunkHdr { magic: MAGIC_SMALL, class: ci as u32, map_len: CHUNK, usable: class });
                let mut p = c + first_block(class);
                let mut head: *mut Free = ptr::null_mut();
                // Thread blocks in address order.
                let mut tail: *mut *mut Free = &mut head;
                while p + class <= c + CHUNK {
                    let f = p as *mut Free;
                    (*f).next = ptr::null_mut();
                    *tail = f;
                    tail = &mut (*f).next;
                    p += class;
                }
                st.free[ci] = head;
            }
        }
        let f = st.free[ci];
        unsafe { st.free[ci] = (*f).next };
        return f as *mut u8;
    }
    // Large: header in the first 64 bytes (or `align` bytes) of a dedicated mapping.
    let off = HDR.max(align);
    let Some(total) = size.checked_add(off) else { return ptr::null_mut() };
    let Some(c) = map_chunk(total) else { return ptr::null_mut() };
    let map_len = sys::page_up(total);
    unsafe {
        ptr::write(c as *mut ChunkHdr, ChunkHdr { magic: MAGIC_LARGE, class: u32::MAX, map_len, usable: map_len - off });
    }
    (c + off) as *mut u8
}

unsafe fn hdr(p: *mut u8) -> &'static ChunkHdr {
    let h = &*((p as usize & !(CHUNK - 1)) as *const ChunkHdr);
    if h.magic != MAGIC_SMALL && h.magic != MAGIC_LARGE {
        crate::hlog!("hospes: heap: free of a pointer we did not allocate: {:p}", p);
        sys::exit(134);
    }
    h
}

/// Free a block from [`alloc`]. Null is ignored.
pub unsafe fn free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    let h = hdr(p);
    if h.magic == MAGIC_SMALL {
        let ci = h.class as usize;
        let f = p as *mut Free;
        let mut st = STATE.lock();
        (*f).next = st.free[ci];
        st.free[ci] = f;
    } else {
        let base = p as usize & !(CHUNK - 1);
        let len = h.map_len;
        let _ = sys::munmap(base, len);
        MAPPED.fetch_sub(len, Ordering::Relaxed);
    }
}

/// Bytes usable at `p` (`malloc_usable_size`).
pub unsafe fn usable_size(p: *mut u8) -> usize {
    if p.is_null() {
        return 0;
    }
    let h = hdr(p);
    if h.magic == MAGIC_SMALL {
        CLASSES[h.class as usize]
    } else {
        h.usable
    }
}

pub unsafe fn realloc(p: *mut u8, new: usize, align: usize) -> *mut u8 {
    if p.is_null() {
        return alloc(new, align);
    }
    let have = usable_size(p);
    if new <= have && (p as usize) % align.max(16) == 0 {
        return p;
    }
    let q = alloc(new, align);
    if !q.is_null() {
        ptr::copy_nonoverlapping(p, q, have.min(new));
        free(p);
    }
    q
}

/// `GlobalAlloc` over the same heap.
pub struct Heap;

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        alloc(l.size(), l.align())
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = alloc(l.size(), l.align());
        if !p.is_null() {
            ptr::write_bytes(p, 0, l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
        free(p)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        realloc(p, new, l.align())
    }
}
