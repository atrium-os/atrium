//! Per-page analysis: what a compressed-RAM store would do with each page, and
//! what it would cost in physical bytes under different allocators.
//!
//! The store semantics mirror `atrium_zram.ko` (`zram_store`):
//!   * a same-filled page (all one byte, including all-zero) is kept as a tag,
//!     0 data bytes;
//!   * otherwise the page is compressed, and if the result is not smaller than a
//!     page it is stored verbatim (RAW, 4096 bytes).
//!
//! The allocator models answer "what does storing an N-byte object really
//! cost":
//!   * `ideal`    — exactly N bytes (an upper bound on what any allocator can do);
//!   * `packed`   — N rounded up to a 32-byte class, the zsmalloc / macOS
//!                  compressor-segment shape (objects packed into shared pages);
//!   * `malloc9`  — N rounded up to the FreeBSD `malloc(9)` zone it lands in,
//!                  which is what `atrium_zram.ko` does today (one `malloc` per
//!                  page). Anything over 2048 bytes lands in the 4096 zone, so
//!                  such a page saves nothing at all.

use std::collections::HashSet;
use std::time::Instant;

pub const PAGE: usize = 4096;

/// FreeBSD `malloc(9)` small-object zones (`kmemzones[]` in kern_malloc.c on
/// FreeBSD 13/14). A request is served from the smallest zone that fits.
pub const MALLOC9_ZONES: [usize; 10] = [16, 32, 64, 128, 256, 384, 512, 1024, 2048, 4096];

/// Granularity of the packed-allocator model.
pub const PACKED_CLASS: usize = 32;

/// Histogram bucket width for compressed sizes; the last bucket is RAW.
pub const HIST_BUCKET: usize = 256;
pub const HIST_LEN: usize = PAGE / HIST_BUCKET + 1;

pub fn malloc9_cost(n: usize) -> usize {
    MALLOC9_ZONES.iter().copied().find(|&z| z >= n).unwrap_or(PAGE)
}

pub fn packed_cost(n: usize) -> usize {
    n.div_ceil(PACKED_CLASS) * PACKED_CLASS
}

/// Returns the fill byte if every byte of the page is the same.
pub fn same_filled(p: &[u8]) -> Option<u8> {
    let b = p[0];
    let word = u64::from_ne_bytes([b; 8]);
    if p.chunks_exact(8).all(|c| u64::from_ne_bytes(c.try_into().unwrap()) == word) {
        Some(b)
    } else {
        None
    }
}

/// FNV-1a over 64-bit words: fast, and good enough to count duplicate pages
/// (a statistic, not an identity: a collision miscounts one page).
fn page_hash(p: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in p.chunks_exact(8) {
        h ^= u64::from_ne_bytes(c.try_into().unwrap());
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Lz4,
    #[cfg(feature = "zstd")]
    Zstd1,
    #[cfg(feature = "zstd")]
    Zstd3,
}

impl Codec {
    pub fn all() -> Vec<Codec> {
        vec![
            Codec::Lz4,
            #[cfg(feature = "zstd")]
            Codec::Zstd1,
            #[cfg(feature = "zstd")]
            Codec::Zstd3,
        ]
    }
    pub fn name(self) -> &'static str {
        match self {
            Codec::Lz4 => "lz4",
            #[cfg(feature = "zstd")]
            Codec::Zstd1 => "zstd-1",
            #[cfg(feature = "zstd")]
            Codec::Zstd3 => "zstd-3",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CodecStats {
    /// Pages that went through the codec (i.e. not same-filled).
    pub pages: u64,
    /// Pages that did not shrink below a page and are stored RAW.
    pub raw: u64,
    pub bytes_ideal: u64,
    pub bytes_packed: u64,
    pub bytes_malloc9: u64,
    pub comp_ns: u64,
    pub decomp_ns: u64,
    pub hist: [u64; HIST_LEN],
    /// Round-trip mismatches. Must be 0; anything else is a codec bug.
    pub errors: u64,
}

impl CodecStats {
    fn add(&mut self, clen: usize, comp_ns: u64, decomp_ns: u64) {
        self.pages += 1;
        self.comp_ns += comp_ns;
        self.decomp_ns += decomp_ns;
        if clen >= PAGE {
            self.raw += 1;
            self.bytes_ideal += PAGE as u64;
            self.bytes_packed += PAGE as u64;
            self.bytes_malloc9 += PAGE as u64;
            self.hist[HIST_LEN - 1] += 1;
        } else {
            self.bytes_ideal += clen as u64;
            self.bytes_packed += packed_cost(clen) as u64;
            self.bytes_malloc9 += malloc9_cost(clen) as u64;
            self.hist[clen / HIST_BUCKET] += 1;
        }
    }

    pub fn merge(&mut self, o: &CodecStats) {
        self.pages += o.pages;
        self.raw += o.raw;
        self.bytes_ideal += o.bytes_ideal;
        self.bytes_packed += o.bytes_packed;
        self.bytes_malloc9 += o.bytes_malloc9;
        self.comp_ns += o.comp_ns;
        self.decomp_ns += o.decomp_ns;
        self.errors += o.errors;
        for (a, b) in self.hist.iter_mut().zip(o.hist.iter()) {
            *a += b;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Anonymous resident pages examined.
    pub pages: u64,
    pub zero: u64,
    /// Same-filled with a non-zero byte.
    pub same: u64,
    /// Not same-filled, and identical to a page already seen in this snapshot
    /// (any process). zram does not dedup; this sizes what dedup would add.
    pub dup: u64,
    pub codecs: Vec<(Codec, CodecStats)>,
}

impl Stats {
    pub fn new() -> Stats {
        Stats { codecs: Codec::all().into_iter().map(|c| (c, CodecStats::default())).collect(), ..Default::default() }
    }

    pub fn merge(&mut self, o: &Stats) {
        self.pages += o.pages;
        self.zero += o.zero;
        self.same += o.same;
        self.dup += o.dup;
        for ((_, a), (_, b)) in self.codecs.iter_mut().zip(o.codecs.iter()) {
            a.merge(b);
        }
    }

    pub fn logical_bytes(&self) -> u64 {
        self.pages * PAGE as u64
    }
}

pub struct Analyzer {
    seen: HashSet<u64>,
    lz4_out: Vec<u8>,
    back: Vec<u8>,
    #[cfg(feature = "zstd")]
    z1: zstd::bulk::Compressor<'static>,
    #[cfg(feature = "zstd")]
    z3: zstd::bulk::Compressor<'static>,
    #[cfg(feature = "zstd")]
    zd: zstd::bulk::Decompressor<'static>,
    #[cfg(feature = "zstd")]
    zout: Vec<u8>,
}

impl Analyzer {
    pub fn new() -> Analyzer {
        Analyzer {
            seen: HashSet::new(),
            lz4_out: vec![0; lz4_flex::block::get_maximum_output_size(PAGE)],
            back: vec![0; PAGE],
            #[cfg(feature = "zstd")]
            z1: zstd::bulk::Compressor::new(1).expect("zstd ctx"),
            #[cfg(feature = "zstd")]
            z3: zstd::bulk::Compressor::new(3).expect("zstd ctx"),
            #[cfg(feature = "zstd")]
            zd: zstd::bulk::Decompressor::new().expect("zstd dctx"),
            #[cfg(feature = "zstd")]
            zout: Vec::with_capacity(zstd::zstd_safe::compress_bound(PAGE)),
        }
    }

    /// Account one resident anonymous page.
    pub fn page(&mut self, st: &mut Stats, p: &[u8]) {
        debug_assert_eq!(p.len(), PAGE);
        st.pages += 1;
        if let Some(b) = same_filled(p) {
            if b == 0 {
                st.zero += 1;
            } else {
                st.same += 1;
            }
            return;
        }
        if !self.seen.insert(page_hash(p)) {
            st.dup += 1;
        }
        for (codec, cs) in st.codecs.iter_mut() {
            match *codec {
                Codec::Lz4 => {
                    let t0 = Instant::now();
                    let n = lz4_flex::block::compress_into(p, &mut self.lz4_out).unwrap_or(PAGE);
                    let t1 = Instant::now();
                    let ok = matches!(lz4_flex::block::decompress_into(&self.lz4_out[..n], &mut self.back), Ok(PAGE))
                        && self.back == p;
                    let t2 = Instant::now();
                    if !ok {
                        cs.errors += 1;
                    }
                    cs.add(n, nanos(t0, t1), nanos(t1, t2));
                }
                #[cfg(feature = "zstd")]
                Codec::Zstd1 | Codec::Zstd3 => {
                    let c = if *codec == Codec::Zstd1 { &mut self.z1 } else { &mut self.z3 };
                    self.zout.clear();
                    let t0 = Instant::now();
                    let n = c.compress_to_buffer(p, &mut self.zout).unwrap_or(PAGE);
                    let t1 = Instant::now();
                    let ok = matches!(self.zd.decompress_to_buffer(&self.zout[..], &mut self.back[..]), Ok(PAGE))
                        && self.back == p;
                    let t2 = Instant::now();
                    if !ok {
                        cs.errors += 1;
                    }
                    cs.add(n, nanos(t0, t1), nanos(t1, t2));
                }
            }
        }
    }
}

fn nanos(a: Instant, b: Instant) -> u64 {
    b.duration_since(a).as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malloc9_rounds_up_to_zone() {
        assert_eq!(malloc9_cost(1), 16);
        assert_eq!(malloc9_cost(300), 384);
        assert_eq!(malloc9_cost(1025), 2048);
        assert_eq!(malloc9_cost(2049), 4096);
        assert_eq!(malloc9_cost(4096), 4096);
    }

    #[test]
    fn packed_rounds_to_class() {
        assert_eq!(packed_cost(1), 32);
        assert_eq!(packed_cost(32), 32);
        assert_eq!(packed_cost(2049), 2080);
    }

    #[test]
    fn classifies_zero_same_dup_and_raw() {
        let mut a = Analyzer::new();
        let mut st = Stats::new();
        a.page(&mut st, &[0u8; PAGE]);
        a.page(&mut st, &[7u8; PAGE]);
        let mut text = vec![0u8; PAGE];
        for (i, b) in text.iter_mut().enumerate() {
            *b = b"memory pressure "[i % 16];
        }
        a.page(&mut st, &text);
        a.page(&mut st, &text);
        // xorshift noise: incompressible.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let noise: Vec<u8> = (0..PAGE)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        a.page(&mut st, &noise);

        assert_eq!((st.pages, st.zero, st.same, st.dup), (5, 1, 1, 1));
        for (c, cs) in &st.codecs {
            assert_eq!(cs.errors, 0, "{}", c.name());
            assert_eq!(cs.pages, 3, "{}", c.name());
            assert_eq!(cs.raw, 1, "{} should store noise RAW", c.name());
            assert!(cs.bytes_ideal < 2 * PAGE as u64, "{}: text must compress", c.name());
            assert!(cs.bytes_ideal <= cs.bytes_packed && cs.bytes_packed <= cs.bytes_malloc9);
        }
    }
}
