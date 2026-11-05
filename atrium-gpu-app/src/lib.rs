//! atrium-gpu-app — the broker side of the app GPU path (F6).
//!
//! An app that brings its own shaders compiles them in its own jail and hands
//! the broker two things through sealed memory slots (docs/spec/gpu-app-path-f6.md):
//!
//! - **code**: a 64-byte [`CodeHeader`] followed by RDNA4 machine code;
//! - **work**: a list of fixed-size [`OP_RECORD`] records from a closed set
//!   (bind code, set user SGPRs, dispatch, barrier).
//!
//! The machine code is untrusted. It runs under the app's own VMID, in a VM
//! holding only the app's buffers, so the GPU MMU is what confines it. This
//! crate makes sure nothing else the app sends can do more:
//!
//! - every field of the header is checked against hardware limits before it
//!   reaches a register;
//! - the app never writes a PM4 packet. [`build`] writes all of them from
//!   checked values, so app bytes reach the CP only as register values the
//!   broker chose to program (SH state for one compute dispatch);
//! - malformed input is refused with a [`Refusal`]; nothing here panics on
//!   app input, because a panic in the broker is a desktop-wide outage.
//!
//! The broker appends its own end-of-pipe fence after the blob and submits it
//! into the app's VM; the driver's hang watchdog bounds how long it can run.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use atrium_rdna4_cmd::{emit_dispatch_at, Blob, Dispatch, Scratch};

/// "AGPC": atrium GPU program code.
pub const CODE_MAGIC: [u8; 4] = *b"AGPC";
/// The only header version so far.
pub const CODE_VERSION: u32 = 1;
/// Architecture ids, as the GPU channel's `HELLO` reports the broker's GPU
/// and each code header names the GPU it was built for. They are the
/// driver's CAP_ISA families (`ATRIUM_GPU_ISA_*` in gpu_abi_v2.h). The record set and
/// the channel are the same for every architecture; the code header layout
/// and the packet builder are per architecture (see [`session::Arch`]).
pub const ARCH_RDNA4: u32 = 1;
/// Size of [`CodeHeader`] on the wire.
pub const CODE_HEADER_SIZE: usize = 64;
/// Size of one work record on the wire.
pub const OP_RECORD: usize = 80;

/// One app's GPU context, broker side (F6 G2).
pub mod session;

/// Hardware and policy limits the broker enforces.
pub mod limits {
    /// Largest code body accepted (bytes).
    pub const MAX_CODE_BYTES: u32 = 1 << 20;
    /// VGPRs per lane a wave may ask for (RDNA4 wave32 VGPR file).
    pub const MAX_VGPRS: u32 = 256;
    /// User SGPRs the dispatch can preload (COMPUTE_USER_DATA_0..15).
    pub const MAX_USER_SGPRS: u32 = 16;
    /// LDS per workgroup (bytes).
    pub const MAX_LDS_BYTES: u32 = 64 * 1024;
    /// Threads per workgroup.
    pub const MAX_THREADS: u32 = 1024;
    /// Per-dimension thread count limit (the local id packs 10 bits per axis).
    pub const MAX_THREADS_PER_AXIS: u32 = 1024;
    /// Scratch dwords per lane.
    pub const MAX_SCRATCH_DWORDS: u32 = 4096;
    /// Workgroups in one dispatch (product of the three counts).
    pub const MAX_GROUPS: u64 = 1 << 24;
    /// Records in one submission.
    pub const MAX_RECORDS: usize = 256;
    /// Dispatches in one submission.
    pub const MAX_DISPATCHES: usize = 64;
}

/// Why the broker refused something an app sent. The index, where present, is
/// the record number in the submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // fields: `index` is the record number, the rest as named
pub enum Refusal {
    /// The slot is shorter than what it claims to hold.
    Truncated,
    /// Wrong magic.
    BadMagic,
    /// Unknown header version.
    BadVersion(u32),
    /// Code built for another GPU architecture than the broker's.
    WrongArch(u32),
    /// A reserved field or flag bit is not zero.
    Reserved,
    /// Code size is zero, not a multiple of 4, or over the limit.
    CodeSize(u32),
    /// VGPR count out of range.
    Vgprs(u32),
    /// User SGPR count out of range.
    UserSgprs(u32),
    /// LDS size over the limit.
    Lds(u32),
    /// Thread counts zero or over the limit.
    Threads([u32; 3]),
    /// Scratch request over the limit.
    Scratch(u32),
    /// The record list is empty, too long, or not a whole number of records.
    RecordCount(usize),
    /// Unknown opcode.
    UnknownOp { index: usize, op: u32 },
    /// A record's unused bytes are not zero.
    RecordPadding { index: usize },
    /// The code id is not one of this app's.
    UnknownCode { index: usize, code: u32 },
    /// A dispatch with no code bound.
    NoCode { index: usize },
    /// USER_SGPRS count out of range.
    SgprCount { index: usize, count: u32 },
    /// The user SGPRs set do not match what the bound code declares.
    SgprMismatch { index: usize, set: u32, declared: u32 },
    /// Workgroup counts zero, overflowing, or over the limit.
    Groups { index: usize },
    /// Too many dispatches in one submission.
    TooManyDispatches,
}

/// What an app declares about one piece of code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeHeader {
    /// Bytes of machine code after the header.
    pub code_bytes: u32,
    /// VGPRs per lane.
    pub vgprs: u32,
    /// User SGPRs the code expects preloaded (s0..).
    pub user_sgprs: u32,
    /// LDS per workgroup, bytes.
    pub lds_bytes: u32,
    /// Scratch dwords per lane (0 = none).
    pub scratch_dwords: u32,
    /// Threads per workgroup.
    pub threads: [u32; 3],
    /// Wave32 (else wave64).
    pub wave32: bool,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl CodeHeader {
    /// Wire layout, little-endian:
    ///
    /// | off | field |
    /// |---|---|
    /// | 0 | magic `AGPC` |
    /// | 4 | version (1) |
    /// | 8 | code_bytes |
    /// | 12 | vgprs |
    /// | 16 | user_sgprs |
    /// | 20 | lds_bytes |
    /// | 24 | scratch_dwords |
    /// | 28 | threads x, y, z |
    /// | 40 | flags: bit 0 wave32, the rest zero |
    /// | 44 | architecture ([`ARCH_RDNA4`]) |
    /// | 48..64 | reserved, zero |
    ///
    /// Parses and checks a code slot, returning the header and the code.
    pub fn parse(slot: &[u8]) -> Result<(CodeHeader, &[u8]), Refusal> {
        use limits::*;
        if slot.len() < CODE_HEADER_SIZE {
            return Err(Refusal::Truncated);
        }
        if slot[0..4] != CODE_MAGIC {
            return Err(Refusal::BadMagic);
        }
        let version = le32(slot, 4);
        if version != CODE_VERSION {
            return Err(Refusal::BadVersion(version));
        }
        let flags = le32(slot, 40);
        if le32(slot, 44) != ARCH_RDNA4 {
            return Err(Refusal::WrongArch(le32(slot, 44)));
        }
        if flags & !1 != 0 || slot[48..64].iter().any(|&b| b != 0) {
            return Err(Refusal::Reserved);
        }
        let h = CodeHeader {
            code_bytes: le32(slot, 8),
            vgprs: le32(slot, 12),
            user_sgprs: le32(slot, 16),
            lds_bytes: le32(slot, 20),
            scratch_dwords: le32(slot, 24),
            threads: [le32(slot, 28), le32(slot, 32), le32(slot, 36)],
            wave32: flags & 1 != 0,
        };
        if h.code_bytes == 0 || h.code_bytes % 4 != 0 || h.code_bytes > MAX_CODE_BYTES {
            return Err(Refusal::CodeSize(h.code_bytes));
        }
        if slot.len() - CODE_HEADER_SIZE < h.code_bytes as usize {
            return Err(Refusal::Truncated);
        }
        if h.vgprs == 0 || h.vgprs > MAX_VGPRS {
            return Err(Refusal::Vgprs(h.vgprs));
        }
        if h.user_sgprs > MAX_USER_SGPRS {
            return Err(Refusal::UserSgprs(h.user_sgprs));
        }
        if h.lds_bytes > MAX_LDS_BYTES {
            return Err(Refusal::Lds(h.lds_bytes));
        }
        let [x, y, z] = h.threads;
        if x == 0 || y == 0 || z == 0
            || x > MAX_THREADS_PER_AXIS || y > MAX_THREADS_PER_AXIS || z > MAX_THREADS_PER_AXIS
            || (x as u64) * (y as u64) * (z as u64) > MAX_THREADS as u64
        {
            return Err(Refusal::Threads(h.threads));
        }
        if h.scratch_dwords > MAX_SCRATCH_DWORDS {
            return Err(Refusal::Scratch(h.scratch_dwords));
        }
        let code = &slot[CODE_HEADER_SIZE..CODE_HEADER_SIZE + h.code_bytes as usize];
        Ok((h, code))
    }

    /// Encode (for apps and tests).
    pub fn encode(&self, code: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; CODE_HEADER_SIZE];
        v[0..4].copy_from_slice(&CODE_MAGIC);
        let put = |v: &mut Vec<u8>, at: usize, x: u32| v[at..at + 4].copy_from_slice(&x.to_le_bytes());
        put(&mut v, 4, CODE_VERSION);
        put(&mut v, 8, self.code_bytes);
        put(&mut v, 12, self.vgprs);
        put(&mut v, 16, self.user_sgprs);
        put(&mut v, 20, self.lds_bytes);
        put(&mut v, 24, self.scratch_dwords);
        put(&mut v, 28, self.threads[0]);
        put(&mut v, 32, self.threads[1]);
        put(&mut v, 36, self.threads[2]);
        put(&mut v, 40, self.wave32 as u32);
        put(&mut v, 44, ARCH_RDNA4);
        v.extend_from_slice(code);
        v
    }

    /// Lanes per wave.
    pub fn lanes(&self) -> u32 {
        if self.wave32 { 32 } else { 64 }
    }
}

/// Where the broker placed a piece of the app's code, in the app's own VM.
#[derive(Debug, Clone, Copy)]
pub struct CodeBinding {
    /// The checked header.
    pub header: CodeHeader,
    /// GPU VA of the code; the broker allocates it 256-byte aligned.
    pub va: u64,
    /// The scratch ring the broker allocated for it in the app's VM, if the
    /// header asked for scratch.
    pub scratch: Option<Scratch>,
}

/// Record opcodes (the closed set).
pub mod op {
    /// `[op, code_id]`: later dispatches run this code.
    pub const BIND_CODE: u32 = 1;
    /// `[op, count, values[16]]`: user data words for later dispatches
    /// (on RDNA4, preloaded into s0.. as user SGPRs).
    pub const USER_SGPRS: u32 = 2;
    /// The architecture-neutral name for [`USER_SGPRS`].
    pub const USER_DATA: u32 = USER_SGPRS;
    /// `[op, groups x y z, start x y z]`.
    pub const DISPATCH: u32 = 3;
    /// `[op]`: wait for earlier dispatches and make their writes visible.
    pub const BARRIER: u32 = 4;
}

/// Encode a record (for apps and tests).
pub fn record(op: u32, words: &[u32]) -> [u8; OP_RECORD] {
    let mut r = [0u8; OP_RECORD];
    r[0..4].copy_from_slice(&op.to_le_bytes());
    for (i, w) in words.iter().enumerate().take(OP_RECORD / 4 - 1) {
        r[4 + i * 4..8 + i * 4].copy_from_slice(&w.to_le_bytes());
    }
    r
}

fn padding_clear(r: &[u8], used_words: usize) -> bool {
    r[4 + used_words * 4..].iter().all(|&b| b == 0)
}

/// Check a submission and build its command blob. `code` resolves the app's
/// code ids (its own table only). The blob starts by making earlier writes
/// visible and ends with a CS partial flush; the caller appends its fence.
pub fn build(
    records: &[u8],
    code: &dyn Fn(u32) -> Option<CodeBinding>,
) -> Result<Blob, Refusal> {
    use limits::*;
    if records.is_empty() || records.len() % OP_RECORD != 0
        || records.len() / OP_RECORD > MAX_RECORDS
    {
        return Err(Refusal::RecordCount(records.len()));
    }
    let mut blob = Blob::compute();
    blob.acquire_all();
    let mut bound: Option<CodeBinding> = None;
    let mut sgprs: Option<Vec<u32>> = None;
    let mut dispatches = 0usize;

    for (index, r) in records.chunks_exact(OP_RECORD).enumerate() {
        let w = |i: usize| le32(r, 4 + i * 4);
        match le32(r, 0) {
            op::BIND_CODE => {
                if !padding_clear(r, 1) {
                    return Err(Refusal::RecordPadding { index });
                }
                let id = w(0);
                bound = Some(code(id).ok_or(Refusal::UnknownCode { index, code: id })?);
            }
            op::USER_SGPRS => {
                if !padding_clear(r, 17) {
                    return Err(Refusal::RecordPadding { index });
                }
                let count = w(0);
                if count > MAX_USER_SGPRS {
                    return Err(Refusal::SgprCount { index, count });
                }
                // Unused value slots must be zero too.
                if (count as usize..16).any(|i| w(1 + i) != 0) {
                    return Err(Refusal::RecordPadding { index });
                }
                sgprs = Some((0..count as usize).map(|i| w(1 + i)).collect());
            }
            op::DISPATCH => {
                if !padding_clear(r, 6) {
                    return Err(Refusal::RecordPadding { index });
                }
                let b = bound.ok_or(Refusal::NoCode { index })?;
                let set = sgprs.as_ref().map_or(0, |s| s.len() as u32);
                if set != b.header.user_sgprs {
                    return Err(Refusal::SgprMismatch {
                        index, set, declared: b.header.user_sgprs,
                    });
                }
                let groups = [w(0), w(1), w(2)];
                let start = [w(3), w(4), w(5)];
                // Checked: three u32 counts can overflow even a u64 product.
                let total = groups.iter().try_fold(1u64, |t, &g| t.checked_mul(g as u64));
                if groups.contains(&0) || total.map_or(true, |t| t > MAX_GROUPS)
                    || (0..3).any(|i| start[i].checked_add(groups[i]).is_none())
                {
                    return Err(Refusal::Groups { index });
                }
                dispatches += 1;
                if dispatches > MAX_DISPATCHES {
                    return Err(Refusal::TooManyDispatches);
                }
                let d = Dispatch {
                    shader_va: b.va,
                    vgprs: b.header.vgprs,
                    user_sgprs: sgprs.clone().unwrap_or_default(),
                    threads: b.header.threads,
                    groups,
                    wave32: b.header.wave32,
                    lds_bytes: b.header.lds_bytes,
                    scratch: b.scratch,
                };
                emit_dispatch_at(&mut blob, &d, start);
            }
            op::BARRIER => {
                if !padding_clear(r, 0) {
                    return Err(Refusal::RecordPadding { index });
                }
                blob.cs_partial_flush();
                blob.acquire_all();
            }
            other => return Err(Refusal::UnknownOp { index, op: other }),
        }
    }
    blob.cs_partial_flush();
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr() -> CodeHeader {
        CodeHeader {
            code_bytes: 8, vgprs: 16, user_sgprs: 2, lds_bytes: 0,
            scratch_dwords: 0, threads: [64, 1, 1], wave32: true,
        }
    }

    fn binding(h: CodeHeader) -> CodeBinding {
        CodeBinding { header: h, va: 0x1_0000_0000, scratch: None }
    }

    #[test]
    fn header_round_trips() {
        let code = [0u8; 8];
        let slot = hdr().encode(&code);
        let (h, c) = CodeHeader::parse(&slot).unwrap();
        assert_eq!(h, hdr());
        assert_eq!(c, &code);
    }

    #[test]
    fn header_refusals() {
        let ok = hdr().encode(&[0u8; 8]);
        let mut bad = ok.clone();
        bad[0] = b'X';
        assert_eq!(CodeHeader::parse(&bad), Err(Refusal::BadMagic));
        let mut bad = ok.clone();
        bad[60] = 1;
        assert_eq!(CodeHeader::parse(&bad), Err(Refusal::Reserved));
        let mut bad = ok.clone();
        bad[40] = 2;
        assert_eq!(CodeHeader::parse(&bad), Err(Refusal::Reserved));
        assert_eq!(CodeHeader::parse(&ok[..ok.len() - 1]), Err(Refusal::Truncated));
        let cases: [(fn(&mut CodeHeader), Refusal); 6] = [
            (|h| h.code_bytes = 6, Refusal::CodeSize(6)),
            (|h| h.vgprs = 0, Refusal::Vgprs(0)),
            (|h| h.user_sgprs = 17, Refusal::UserSgprs(17)),
            (|h| h.lds_bytes = 65 * 1024, Refusal::Lds(65 * 1024)),
            (|h| h.threads = [32, 32, 2], Refusal::Threads([32, 32, 2])),
            (|h| h.scratch_dwords = 5000, Refusal::Scratch(5000)),
        ];
        for (f, want) in cases {
            let mut h = hdr();
            f(&mut h);
            let slot = h.encode(&[0u8; 8]);
            assert_eq!(CodeHeader::parse(&slot).map(|_| ()), Err(want));
        }
    }

    #[test]
    fn builds_the_same_packets_as_a_direct_dispatch() {
        let recs = [
            record(op::BIND_CODE, &[7]),
            record(op::USER_SGPRS, &[2, 0xdead, 0xbeef]),
            record(op::DISPATCH, &[4, 2, 1, 0, 0, 0]),
        ]
        .concat();
        let blob = build(&recs, &|id| (id == 7).then(|| binding(hdr()))).unwrap();
        let mut want = Blob::compute();
        want.acquire_all();
        emit_dispatch_at(&mut want, &Dispatch {
            shader_va: 0x1_0000_0000, vgprs: 16, user_sgprs: vec![0xdead, 0xbeef],
            threads: [64, 1, 1], groups: [4, 2, 1], wave32: true, lds_bytes: 0,
            scratch: None,
        }, [0; 3]);
        want.cs_partial_flush();
        assert_eq!(blob.dw, want.dw);
    }

    #[test]
    fn record_refusals() {
        let code = |id: u32| (id == 1).then(|| binding(hdr()));
        let sg = record(op::USER_SGPRS, &[2, 1, 2]);
        let bind = record(op::BIND_CODE, &[1]);
        let disp = |g: [u32; 3], s: [u32; 3]| record(op::DISPATCH, &[g[0], g[1], g[2], s[0], s[1], s[2]]);
        let go = |rs: &[[u8; OP_RECORD]]| build(&rs.concat(), &code).map(|_| ());
        assert_eq!(go(&[]), Err(Refusal::RecordCount(0)));
        assert_eq!(go(&[record(9, &[])]), Err(Refusal::UnknownOp { index: 0, op: 9 }));
        assert_eq!(go(&[record(op::BIND_CODE, &[2])]),
            Err(Refusal::UnknownCode { index: 0, code: 2 }));
        assert_eq!(go(&[disp([1, 1, 1], [0; 3])]), Err(Refusal::NoCode { index: 0 }));
        assert_eq!(go(&[bind, disp([1, 1, 1], [0; 3])]),
            Err(Refusal::SgprMismatch { index: 1, set: 0, declared: 2 }));
        assert_eq!(go(&[bind, sg, disp([0, 1, 1], [0; 3])]), Err(Refusal::Groups { index: 2 }));
        assert_eq!(go(&[bind, sg, disp([1 << 13, 1 << 12, 1], [0; 3])]),
            Err(Refusal::Groups { index: 2 }));
        assert_eq!(go(&[bind, sg, disp([2, 1, 1], [u32::MAX, 0, 0])]),
            Err(Refusal::Groups { index: 2 }));
        assert_eq!(go(&[record(op::USER_SGPRS, &[17])]),
            Err(Refusal::SgprCount { index: 0, count: 17 }));
        assert_eq!(go(&[record(op::USER_SGPRS, &[1, 5, 6])]),
            Err(Refusal::RecordPadding { index: 0 }));
        assert_eq!(go(&[record(op::BARRIER, &[1])]), Err(Refusal::RecordPadding { index: 0 }));
        let many: Vec<_> = std::iter::once(bind).chain(std::iter::once(sg))
            .chain(std::iter::repeat(disp([1, 1, 1], [0; 3])).take(65)).collect();
        assert_eq!(go(&many), Err(Refusal::TooManyDispatches));
        let mut ragged = [bind, sg].concat();
        ragged.push(0);
        assert!(matches!(build(&ragged, &code), Err(Refusal::RecordCount(_))));
    }

    /// Random and mutated input never panics (a panic in the broker is a
    /// desktop-wide outage).
    #[test]
    fn hostile_bytes_never_panic() {
        let mut s: u64 = std::env::var("APP_FUZZ_SEED").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(0x9e37_79b9_7f4a_7c15);
        let iters: usize = std::env::var("APP_FUZZ_ITERS").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(20_000);
        let mut next = move || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
        let good_slot = hdr().encode(&[0u8; 8]);
        let good_recs = [
            record(op::BIND_CODE, &[1]),
            record(op::USER_SGPRS, &[2, 1, 2]),
            record(op::DISPATCH, &[4, 2, 1, 0, 0, 0]),
            record(op::BARRIER, &[]),
        ].concat();
        let code = |id: u32| (id < 4).then(|| binding(hdr()));
        let (mut ok_h, mut ok_b) = (0, 0);
        for _ in 0..iters {
            let mut slot = good_slot.clone();
            let mut recs = good_recs.clone();
            for _ in 0..(next() % 4 + 1) {
                let i = (next() as usize) % slot.len();
                slot[i] = next() as u8;
                let i = (next() as usize) % recs.len();
                recs[i] = next() as u8;
            }
            if next() % 8 == 0 {
                recs.truncate((next() as usize) % (recs.len() + 1));
            }
            ok_h += CodeHeader::parse(&slot).is_ok() as u32;
            ok_b += build(&recs, &code).is_ok() as u32;
        }
        // Mutation leaves some inputs valid, so both accept paths are reached.
        assert!(ok_h > 0 && ok_b > 0, "accept paths not reached: {ok_h} {ok_b}");
    }
}
