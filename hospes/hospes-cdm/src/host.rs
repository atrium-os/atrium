//! What the CDM calls: our `cdm::Host_10`, and the objects it hands back and forth
//! (`Buffer`, `DecryptedBlock`, `VideoFrame`, `FileIO`), each a `#[repr(C)]` struct whose
//! first word points at a vtable of `extern "C"` functions taking `this` (docs/spec/hospes.md
//! §8.1, §8.2).
//!
//! One CDM per process, called from one thread: the loop's. A callback from any other thread
//! ends the process (a later step can add a cross-thread queue if Widevine needs one).
//! Results that Chromium delivers asynchronously (file I/O, output protection, the storage
//! id) are queued and delivered from the loop, never re-entrantly inside the CDM's call.
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use hospes_proto::{KeyInfo, Msg};
use hospes_rt::{hlog, sys};

use crate::abi::{self, Cdm, FileClient, KeyInformation, Obj, Size, VTable};
use crate::store::{self, Store};

/// Largest buffer the CDM may ask for.
pub const MAX_BUFFER: u32 = 64 << 20;

pub enum Deferred {
    Open(usize, u32),
    Read(usize, u32, Vec<u8>),
    Write(usize, u32),
    Protection,
    StorageId(u32),
    Challenge,
}

pub struct State {
    pub ctrl: i32,
    pub cdm: Option<Cdm>,
    pub timers: Vec<(u64, usize)>,
    pub deferred: VecDeque<Deferred>,
    pub store: Option<Store>,
    pub files: Vec<Option<Box<FileIo>>>,
    pub buffers: Vec<*mut BufferObj>,
    pub link_mask: u32,
    pub thread: i64,
    pub ctrl_broken: bool,
}

static mut STATE: *mut State = ptr::null_mut();

pub fn install(s: State) {
    // Safety: called once, before the CDM exists.
    unsafe { STATE = Box::into_raw(Box::new(s)) };
}

/// The process's one state. Every caller is on the loop's thread (checked).
#[allow(clippy::mut_from_ref)]
pub fn st() -> &'static mut State {
    // Safety: installed before any CDM call; single-threaded by the check below.
    let s = unsafe { &mut *STATE };
    if sys::gettid() != s.thread {
        crate::fatal(10, "CDM called the host from another thread");
    }
    s
}

pub fn send(m: &Msg) {
    let s = st();
    if s.ctrl_broken {
        return;
    }
    if sys::write_all(s.ctrl, &m.encode()).is_err() {
        s.ctrl_broken = true;
    }
}

unsafe fn bytes<'a>(p: *const u8, n: u32) -> &'a [u8] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(p, n as usize)
    }
}

extern "C" fn never(_this: *mut Obj) {
    crate::fatal(11, "a protected destructor was called");
}

// ---------------------------------------------------------------- Buffer

#[repr(C)]
pub struct BufferObj {
    vptr: *const usize,
    pub data: *mut u8,
    pub cap: u32,
    pub size: u32,
}

extern "C" fn buf_destroy(this: *mut BufferObj) {
    let s = st();
    if let Some(i) = s.buffers.iter().position(|&b| b == this) {
        s.buffers.swap_remove(i);
        // Safety: made by `allocate` below, and no longer listed.
        unsafe {
            let b = Box::from_raw(this);
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(b.data, b.cap as usize)));
        }
    } else {
        crate::fatal(12, "Destroy on a buffer that is not ours");
    }
}
extern "C" fn buf_capacity(this: *const BufferObj) -> u32 {
    unsafe { (*this).cap }
}
extern "C" fn buf_data(this: *mut BufferObj) -> *mut u8 {
    unsafe { (*this).data }
}
extern "C" fn buf_set_size(this: *mut BufferObj, n: u32) {
    unsafe { (*this).size = n.min((*this).cap) };
}
extern "C" fn buf_size(this: *const BufferObj) -> u32 {
    unsafe { (*this).size }
}

static BUFFER_VT: VTable<7> = VTable {
    offset_to_top: 0,
    rtti: ptr::null(),
    slots: [
        buf_destroy as *const (),
        buf_capacity as *const (),
        buf_data as *const (),
        buf_set_size as *const (),
        buf_size as *const (),
        never as *const (),
        never as *const (),
    ],
};

pub fn allocate(cap: u32) -> *mut BufferObj {
    if cap > MAX_BUFFER {
        return ptr::null_mut();
    }
    let cap = cap.max(1);
    let data = Box::into_raw(alloc::vec![0u8; cap as usize].into_boxed_slice()) as *mut u8;
    let b = Box::into_raw(Box::new(BufferObj { vptr: BUFFER_VT.slots.as_ptr() as *const usize, data, cap, size: 0 }));
    st().buffers.push(b);
    b
}

/// Take a buffer the CDM handed back: copy its bytes out and destroy it. `None` if it is
/// not one of ours.
pub fn take(b: *mut BufferObj) -> Option<Vec<u8>> {
    if b.is_null() || !st().buffers.contains(&b) {
        return None;
    }
    // Safety: listed, so live and ours.
    let v = unsafe { bytes((*b).data, (*b).size).to_vec() };
    buf_destroy(b);
    Some(v)
}

// ---------------------------------------------------------------- DecryptedBlock

#[repr(C)]
pub struct BlockObj {
    vptr: *const usize,
    pub buffer: *mut BufferObj,
    pub timestamp: i64,
}
extern "C" fn blk_set_buffer(this: *mut BlockObj, b: *mut BufferObj) {
    unsafe { (*this).buffer = b };
}
extern "C" fn blk_buffer(this: *mut BlockObj) -> *mut BufferObj {
    unsafe { (*this).buffer }
}
extern "C" fn blk_set_ts(this: *mut BlockObj, t: i64) {
    unsafe { (*this).timestamp = t };
}
extern "C" fn blk_ts(this: *const BlockObj) -> i64 {
    unsafe { (*this).timestamp }
}
static BLOCK_VT: VTable<6> = VTable {
    offset_to_top: 0,
    rtti: ptr::null(),
    slots: [blk_set_buffer as *const (), blk_buffer as *const (), blk_set_ts as *const (), blk_ts as *const (), never as *const (), never as *const ()],
};
impl BlockObj {
    pub fn new() -> BlockObj {
        BlockObj { vptr: BLOCK_VT.slots.as_ptr() as *const usize, buffer: ptr::null_mut(), timestamp: 0 }
    }
}

// ---------------------------------------------------------------- VideoFrame

#[repr(C)]
pub struct FrameObj {
    vptr: *const usize,
    pub format: u32,
    pub size: Size,
    pub buffer: *mut BufferObj,
    pub offsets: [u32; 3],
    pub strides: [u32; 3],
    pub timestamp: i64,
}
extern "C" fn fr_set_format(this: *mut FrameObj, f: u32) {
    unsafe { (*this).format = f };
}
extern "C" fn fr_format(this: *const FrameObj) -> u32 {
    unsafe { (*this).format }
}
extern "C" fn fr_set_size(this: *mut FrameObj, s: Size) {
    unsafe { (*this).size = s };
}
extern "C" fn fr_size(this: *const FrameObj) -> Size {
    unsafe { (*this).size }
}
extern "C" fn fr_set_buffer(this: *mut FrameObj, b: *mut BufferObj) {
    unsafe { (*this).buffer = b };
}
extern "C" fn fr_buffer(this: *mut FrameObj) -> *mut BufferObj {
    unsafe { (*this).buffer }
}
extern "C" fn fr_set_offset(this: *mut FrameObj, plane: u32, v: u32) {
    if plane < 3 {
        unsafe { (*this).offsets[plane as usize] = v };
    }
}
extern "C" fn fr_offset(this: *mut FrameObj, plane: u32) -> u32 {
    if plane < 3 { unsafe { (*this).offsets[plane as usize] } } else { 0 }
}
extern "C" fn fr_set_stride(this: *mut FrameObj, plane: u32, v: u32) {
    if plane < 3 {
        unsafe { (*this).strides[plane as usize] = v };
    }
}
extern "C" fn fr_stride(this: *mut FrameObj, plane: u32) -> u32 {
    if plane < 3 { unsafe { (*this).strides[plane as usize] } } else { 0 }
}
extern "C" fn fr_set_ts(this: *mut FrameObj, t: i64) {
    unsafe { (*this).timestamp = t };
}
extern "C" fn fr_ts(this: *const FrameObj) -> i64 {
    unsafe { (*this).timestamp }
}
static FRAME_VT: VTable<14> = VTable {
    offset_to_top: 0,
    rtti: ptr::null(),
    slots: [
        fr_set_format as *const (),
        fr_format as *const (),
        fr_set_size as *const (),
        fr_size as *const (),
        fr_set_buffer as *const (),
        fr_buffer as *const (),
        fr_set_offset as *const (),
        fr_offset as *const (),
        fr_set_stride as *const (),
        fr_stride as *const (),
        fr_set_ts as *const (),
        fr_ts as *const (),
        never as *const (),
        never as *const (),
    ],
};
impl FrameObj {
    pub fn new() -> FrameObj {
        FrameObj {
            vptr: FRAME_VT.slots.as_ptr() as *const usize,
            format: 0,
            size: Size { width: 0, height: 0 },
            buffer: ptr::null_mut(),
            offsets: [0; 3],
            strides: [0; 3],
            timestamp: 0,
        }
    }
}

// ---------------------------------------------------------------- FileIO

pub mod fio {
    pub const SUCCESS: u32 = 0;
    pub const IN_USE: u32 = 1;
    pub const ERROR: u32 = 2;
}

#[repr(C)]
pub struct FileIo {
    vptr: *const usize,
    id: usize,
    client: *mut Obj,
    name: Vec<u8>,
    open: bool,
    busy: bool,
}

fn file<'a>(this: *mut FileIo) -> &'a mut FileIo {
    // Safety: the CDM only has pointers we made; `close` removes it from `files`.
    unsafe { &mut *this }
}

extern "C" fn fio_open(this: *mut FileIo, name: *const u8, n: u32) {
    let f = file(this);
    let name = unsafe { bytes(name, n) };
    let status = if f.open || f.busy {
        fio::IN_USE
    } else if st().store.is_none() || !store::name_ok(name) {
        fio::ERROR
    } else {
        f.name = name.to_vec();
        f.open = true;
        fio::SUCCESS
    };
    f.busy = status == fio::SUCCESS;
    st().deferred.push_back(Deferred::Open(f.id, status));
}

extern "C" fn fio_read(this: *mut FileIo) {
    let f = file(this);
    if !f.open || f.busy {
        st().deferred.push_back(Deferred::Read(f.id, if f.open { fio::IN_USE } else { fio::ERROR }, Vec::new()));
        return;
    }
    f.busy = true;
    let r = st().store.as_ref().map(|s| s.read(&f.name));
    let (status, data) = match r {
        Some(Ok(d)) => (fio::SUCCESS, d),
        _ => (fio::ERROR, Vec::new()),
    };
    st().deferred.push_back(Deferred::Read(f.id, status, data));
}

extern "C" fn fio_write(this: *mut FileIo, d: *const u8, n: u32) {
    let f = file(this);
    if !f.open || f.busy {
        st().deferred.push_back(Deferred::Write(f.id, if f.open { fio::IN_USE } else { fio::ERROR }));
        return;
    }
    f.busy = true;
    let data = unsafe { bytes(d, n) };
    let r = st().store.as_mut().map(|s| s.write(&f.name, data));
    let status = match r {
        Some(Ok(())) => fio::SUCCESS,
        Some(Err(e)) => {
            hlog!("hospes-cdm: record write: {:?}", e);
            fio::ERROR
        }
        None => fio::ERROR,
    };
    st().deferred.push_back(Deferred::Write(f.id, status));
}

extern "C" fn fio_close(this: *mut FileIo) {
    let id = file(this).id;
    // Drops the object; nothing touches `this` afterwards, and queued results for it are
    // skipped (Chromium: no callbacks after Close).
    st().files[id] = None;
}

static FILE_VT: VTable<6> = VTable {
    offset_to_top: 0,
    rtti: ptr::null(),
    slots: [fio_open as *const (), fio_read as *const (), fio_write as *const (), fio_close as *const (), never as *const (), never as *const ()],
};

// ---------------------------------------------------------------- Host_10

extern "C" fn h_allocate(_this: *mut Obj, cap: u32) -> *mut BufferObj {
    allocate(cap)
}
extern "C" fn h_set_timer(_this: *mut Obj, delay_ms: i64, ctx: usize) {
    let at = sys::monotonic_ns().saturating_add((delay_ms.clamp(0, 86_400_000) as u64) * 1_000_000);
    st().timers.push((at, ctx));
}
extern "C" fn h_wall_time(_this: *mut Obj) -> f64 {
    let (s, ns) = sys::realtime();
    s as f64 + ns as f64 / 1e9
}
extern "C" fn h_on_initialized(_this: *mut Obj, ok: bool) {
    send(&Msg::Initialized { ok });
}
extern "C" fn h_key_status(_this: *mut Obj, promise: u32, status: u32) {
    send(&Msg::KeyStatus { promise, status });
}
extern "C" fn h_new_session(_this: *mut Obj, promise: u32, id: *const u8, n: u32) {
    send(&Msg::NewSession { promise, id: unsafe { bytes(id, n) }.to_vec() });
}
extern "C" fn h_resolve(_this: *mut Obj, promise: u32) {
    send(&Msg::Resolved { promise });
}
extern "C" fn h_reject(_this: *mut Obj, promise: u32, exception: u32, system_code: u32, m: *const u8, n: u32) {
    send(&Msg::Rejected { promise, exception, system_code, message: unsafe { bytes(m, n.min(1024)) }.to_vec() });
}
extern "C" fn h_session_message(_this: *mut Obj, id: *const u8, idn: u32, ty: u32, m: *const u8, n: u32) {
    send(&Msg::SessionMessage { id: unsafe { bytes(id, idn) }.to_vec(), message_type: ty, message: unsafe { bytes(m, n) }.to_vec() });
}
extern "C" fn h_keys_change(_this: *mut Obj, id: *const u8, idn: u32, has_new: bool, k: *const KeyInformation, n: u32) {
    let mut keys = Vec::new();
    for i in 0..n.min(hospes_proto::MAX_ITEMS) as usize {
        // Safety: the CDM passes `n` entries.
        let ki = unsafe { &*k.add(i) };
        keys.push(KeyInfo { key_id: unsafe { bytes(ki.key_id, ki.key_id_size.min(1024)) }.to_vec(), status: ki.status, system_code: ki.system_code });
    }
    send(&Msg::KeysChange { id: unsafe { bytes(id, idn) }.to_vec(), has_new_usable: has_new, keys });
}
extern "C" fn h_expiration(_this: *mut Obj, id: *const u8, n: u32, t: f64) {
    send(&Msg::Expiration { id: unsafe { bytes(id, n) }.to_vec(), time: t });
}
extern "C" fn h_closed(_this: *mut Obj, id: *const u8, n: u32) {
    send(&Msg::Closed { id: unsafe { bytes(id, n) }.to_vec() });
}
extern "C" fn h_platform_challenge(_this: *mut Obj, _s: *const u8, _sn: u32, _c: *const u8, _cn: u32) {
    // No platform attestation (§8.2): answer with an empty response.
    st().deferred.push_back(Deferred::Challenge);
}
extern "C" fn h_enable_output_protection(_this: *mut Obj, _mask: u32) {}
extern "C" fn h_query_output_protection(_this: *mut Obj) {
    st().deferred.push_back(Deferred::Protection);
}
extern "C" fn h_deferred_init_done(_this: *mut Obj, _stream: u32, _status: u32) {}
extern "C" fn h_create_file_io(_this: *mut Obj, client: *mut Obj) -> *mut FileIo {
    let s = st();
    if client.is_null() {
        return ptr::null_mut();
    }
    let id = s.files.iter().position(|f| f.is_none()).unwrap_or_else(|| {
        s.files.push(None);
        s.files.len() - 1
    });
    let mut f = Box::new(FileIo { vptr: FILE_VT.slots.as_ptr() as *const usize, id, client, name: Vec::new(), open: false, busy: false });
    let p: *mut FileIo = &mut *f;
    s.files[id] = Some(f);
    p
}
extern "C" fn h_request_storage_id(_this: *mut Obj, version: u32) {
    st().deferred.push_back(Deferred::StorageId(version));
}

static HOST_VT: VTable<20> = VTable {
    offset_to_top: 0,
    rtti: ptr::null(),
    slots: [
        h_allocate as *const (),
        h_set_timer as *const (),
        h_wall_time as *const (),
        h_on_initialized as *const (),
        h_key_status as *const (),
        h_new_session as *const (),
        h_resolve as *const (),
        h_reject as *const (),
        h_session_message as *const (),
        h_keys_change as *const (),
        h_expiration as *const (),
        h_closed as *const (),
        h_platform_challenge as *const (),
        h_enable_output_protection as *const (),
        h_query_output_protection as *const (),
        h_deferred_init_done as *const (),
        h_create_file_io as *const (),
        h_request_storage_id as *const (),
        never as *const (),
        never as *const (),
    ],
};

#[repr(C)]
pub struct HostObj {
    vptr: *const usize,
}
// Safety: immutable after construction.
unsafe impl Sync for HostObj {}
pub static HOST: HostObj = HostObj { vptr: HOST_VT.slots.as_ptr() as *const usize };

/// `GetCdmHostFunc`: the CDM asks for the host interface it speaks.
pub extern "C" fn get_host(version: i32, _user: *mut c_void) -> *mut c_void {
    if version == 10 {
        &HOST as *const HostObj as *mut c_void
    } else {
        ptr::null_mut()
    }
}

/// Deliver one queued result. Returns false when the queue is empty.
pub fn run_one_deferred() -> bool {
    let Some(d) = st().deferred.pop_front() else { return false };
    let s = st();
    let cdm = match &s.cdm {
        Some(c) => Cdm(c.0),
        None => return true,
    };
    let client = |id: usize| s.files.get(id).and_then(|f| f.as_ref()).map(|f| FileClient(f.client));
    match d {
        Deferred::Open(id, status) => {
            if let Some(c) = client(id) {
                if let Some(Some(f)) = st().files.get_mut(id) {
                    f.busy = false;
                }
                c.on_open_complete(status);
            }
        }
        Deferred::Read(id, status, data) => {
            if let Some(c) = client(id) {
                if let Some(Some(f)) = st().files.get_mut(id) {
                    f.busy = false;
                }
                c.on_read_complete(status, &data);
            }
        }
        Deferred::Write(id, status) => {
            if let Some(c) = client(id) {
                if let Some(Some(f)) = st().files.get_mut(id) {
                    f.busy = false;
                }
                c.on_write_complete(status);
            }
        }
        Deferred::Protection => cdm.on_query_output_protection_status(0, st().link_mask, 0),
        Deferred::StorageId(v) => {
            let id = st().store.as_ref().map(|s| s.storage_id());
            match id {
                Some(Ok(id)) if v <= 1 => cdm.on_storage_id(1, &id),
                _ => cdm.on_storage_id(v, &[]),
            }
        }
        Deferred::Challenge => {
            let r = abi::PlatformChallengeResponse {
                signed_data: ptr::null(),
                signed_data_length: 0,
                signed_data_signature: ptr::null(),
                signed_data_signature_length: 0,
                platform_key_certificate: ptr::null(),
                platform_key_certificate_length: 0,
            };
            cdm.on_platform_challenge_response(&r);
        }
    }
    true
}
