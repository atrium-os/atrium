//! The app GPU channel, broker side (F6 G2, docs/spec/gpu-app-path-f6.md;
//! protocol in `fresco_split::gpu`).
//!
//! An app that brings its own shaders connects to `FRESCOD_GPU_SOCK`.
//! Reaching that socket is the grant (the TCB mounts it only into jails
//! whose manifest has `gpu.access`), and each peer uid may hold at most
//! [`MAX_PER_UID`] contexts. Each connection is one GPU context: a VM of its
//! own on frescod's device fd, served by its own thread, so an app waiting
//! on its GPU work holds up nothing but itself. The app never holds a GPU
//! fd and never writes a packet: `atrium_gpu_app::session` checks what it
//! sends and builds the command stream, behind an architecture interface so
//! another GPU slots in without touching this file's protocol handling.
//!
//! A record that is refused ends the connection after a `REFUSED`; work
//! that is lost (hang, fault) ends it after a `LOST`, and the VM goes with
//! it. Replies go out with `send_now`: an app that stops reading is dropped,
//! never waited on.

use std::collections::HashMap;
use std::io;
use std::sync::{Mutex, OnceLock};

use atrium_gpu::amd::Gpu;
use atrium_gpu_app::session::{amd::AmdBackend, Arch, Error, Rdna4, Session};
use atrium_gpu_app::{limits, CODE_HEADER_SIZE, OP_RECORD};
use fresco_split::chan::{Chan, Raw, Shm};
use fresco_split::gpu::{reason, GpuMsg};

use crate::split::Bucket;

/// Contexts one peer uid may hold at once.
pub const MAX_PER_UID: usize = 4;
/// Contexts frescod serves at once.
pub const MAX_CONTEXTS: usize = 32;
/// Records per second one context may send (it waits past that).
const RECORDS_PER_S: f64 = 2000.0;
const RECORD_BURST: f64 = 256.0;
/// Longest wait on one submit. The driver's watchdog (`hang_ms`, 5 s by
/// default) fails hung work well before this.
const WAIT_MS: u32 = 30_000;

const UPLOAD_BYTES: usize = CODE_HEADER_SIZE + limits::MAX_CODE_BYTES as usize;
const WORK_BYTES: usize = limits::MAX_RECORDS * OP_RECORD;

// The protocol's limits are the session's.
const _: () = assert!(fresco_split::gpu::MAX_CODES == atrium_gpu_app::session::MAX_CODES);
const _: () = assert!(fresco_split::gpu::MAX_BUFFERS == atrium_gpu_app::session::MAX_BUFFERS);
const _: () = assert!(fresco_split::gpu::MAX_BUFFER_BYTES as u64 == atrium_gpu_app::session::MAX_BUFFER_BYTES);

static CONTEXTS: Mutex<Option<HashMap<u32, usize>>> = Mutex::new(None);

/// frescod's device fd for app contexts, opened on first use.
fn device() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| match Gpu::open() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("frescod: GPU channel: no GPU ({e}); every context will be refused");
            None
        }
    })
    .as_ref()
}

/// A context slot for `uid`, released on drop.
struct Slot(u32);

impl Slot {
    fn take(uid: u32) -> Option<Slot> {
        let mut g = CONTEXTS.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        let total: usize = m.values().sum();
        let mine = m.entry(uid).or_insert(0);
        if *mine >= MAX_PER_UID || total >= MAX_CONTEXTS {
            return None;
        }
        *mine += 1;
        Some(Slot(uid))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(m) = CONTEXTS.lock().unwrap().as_mut() {
            if let Some(n) = m.get_mut(&self.0) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.0);
                }
            }
        }
    }
}

/// Serve the GPU socket at `path`.
pub fn serve(path: &std::path::Path) -> io::Result<()> {
    let l = fresco_split::chan::listen(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }
    eprintln!("frescod: GPU channel on {}", path.display());
    std::thread::Builder::new().name("frescod-gpu-accept".into()).spawn(move || {
        let mut next = 0u64;
        loop {
            match fresco_split::chan::accept(&l) {
                Ok((c, uid)) => {
                    next += 1;
                    let conn = next;
                    let _ = std::thread::Builder::new().name(format!("frescod-gpu-{conn}")).spawn(move || {
                        let why = match Slot::take(uid) {
                            Some(_slot) => context(&c),
                            None => {
                                refuse(&c, reason::LIMIT, 0);
                                format!("uid {uid} at its context cap")
                            }
                        };
                        log::info!("frescod: GPU context {conn} (uid {uid}): {why}");
                    });
                }
                Err(e) => {
                    eprintln!("frescod: GPU accept: {e}");
                    return;
                }
            }
        }
    })?;
    Ok(())
}

fn refuse(c: &Chan, why: u32, detail: u32) {
    let _ = c.send_now_raw(&GpuMsg::Refused { reason: why, detail }.encode(), &[]);
}

/// The reason code and detail an error is refused with.
fn refusal(e: &Error) -> (u32, u32) {
    match e {
        Error::Code(_) => (reason::CODE, 0),
        Error::Work(r) => (reason::WORK, work_index(r)),
        Error::Limit(_) => (reason::LIMIT, 0),
        Error::Id(id) => (reason::ID, *id),
        Error::Range => (reason::RANGE, 0),
        Error::Device(_) | Error::Lost(_) => (reason::DEVICE, 0),
    }
}

fn work_index(r: &atrium_gpu_app::Refusal) -> u32 {
    use atrium_gpu_app::Refusal as R;
    match *r {
        R::UnknownOp { index, .. }
        | R::RecordPadding { index }
        | R::UnknownCode { index, .. }
        | R::NoCode { index }
        | R::SgprCount { index, .. }
        | R::SgprMismatch { index, .. }
        | R::Groups { index } => index as u32,
        _ => 0,
    }
}

/// One context; returns why it ended.
fn context(c: &Chan) -> String {
    let Some(gpu) = device() else {
        refuse(c, reason::DEVICE, 0);
        return "no GPU".into();
    };
    // The architecture comes from the driver's CAP_ISA, never the vendor
    // string. Only RDNA4 has an app-path Arch so far; another family gets
    // its own `Arch` and a match arm here.
    match gpu.caps().map(|c| c.isa_family) {
        Ok(f) if f == Rdna4::ID => {}
        other => {
            refuse(c, reason::DEVICE, 0);
            return format!("no app-path architecture for this GPU (ISA family {other:?})");
        }
    }
    let session = AmdBackend::new(gpu, WAIT_MS).and_then(Session::<Rdna4, _>::new);
    let mut s = match session {
        Ok(s) => s,
        Err(e) => {
            refuse(c, reason::DEVICE, 0);
            return format!("VM: {e}");
        }
    };
    let (upload, work) = match (Shm::create(UPLOAD_BYTES), Shm::create(WORK_BYTES)) {
        (Ok(u), Ok(w)) => (u, w),
        _ => {
            refuse(c, reason::DEVICE, 0);
            return "slots".into();
        }
    };
    let hello = GpuMsg::Hello { arch: Rdna4::ID, upload_bytes: UPLOAD_BYTES as u32, work_bytes: WORK_BYTES as u32 };
    if let Err(e) = c.send_now_raw(&hello.encode(), &[upload.fd(), work.fd()]) {
        return e.to_string();
    }
    let mut staging: HashMap<u32, Shm> = HashMap::new();
    let mut rate = Bucket::new(RECORDS_PER_S, RECORD_BURST);
    loop {
        let m = match c.recv_raw() {
            Ok(Raw::Rec(rec, fds)) if fds.is_empty() => match GpuMsg::decode(&rec) {
                Ok(m) if m.from_app() => m,
                Ok(_) => {
                    refuse(c, reason::MALFORMED, 0);
                    return "sent a broker's record".into();
                }
                Err(e) => {
                    refuse(c, reason::MALFORMED, 0);
                    return format!("refused a record: {}", e.0);
                }
            },
            Ok(Raw::Rec(..)) => {
                refuse(c, reason::MALFORMED, 0);
                return "a record with fds".into();
            }
            Ok(Raw::Refused(e)) => {
                refuse(c, reason::MALFORMED, 0);
                return format!("refused a record: {}", e.0);
            }
            Ok(Raw::Eof) => return "closed".into(),
            Err(e) => return e.to_string(),
        };
        rate.take();
        let fence = if let GpuMsg::Submit { fence, .. } = m { fence } else { 0 };
        let (reply, fd) = match serve_one(&mut s, &upload, &work, &mut staging, m) {
            Ok(r) => r,
            Err(Error::Lost(e)) => {
                let errno = e.raw_os_error().unwrap_or(libc::EIO) as u32;
                let _ = c.send_now_raw(&GpuMsg::Lost { fence, errno }.encode(), &[]);
                return format!("work lost: {e}");
            }
            Err(e) => {
                let (why, detail) = refusal(&e);
                refuse(c, why, detail);
                return format!("refused: {e:?}");
            }
        };
        if let Some(reply) = reply {
            let fds: Vec<_> = fd.into_iter().collect();
            if c.send_now_raw(&reply.encode(), &fds).is_err() {
                return "stopped reading".into();
            }
        }
    }
}

type Ctx = Session<Rdna4, AmdBackend>;

/// `bytes` at `offset` of a slot of `len`, or a range error.
fn span(len: usize, offset: u32, bytes: u32) -> Result<std::ops::Range<usize>, Error> {
    let (o, n) = (offset as usize, bytes as usize);
    match o.checked_add(n) {
        Some(end) if end <= len => Ok(o..end),
        _ => Err(Error::Range),
    }
}

/// Serve one request: the reply, if it has one, and the fd it carries.
fn serve_one(
    s: &mut Ctx,
    upload: &Shm,
    work: &Shm,
    staging: &mut HashMap<u32, Shm>,
    m: GpuMsg,
) -> Result<(Option<GpuMsg>, Option<std::os::fd::RawFd>), Error> {
    Ok(match m {
        GpuMsg::Code { id, bytes } => {
            // Copied out once: the app may be rewriting the slot.
            let code = upload.as_slice()[span(upload.len(), 0, bytes)?].to_vec();
            s.load_code(id, &code)?;
            (Some(GpuMsg::CodeOk { id }), None)
        }
        GpuMsg::Buffer { id, bytes } => {
            let va = s.create_buffer(id, bytes as u64)?;
            match Shm::create(bytes as usize) {
                Ok(shm) => {
                    let fd = shm.fd();
                    staging.insert(id, shm);
                    (Some(GpuMsg::BufferOk { id, va, bytes }), Some(fd))
                }
                Err(e) => {
                    let _ = s.free_buffer(id);
                    return Err(Error::Device(e));
                }
            }
        }
        GpuMsg::Free { id } => {
            s.free_buffer(id)?;
            staging.remove(&id);
            (None, None)
        }
        GpuMsg::Write { id, offset, bytes } => {
            let st = staging.get(&id).ok_or(Error::Id(id))?;
            let data = st.as_slice()[span(st.len(), offset, bytes)?].to_vec();
            s.write_buffer(id, offset as u64, &data)?;
            (None, None)
        }
        GpuMsg::Read { id, offset, bytes } => {
            let st = staging.get_mut(&id).ok_or(Error::Id(id))?;
            let r = span(st.len(), offset, bytes)?;
            let mut data = vec![0u8; bytes as usize];
            s.read_buffer(id, offset as u64, &mut data)?;
            st.as_mut_slice()[r].copy_from_slice(&data);
            (Some(GpuMsg::ReadDone { id }), None)
        }
        GpuMsg::Submit { fence, bytes } => {
            let records = work.as_slice()[span(work.len(), 0, bytes)?].to_vec();
            s.submit(&records)?;
            (Some(GpuMsg::Done { fence }), None)
        }
        _ => unreachable!("from_app() checked"),
    })
}
