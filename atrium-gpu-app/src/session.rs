//! One app's GPU context, broker side (F6 G2).
//!
//! A [`Session`] holds what the broker keeps for one app connection: the
//! app's code objects and buffers in a VM of the app's own, and a command
//! ring only the broker writes. It is written against two interfaces, so no
//! one GPU is hard-wired into the app path:
//!
//! - [`Arch`]: how one GPU architecture checks an app's code object and
//!   turns the closed record set into its command stream. [`Rdna4`] is the
//!   one implemented; another architecture (RDNA3, Intel Xe) adds its own
//!   code header and builder behind the same trait, and the channel, the
//!   records and this session stay as they are.
//! - [`Backend`]: the driver underneath (a VM, buffers in it, submit and
//!   wait). [`amd::AmdBackend`] drives atrium-gpu-amd; tests use memory.
//!
//! Every limit here is per context; the broker also caps contexts per peer.

use std::collections::HashMap;
use std::io;

use atrium_rdna4_cmd::Scratch;

use crate::{CodeBinding, CodeHeader, Refusal, ARCH_RDNA4};

/// Most code objects one context may hold.
pub const MAX_CODES: u32 = 64;
/// Most buffers one context may hold.
pub const MAX_BUFFERS: u32 = 256;
/// Largest single buffer.
pub const MAX_BUFFER_BYTES: u64 = 64 << 20;
/// Most buffer bytes (code, scratch and buffers) one context may hold.
pub const MAX_CONTEXT_BYTES: u64 = 256 << 20;
/// The broker's command ring for one context.
pub const RING_BYTES: u64 = 64 << 10;

const PAGE: u64 = 4096;

/// A code object placed in the app's VM.
#[derive(Debug, Clone, Copy)]
pub struct Placed<C> {
    /// What the architecture checked.
    pub code: C,
    /// GPU VA of the machine code.
    pub va: u64,
    /// GPU VA of the scratch the broker gave it, if it asked for any.
    pub scratch_va: Option<u64>,
}

/// One GPU architecture's half of the app path.
pub trait Arch {
    /// The id `HELLO` reports and code headers carry.
    const ID: u32;
    /// A checked code header.
    type Code: Copy;
    /// Check a code object (header and machine code) against the hardware's
    /// limits; return what was checked and the machine code.
    fn check_code(slot: &[u8]) -> Result<(Self::Code, &[u8]), Refusal>;
    /// Scratch bytes the code needs in the VM (0: none).
    fn scratch_bytes(code: &Self::Code) -> u64;
    /// The command stream for `records`, every packet written here from
    /// checked values, ending with an end-of-pipe that writes `fence_va`
    /// and raises the completion interrupt.
    fn build(records: &[u8], code: &dyn Fn(u32) -> Option<Placed<Self::Code>>, fence_va: u64) -> Result<Vec<u8>, Refusal>;
}

/// AMD RDNA4 (GFX12): [`CodeHeader`], [`crate::build`] and PM4.
pub struct Rdna4;

/// Wave slots in a scratch ring the broker gives a code object.
const SCRATCH_WAVES: u32 = 64;
const IT_RELEASE_MEM: u32 = 0x49;

impl Arch for Rdna4 {
    const ID: u32 = ARCH_RDNA4;
    type Code = CodeHeader;

    fn check_code(slot: &[u8]) -> Result<(CodeHeader, &[u8]), Refusal> {
        CodeHeader::parse(slot)
    }

    fn scratch_bytes(h: &CodeHeader) -> u64 {
        if h.scratch_dwords == 0 {
            0
        } else {
            Scratch::wave_bytes(h.scratch_dwords, h.lanes()) as u64 * SCRATCH_WAVES as u64
        }
    }

    fn build(records: &[u8], code: &dyn Fn(u32) -> Option<Placed<CodeHeader>>, fence_va: u64) -> Result<Vec<u8>, Refusal> {
        let mut blob = crate::build(records, &|id| {
            code(id).map(|p| CodeBinding {
                header: p.code,
                va: p.va,
                scratch: p.scratch_va.map(|va| Scratch {
                    va,
                    wave_bytes: Scratch::wave_bytes(p.code.scratch_dwords, p.code.lanes()),
                    waves: SCRATCH_WAVES,
                }),
            })
        })?;
        // End of pipe: write 1 to the fence and interrupt once it lands.
        blob.packet(IT_RELEASE_MEM, &[5 << 8, (2 << 29) | (2 << 24), fence_va as u32, (fence_va >> 32) as u32, 1, 0]);
        Ok(blob.bytes())
    }
}

/// The driver under a session: one VM, buffers in it, submit and wait.
pub trait Backend {
    /// A buffer bound in the VM.
    type Buf;
    /// A buffer of `bytes` in the VM; `gpu_readonly` maps it so a GPU store
    /// to it faults (the CPU side still fills it).
    fn alloc(&mut self, bytes: u64, gpu_readonly: bool) -> io::Result<Self::Buf>;
    /// The buffer's GPU VA in the VM.
    fn va(&self, b: &Self::Buf) -> u64;
    /// CPU write into the buffer.
    fn write(&mut self, b: &Self::Buf, offset: u64, data: &[u8]) -> io::Result<()>;
    /// CPU read from the buffer.
    fn read(&mut self, b: &Self::Buf, offset: u64, out: &mut [u8]) -> io::Result<()>;
    /// Run the first `dwords` of `ring` and wait for its end of pipe. An
    /// error means the work was lost (hang, fault, reset).
    fn run(&mut self, ring: &Self::Buf, dwords: u32) -> io::Result<()>;
}

/// Why a session refused or failed a request.
#[derive(Debug)]
pub enum Error {
    /// The code object.
    Code(Refusal),
    /// The work records.
    Work(Refusal),
    /// Over a per-context limit.
    Limit(&'static str),
    /// An id that is not this context's, or one already in use.
    Id(u32),
    /// An offset or length outside the buffer.
    Range,
    /// The driver refused (out of memory, no device).
    Device(io::Error),
    /// Work was lost; the context is unusable from here on.
    Lost(io::Error),
}

struct Code<A: Arch, B: Backend> {
    placed: Placed<A::Code>,
    bytes: u64,
    _code: B::Buf,
    _scratch: Option<B::Buf>,
}

struct Buffer<B: Backend> {
    buf: B::Buf,
    bytes: u64,
}

/// One app's GPU context.
pub struct Session<A: Arch, B: Backend> {
    be: B,
    ring: B::Buf,
    fence: B::Buf,
    codes: HashMap<u32, Code<A, B>>,
    bufs: HashMap<u32, Buffer<B>>,
    bytes: u64,
    lost: bool,
}

fn pages(bytes: u64) -> u64 {
    bytes.div_ceil(PAGE).max(1) * PAGE
}

impl<A: Arch, B: Backend> Session<A, B> {
    /// A context over `be` (a fresh VM). The ring is read-only to the GPU:
    /// the app's own dispatches cannot rewrite the packets after theirs.
    pub fn new(mut be: B) -> io::Result<Self> {
        let ring = be.alloc(RING_BYTES, true)?;
        let fence = be.alloc(PAGE, false)?;
        Ok(Session { be, ring, fence, codes: HashMap::new(), bufs: HashMap::new(), bytes: 0, lost: false })
    }

    fn charge(&self, bytes: u64) -> Result<(), Error> {
        if self.bytes + bytes > MAX_CONTEXT_BYTES {
            Err(Error::Limit("context memory"))
        } else {
            Ok(())
        }
    }

    /// Check `slot` and load it as code object `id`, read-only in the VM.
    pub fn load_code(&mut self, id: u32, slot: &[u8]) -> Result<(), Error> {
        if id >= MAX_CODES || self.codes.contains_key(&id) {
            return Err(Error::Id(id));
        }
        let (code, machine) = A::check_code(slot).map_err(Error::Code)?;
        let scratch = A::scratch_bytes(&code);
        let bytes = pages(machine.len() as u64) + if scratch > 0 { pages(scratch) } else { 0 };
        self.charge(bytes)?;
        let buf = self.be.alloc(pages(machine.len() as u64), true).map_err(Error::Device)?;
        self.be.write(&buf, 0, machine).map_err(Error::Device)?;
        let sbuf = if scratch > 0 { Some(self.be.alloc(pages(scratch), false).map_err(Error::Device)?) } else { None };
        let placed = Placed { code, va: self.be.va(&buf), scratch_va: sbuf.as_ref().map(|s| self.be.va(s)) };
        self.bytes += bytes;
        self.codes.insert(id, Code { placed, bytes, _code: buf, _scratch: sbuf });
        Ok(())
    }

    /// Drop code object `id`. No work is in flight when the broker serves a
    /// request (submits are waited on), and the driver defers the free.
    pub fn free_code(&mut self, id: u32) -> Result<(), Error> {
        let c = self.codes.remove(&id).ok_or(Error::Id(id))?;
        self.bytes -= c.bytes;
        Ok(())
    }

    /// Make buffer `id` of `bytes` and return its VA in the app's VM.
    pub fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<u64, Error> {
        if id >= MAX_BUFFERS || self.bufs.contains_key(&id) {
            return Err(Error::Id(id));
        }
        if bytes == 0 || bytes > MAX_BUFFER_BYTES {
            return Err(Error::Limit("buffer size"));
        }
        let size = pages(bytes);
        self.charge(size)?;
        let buf = self.be.alloc(size, false).map_err(Error::Device)?;
        let va = self.be.va(&buf);
        self.bytes += size;
        self.bufs.insert(id, Buffer { buf, bytes });
        Ok(va)
    }

    /// Drop buffer `id`.
    pub fn free_buffer(&mut self, id: u32) -> Result<(), Error> {
        let b = self.bufs.remove(&id).ok_or(Error::Id(id))?;
        self.bytes -= pages(b.bytes);
        Ok(())
    }

    fn span(&self, id: u32, offset: u64, len: u64) -> Result<&Buffer<B>, Error> {
        let b = self.bufs.get(&id).ok_or(Error::Id(id))?;
        match offset.checked_add(len) {
            Some(end) if end <= b.bytes => Ok(b),
            _ => Err(Error::Range),
        }
    }

    /// Copy `data` into buffer `id` at `offset`.
    pub fn write_buffer(&mut self, id: u32, offset: u64, data: &[u8]) -> Result<(), Error> {
        self.span(id, offset, data.len() as u64)?;
        let b = &self.bufs[&id].buf;
        self.be.write(b, offset, data).map_err(Error::Device)
    }

    /// Copy from buffer `id` at `offset` into `out`.
    pub fn read_buffer(&mut self, id: u32, offset: u64, out: &mut [u8]) -> Result<(), Error> {
        self.span(id, offset, out.len() as u64)?;
        let b = &self.bufs[&id].buf;
        self.be.read(b, offset, out).map_err(Error::Device)
    }

    /// Check `records`, build the command stream into the broker's ring,
    /// run it in the app's VM and wait. After `Lost` every request fails.
    pub fn submit(&mut self, records: &[u8]) -> Result<(), Error> {
        if self.lost {
            return Err(Error::Lost(io::Error::from_raw_os_error(libc_eio())));
        }
        let codes = &self.codes;
        let stream = A::build(records, &|id| codes.get(&id).map(|c| c.placed), self.be.va(&self.fence))
            .map_err(Error::Work)?;
        if stream.len() as u64 > RING_BYTES {
            return Err(Error::Limit("command stream larger than the ring"));
        }
        self.be.write(&self.ring, 0, &stream).map_err(Error::Device)?;
        self.be.run(&self.ring, (stream.len() / 4) as u32).map_err(|e| {
            self.lost = true;
            Error::Lost(e)
        })
    }

    /// Whether work was lost (the broker then ends the connection).
    pub fn is_lost(&self) -> bool {
        self.lost
    }
}

const fn libc_eio() -> i32 {
    5
}

/// atrium-gpu-amd as a [`Backend`].
#[cfg(feature = "device")]
pub mod amd {
    use std::io;

    use atrium_gpu::amd::{Bo, Gpu, Syncobj, Vm, ENGINE_COMPUTE};

    /// One VM on the broker's device fd, and a syncobj its end-of-pipe
    /// interrupts signal.
    pub struct AmdBackend {
        vm: Vm<'static>,
        so: Syncobj<'static>,
        value: u64,
        timeout_ms: u32,
    }

    impl AmdBackend {
        /// A fresh VM on `gpu`. `timeout_ms` bounds a wait; the driver's
        /// watchdog (`isolation.hang_ms`) normally fails hung work first.
        pub fn new(gpu: &'static Gpu, timeout_ms: u32) -> io::Result<Self> {
            Ok(AmdBackend { vm: gpu.create_vm()?, so: gpu.create_syncobj()?, value: 0, timeout_ms })
        }
    }

    impl super::Backend for AmdBackend {
        type Buf = Bo<'static>;

        fn alloc(&mut self, bytes: u64, gpu_readonly: bool) -> io::Result<Bo<'static>> {
            if gpu_readonly { self.vm.alloc_readonly(bytes, 0) } else { self.vm.alloc(bytes, 0) }
        }
        fn va(&self, b: &Bo<'static>) -> u64 {
            b.gpu_va()
        }
        fn write(&mut self, b: &Bo<'static>, offset: u64, data: &[u8]) -> io::Result<()> {
            b.write(offset, data)
        }
        fn read(&mut self, b: &Bo<'static>, offset: u64, out: &mut [u8]) -> io::Result<()> {
            b.read(offset, out)
        }
        fn run(&mut self, ring: &Bo<'static>, dwords: u32) -> io::Result<()> {
            self.value += 1;
            // Another app's work may hold the shared queue for up to the
            // watchdog's hang_ms; the driver then says EBUSY. Retry for
            // longer than that before calling it lost.
            let mut tries = 0;
            loop {
                match self.vm.submit(ring, dwords, ENGINE_COMPUTE, Some((&self.so, self.value))) {
                    Err(e) if e.raw_os_error() == Some(libc::EBUSY) && tries < 4 => tries += 1,
                    r => break r?,
                }
            }
            self.so.wait(self.value, self.timeout_ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{op, record};

    /// Memory standing in for a VM.
    #[derive(Default)]
    struct Mem {
        next_va: u64,
        bufs: Vec<(u64, bool, Vec<u8>)>,
        runs: Vec<Vec<u8>>,
        fail: bool,
    }

    impl Backend for Mem {
        type Buf = usize;
        fn alloc(&mut self, bytes: u64, ro: bool) -> io::Result<usize> {
            self.next_va += 0x10_0000;
            self.bufs.push((self.next_va, ro, vec![0; bytes as usize]));
            Ok(self.bufs.len() - 1)
        }
        fn va(&self, b: &usize) -> u64 {
            self.bufs[*b].0
        }
        fn write(&mut self, b: &usize, off: u64, d: &[u8]) -> io::Result<()> {
            self.bufs[*b].2[off as usize..off as usize + d.len()].copy_from_slice(d);
            Ok(())
        }
        fn read(&mut self, b: &usize, off: u64, o: &mut [u8]) -> io::Result<()> {
            o.copy_from_slice(&self.bufs[*b].2[off as usize..off as usize + o.len()]);
            Ok(())
        }
        fn run(&mut self, ring: &usize, dw: u32) -> io::Result<()> {
            self.runs.push(self.bufs[*ring].2[..dw as usize * 4].to_vec());
            if self.fail { Err(io::Error::from_raw_os_error(5)) } else { Ok(()) }
        }
    }

    fn code() -> Vec<u8> {
        CodeHeader { code_bytes: 8, vgprs: 4, user_sgprs: 2, lds_bytes: 0, scratch_dwords: 0, threads: [64, 1, 1], wave32: true }
            .encode(&[1, 0, 0, 0, 2, 0, 0, 0])
    }

    fn work(va: u64) -> Vec<u8> {
        [record(op::BIND_CODE, &[0]), record(op::USER_DATA, &[2, va as u32, (va >> 32) as u32]), record(op::DISPATCH, &[1, 1, 1, 0, 0, 0])]
            .concat()
    }

    #[test]
    fn runs_an_apps_work_from_a_read_only_ring() {
        let mut s: Session<Rdna4, Mem> = Session::new(Mem::default()).unwrap();
        s.load_code(0, &code()).unwrap();
        let va = s.create_buffer(3, 256).unwrap();
        s.write_buffer(3, 0, &[7; 16]).unwrap();
        s.submit(&work(va)).unwrap();
        let mut out = [0u8; 16];
        s.read_buffer(3, 0, &mut out).unwrap();
        assert_eq!(out, [7; 16]);
        assert_eq!(s.be.runs.len(), 1);
        // The ring (first alloc) and the code are GPU read-only.
        assert!(s.be.bufs[0].1, "ring read-only");
        assert!(s.be.bufs[2].1, "code read-only");
        assert!(!s.be.bufs[3].1, "buffer writable");
        // The stream is exactly what the builder makes, plus the fence.
        let want = Rdna4::build(&work(va), &|id| s.codes.get(&id).map(|c| c.placed), s.be.va(&s.fence)).unwrap();
        assert_eq!(s.be.runs[0], want);
    }

    #[test]
    fn refusals_and_limits() {
        let mut s: Session<Rdna4, Mem> = Session::new(Mem::default()).unwrap();
        let mut bad = code();
        bad[44] = 9;
        assert!(matches!(s.load_code(0, &bad), Err(Error::Code(Refusal::WrongArch(9)))));
        s.load_code(0, &code()).unwrap();
        assert!(matches!(s.load_code(0, &code()), Err(Error::Id(0))));
        assert!(matches!(s.load_code(MAX_CODES, &code()), Err(Error::Id(_))));
        assert!(matches!(s.create_buffer(0, MAX_BUFFER_BYTES + 1), Err(Error::Limit(_))));
        for id in 0..3 {
            s.create_buffer(id, MAX_BUFFER_BYTES).unwrap();
        }
        // The code page is charged too, so a fourth full buffer is over.
        assert!(matches!(s.create_buffer(3, MAX_BUFFER_BYTES), Err(Error::Limit("context memory"))));
        s.create_buffer(3, MAX_BUFFER_BYTES - 4096).unwrap();
        assert!(matches!(s.create_buffer(4, 4096), Err(Error::Limit("context memory"))));
        s.free_buffer(3).unwrap();
        s.create_buffer(4, 4096).unwrap();
        assert!(matches!(s.write_buffer(4, 4090, &[0; 7]), Err(Error::Range)));
        assert!(matches!(s.write_buffer(4, u64::MAX, &[0; 1]), Err(Error::Range)));
        assert!(matches!(s.read_buffer(9, 0, &mut [0; 1]), Err(Error::Id(9))));
        assert!(matches!(s.submit(&[0; 79]), Err(Error::Work(Refusal::RecordCount(79)))));
        let unknown = [record(op::BIND_CODE, &[5])].concat();
        assert!(matches!(s.submit(&unknown), Err(Error::Work(Refusal::UnknownCode { .. }))));
        assert!(s.be.runs.is_empty(), "nothing refused reached the GPU");
    }

    #[test]
    fn lost_work_ends_the_context() {
        let mut s: Session<Rdna4, Mem> = Session::new(Mem { fail: true, ..Default::default() }).unwrap();
        s.load_code(0, &code()).unwrap();
        let va = s.create_buffer(0, 64).unwrap();
        assert!(matches!(s.submit(&work(va)), Err(Error::Lost(_))));
        assert!(s.is_lost());
        s.be.fail = false;
        assert!(matches!(s.submit(&work(va)), Err(Error::Lost(_))));
        assert_eq!(s.be.runs.len(), 1);
    }
}
