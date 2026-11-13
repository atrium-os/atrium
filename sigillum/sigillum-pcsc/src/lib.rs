//! The pcsc-lite client API a vendor PKCS#11 module imports (`SCard*`, Linux
//! LP64 ABI: `DWORD` and `LONG` are 64 bits), answered from one CCID reader
//! that the `sigillum-p11` host opened (sigillum.md §16.5). There is no
//! pcscd: the module's process holds the token's USB endpoints itself, in its
//! jail, and nothing else can reach them.
//!
//! One reader, named by the host. Contexts and card handles are plain
//! counters. Calls are serialised by one lock, so the module's threads see
//! one card, as they would through pcscd with an exclusive share. APDUs are
//! never logged: a `VERIFY` carries the PIN.
#![no_std]
#![allow(non_snake_case, clippy::missing_safety_doc)]
extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_ulong, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use sigillum_ccid::atr::Atr;
use sigillum_ccid::reader::{Bulk, Reader};
use sigillum_ccid::{msg, Error};

pub type Dword = c_ulong;
pub type Long = i64;
pub type Context = i64;
pub type Handle = i64;

// pcsc-lite's pcsclite.h values.
pub const SCARD_S_SUCCESS: Long = 0;
pub const SCARD_F_INTERNAL_ERROR: Long = 0x8010_0001;
pub const SCARD_E_INVALID_HANDLE: Long = 0x8010_0003;
pub const SCARD_E_INVALID_PARAMETER: Long = 0x8010_0004;
pub const SCARD_E_INSUFFICIENT_BUFFER: Long = 0x8010_0008;
pub const SCARD_E_UNKNOWN_READER: Long = 0x8010_0009;
pub const SCARD_E_TIMEOUT: Long = 0x8010_000A;
pub const SCARD_E_NO_SMARTCARD: Long = 0x8010_000C;
pub const SCARD_E_PROTO_MISMATCH: Long = 0x8010_000F;
pub const SCARD_E_INVALID_VALUE: Long = 0x8010_0011;
pub const SCARD_F_COMM_ERROR: Long = 0x8010_0013;
pub const SCARD_E_NOT_TRANSACTED: Long = 0x8010_0016;
pub const SCARD_E_READER_UNAVAILABLE: Long = 0x8010_0017;
pub const SCARD_E_NO_READERS_AVAILABLE: Long = 0x8010_002E;
pub const SCARD_W_UNRESPONSIVE_CARD: Long = 0x8010_0066;
pub const SCARD_W_REMOVED_CARD: Long = 0x8010_0069;

pub const SCARD_PROTOCOL_T0: Dword = 1;
pub const SCARD_PROTOCOL_T1: Dword = 2;
pub const SCARD_PROTOCOL_RAW: Dword = 4;
pub const SCARD_SHARE_DIRECT: Dword = 3;
pub const SCARD_LEAVE_CARD: Dword = 0;
pub const SCARD_RESET_CARD: Dword = 1;
pub const SCARD_UNPOWER_CARD: Dword = 2;
pub const SCARD_EJECT_CARD: Dword = 3;
pub const SCARD_AUTOALLOCATE: Dword = Dword::MAX;
pub const INFINITE: Dword = 0xFFFF_FFFF;

/// `SCardStatus` card states.
pub const SCARD_ABSENT: Dword = 0x0002;
pub const SCARD_PRESENT: Dword = 0x0004;
pub const SCARD_POWERED: Dword = 0x0010;
pub const SCARD_SPECIFIC: Dword = 0x0040;
/// `SCardGetStatusChange` reader states.
pub const SCARD_STATE_IGNORE: Dword = 0x0001;
pub const SCARD_STATE_CHANGED: Dword = 0x0002;
pub const SCARD_STATE_UNKNOWN: Dword = 0x0004;
pub const SCARD_STATE_EMPTY: Dword = 0x0010;
pub const SCARD_STATE_PRESENT: Dword = 0x0020;
pub const SCARD_STATE_INUSE: Dword = 0x0100;

pub const MAX_ATR_SIZE: usize = 33;
const PNP: &[u8] = b"\\\\?PnP?\\Notification";

#[repr(C)]
pub struct IoRequest {
    pub protocol: Dword,
    pub pci_length: Dword,
}

/// `SCARD_READERSTATE` (pcsc-lite, LP64): 80 bytes.
#[repr(C)]
pub struct ReaderState {
    pub reader: *const c_char,
    pub user: *mut c_void,
    pub current: Dword,
    pub event: Dword,
    pub atr_len: Dword,
    pub atr: [u8; MAX_ATR_SIZE],
}
const _: () = assert!(core::mem::size_of::<ReaderState>() == 80);

/// What the PC/SC layer needs of a reader.
pub trait Card {
    /// Is a card in the slot (powered or not)?
    fn present(&mut self) -> Result<bool, Error>;
    fn power_on(&mut self) -> Result<Vec<u8>, Error>;
    fn power_off(&mut self) -> Result<(), Error>;
    /// One APDU, bytes in, response bytes (data and SW) out.
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>, Error>;
}

impl<B: Bulk> Card for Reader<B> {
    fn present(&mut self) -> Result<bool, Error> {
        Ok(self.slot_status()? != msg::ICC_ABSENT)
    }
    fn power_on(&mut self) -> Result<Vec<u8>, Error> {
        Ok(Reader::power_on(self)?.bytes.clone())
    }
    fn power_off(&mut self) -> Result<(), Error> {
        Reader::power_off(self)
    }
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>, Error> {
        self.transmit_raw(apdu)
    }
}

/// The host's side: the reader, its name, and how to wait.
pub struct Setup {
    pub card: Box<dyn Card + Send>,
    /// The name pcscd would give it, e.g. "Vendor Product [CCID Interface] 00 00".
    pub name: String,
    /// Sleep this many milliseconds (polling in `SCardGetStatusChange`).
    pub sleep_ms: fn(u32),
}

struct State {
    setup: Setup,
    contexts: Vec<Context>,
    next: i64,
    /// The connected handle, its protocol, and the ATR of this power-up.
    card: Option<(Handle, Dword)>,
    atr: Option<Vec<u8>>,
    /// Bumped on every change the module could see: the upper 16 bits of an event state.
    events: u32,
}

struct Lock {
    busy: AtomicBool,
    state: UnsafeCell<Option<State>>,
}
unsafe impl Sync for Lock {}

static LOCK: Lock = Lock { busy: AtomicBool::new(false), state: UnsafeCell::new(None) };

fn with<R>(f: impl FnOnce(&mut Option<State>) -> R) -> R {
    while LOCK.busy.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    // Safety: the flag gives this thread the only reference.
    let r = f(unsafe { &mut *LOCK.state.get() });
    LOCK.busy.store(false, Ordering::Release);
    r
}

/// Give the PC/SC layer its reader; once, before the module is loaded.
pub fn install(setup: Setup) {
    with(|s| *s = Some(State { setup, contexts: Vec::new(), next: 0x1000, card: None, atr: None, events: 0 }));
}

/// Forget the reader (tests; and a host shutting the token down).
pub fn uninstall() {
    with(|s| *s = None);
}

fn map(e: Error) -> Long {
    match e {
        Error::NoCard => SCARD_W_REMOVED_CARD,
        Error::Io => SCARD_E_READER_UNAVAILABLE,
        Error::TooLong => SCARD_E_INSUFFICIENT_BUFFER,
        Error::Protocol(_) | Error::Slot(_) | Error::Unsupported(_) => SCARD_F_COMM_ERROR,
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> &'a [u8] {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    core::slice::from_raw_parts(p as *const u8, n)
}

/// Memory handed out under `SCARD_AUTOALLOCATE`: a length word, then the bytes.
fn give(bytes: &[u8]) -> *mut u8 {
    let mut v = Vec::with_capacity(8 + bytes.len());
    v.extend_from_slice(&(bytes.len() as u64).to_ne_bytes());
    v.extend_from_slice(bytes);
    let b = Box::into_raw(v.into_boxed_slice()) as *mut u8;
    unsafe { b.add(8) }
}

/// Copy `bytes` out the pcsc-lite way: `*len` is the room in (or AUTOALLOCATE,
/// then `out` is a `*mut *mut u8`), the size needed out. A null `out` only asks
/// the size.
unsafe fn copy_out(bytes: &[u8], out: *mut u8, len: *mut Dword) -> Long {
    if len.is_null() {
        return SCARD_E_INVALID_PARAMETER;
    }
    let room = *len;
    *len = bytes.len() as Dword;
    if out.is_null() {
        return SCARD_S_SUCCESS;
    }
    if room == SCARD_AUTOALLOCATE {
        *(out as *mut *mut u8) = give(bytes);
        return SCARD_S_SUCCESS;
    }
    if (room as usize) < bytes.len() {
        return SCARD_E_INSUFFICIENT_BUFFER;
    }
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
    SCARD_S_SUCCESS
}

fn context_ok(st: &State, h: Context) -> bool {
    st.contexts.contains(&h)
}

fn power(st: &mut State) -> Result<Vec<u8>, Long> {
    if let Some(a) = &st.atr {
        return Ok(a.clone());
    }
    let atr = st.setup.card.power_on().map_err(|e| if e == Error::NoCard { SCARD_E_NO_SMARTCARD } else { map(e) })?;
    st.atr = Some(atr.clone());
    st.events = st.events.wrapping_add(1);
    Ok(atr)
}

/// The protocol to use: T=1 when both sides offer it, else T=0.
fn pick(atr: &[u8], wanted: Dword) -> Option<Dword> {
    let a = Atr::parse(atr).ok()?;
    if a.t1 && wanted & SCARD_PROTOCOL_T1 != 0 {
        Some(SCARD_PROTOCOL_T1)
    } else if a.t0 && wanted & SCARD_PROTOCOL_T0 != 0 {
        Some(SCARD_PROTOCOL_T0)
    } else {
        None
    }
}

fn dispose(st: &mut State, how: Dword) -> Result<(), Long> {
    match how {
        SCARD_LEAVE_CARD => Ok(()),
        SCARD_RESET_CARD => {
            st.atr = None;
            let _ = st.setup.card.power_off();
            power(st).map(|_| ())
        }
        SCARD_UNPOWER_CARD | SCARD_EJECT_CARD => {
            st.atr = None;
            st.events = st.events.wrapping_add(1);
            st.setup.card.power_off().map_err(map)
        }
        _ => Err(SCARD_E_INVALID_VALUE),
    }
}

/// Each call's name and result to the host's log (never a buffer, a length or an APDU).
fn trace(call: &str, rv: Long) {
    platform::log(format_args!("pcsc: {call} -> {rv:#x}"));
}

// ---- the API ----------------------------------------------------------------------

pub unsafe extern "C" fn SCardEstablishContext(_scope: Dword, _r1: *const c_void, _r2: *const c_void, ctx: *mut Context) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        if ctx.is_null() {
            return SCARD_E_INVALID_PARAMETER;
        }
        with(|s| {
            let Some(st) = s else { return SCARD_E_NO_READERS_AVAILABLE };
            st.next += 1;
            st.contexts.push(st.next);
            *ctx = st.next;
            SCARD_S_SUCCESS
        })
    })();
    trace("SCardEstablishContext", r);
    r
}

pub unsafe extern "C" fn SCardReleaseContext(ctx: Context) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| {
            let Some(st) = s else { return SCARD_E_INVALID_HANDLE };
            let n = st.contexts.len();
            st.contexts.retain(|c| *c != ctx);
            if st.contexts.len() == n { SCARD_E_INVALID_HANDLE } else { SCARD_S_SUCCESS }
        })
    })();
    trace("SCardReleaseContext", r);
    r
}

pub unsafe extern "C" fn SCardFreeMemory(_ctx: Context, mem: *const c_void) -> Long {
    if !mem.is_null() {
        let p = (mem as *mut u8).sub(8);
        let n = u64::from_ne_bytes(*(p as *const [u8; 8])) as usize;
        drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(p, 8 + n)));
    }
    SCARD_S_SUCCESS
}

/// The one reader, as a multi-string (`name\0\0`).
pub unsafe extern "C" fn SCardListReaders(ctx: Context, _groups: *const c_char, out: *mut c_char, len: *mut Dword) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| {
            let Some(st) = s else { return SCARD_E_NO_READERS_AVAILABLE };
            if !context_ok(st, ctx) {
                return SCARD_E_INVALID_HANDLE;
            }
            let mut m = Vec::from(st.setup.name.as_bytes());
            m.extend_from_slice(&[0, 0]);
            copy_out(&m, out as *mut u8, len)
        })
    })();
    trace("SCardListReaders", r);
    r
}

pub unsafe extern "C" fn SCardConnect(
    ctx: Context,
    reader: *const c_char,
    share: Dword,
    protocols: Dword,
    card: *mut Handle,
    active: *mut Dword,
) -> Long {
    platform::log(format_args!("pcsc: SCardConnect share {share} protocols {protocols:#x}"));
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        if reader.is_null() || card.is_null() || active.is_null() {
            return SCARD_E_INVALID_PARAMETER;
        }
        with(|s| {
            let Some(st) = s else { return SCARD_E_NO_READERS_AVAILABLE };
            if !context_ok(st, ctx) {
                return SCARD_E_INVALID_HANDLE;
            }
            if cstr(reader) != st.setup.name.as_bytes() {
                return SCARD_E_UNKNOWN_READER;
            }
            if share == SCARD_SHARE_DIRECT && protocols == 0 {
                st.next += 1;
                st.card = Some((st.next, 0));
                *card = st.next;
                *active = 0;
                return SCARD_S_SUCCESS;
            }
            let atr = match power(st) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let Some(p) = pick(&atr, protocols) else { return SCARD_E_PROTO_MISMATCH };
            st.next += 1;
            st.card = Some((st.next, p));
            *card = st.next;
            *active = p;
            SCARD_S_SUCCESS
        })
    })();
    trace("SCardConnect", r);
    r
}

fn connected(st: &State, h: Handle) -> Option<Dword> {
    st.card.filter(|c| c.0 == h).map(|c| c.1)
}

pub unsafe extern "C" fn SCardReconnect(card: Handle, _share: Dword, protocols: Dword, init: Dword, active: *mut Dword) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| {
            let Some(st) = s else { return SCARD_E_INVALID_HANDLE };
            if connected(st, card).is_none() {
                return SCARD_E_INVALID_HANDLE;
            }
            if let Err(e) = dispose(st, init) {
                return e;
            }
            let atr = match power(st) {
                Ok(a) => a,
                Err(e) => return e,
            };
            let Some(p) = pick(&atr, protocols) else { return SCARD_E_PROTO_MISMATCH };
            st.card = Some((card, p));
            if !active.is_null() {
                *active = p;
            }
            SCARD_S_SUCCESS
        })
    })();
    trace("SCardReconnect", r);
    r
}

pub unsafe extern "C" fn SCardDisconnect(card: Handle, how: Dword) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| {
            let Some(st) = s else { return SCARD_E_INVALID_HANDLE };
            if connected(st, card).is_none() {
                return SCARD_E_INVALID_HANDLE;
            }
            st.card = None;
            match dispose(st, how) {
                Ok(()) => SCARD_S_SUCCESS,
                Err(e) => e,
            }
        })
    })();
    trace("SCardDisconnect", r);
    r
}

/// One process holds the token: a transaction only needs a live handle.
pub unsafe extern "C" fn SCardBeginTransaction(card: Handle) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| match s {
            Some(st) if connected(st, card).is_some() => SCARD_S_SUCCESS,
            _ => SCARD_E_INVALID_HANDLE,
        })
    })();
    trace("SCardBeginTransaction", r);
    r
}

pub unsafe extern "C" fn SCardEndTransaction(card: Handle, how: Dword) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| {
            let Some(st) = s else { return SCARD_E_INVALID_HANDLE };
            if connected(st, card).is_none() {
                return SCARD_E_INVALID_HANDLE;
            }
            match dispose(st, how) {
                Ok(()) => SCARD_S_SUCCESS,
                Err(_) => SCARD_E_NOT_TRANSACTED,
            }
        })
    })();
    trace("SCardEndTransaction", r);
    r
}

pub unsafe extern "C" fn SCardStatus(
    card: Handle,
    name: *mut c_char,
    name_len: *mut Dword,
    state: *mut Dword,
    protocol: *mut Dword,
    atr: *mut u8,
    atr_len: *mut Dword,
) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        with(|s| {
            let Some(st) = s else { return SCARD_E_INVALID_HANDLE };
            let Some(p) = connected(st, card) else { return SCARD_E_INVALID_HANDLE };
            let a = st.atr.clone().unwrap_or_default();
            if !state.is_null() {
                *state = if a.is_empty() { SCARD_PRESENT } else { SCARD_SPECIFIC | SCARD_POWERED | SCARD_PRESENT };
            }
            if !protocol.is_null() {
                *protocol = p;
            }
            if !name_len.is_null() {
                let mut m = Vec::from(st.setup.name.as_bytes());
                m.extend_from_slice(&[0, 0]);
                let r = copy_out(&m, name as *mut u8, name_len);
                if r != SCARD_S_SUCCESS {
                    return r;
                }
            }
            if !atr_len.is_null() {
                return copy_out(&a, atr, atr_len);
            }
            SCARD_S_SUCCESS
        })
    })();
    trace("SCardStatus", r);
    r
}

/// The reader's state as `SCardGetStatusChange` reports it, and the ATR.
fn reader_state(st: &mut State) -> (Dword, Vec<u8>) {
    match st.setup.card.present() {
        Ok(true) => {
            let atr = st.atr.clone().or_else(|| power(st).ok()).unwrap_or_default();
            let inuse = if st.card.is_some() { SCARD_STATE_INUSE } else { 0 };
            (SCARD_STATE_PRESENT | inuse, atr)
        }
        Ok(false) => {
            if st.atr.take().is_some() {
                st.events = st.events.wrapping_add(1);
            }
            (SCARD_STATE_EMPTY, Vec::new())
        }
        Err(_) => (SCARD_STATE_UNKNOWN, Vec::new()),
    }
}

pub unsafe extern "C" fn SCardGetStatusChange(ctx: Context, timeout: Dword, states: *mut ReaderState, n: Dword) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        if n > 0 && states.is_null() {
            return SCARD_E_INVALID_PARAMETER;
        }
        let rs = core::slice::from_raw_parts_mut(states, n as usize);
        let mut waited: u64 = 0;
        loop {
            let (r, sleep) = with(|s| {
                let Some(st) = s else { return (SCARD_E_NO_READERS_AVAILABLE, None) };
                if !context_ok(st, ctx) {
                    return (SCARD_E_INVALID_HANDLE, None);
                }
                let (now, atr) = reader_state(st);
                let now = now | ((st.events as Dword & 0xFFFF) << 16);
                let mut changed = false;
                for r in rs.iter_mut() {
                    if r.reader.is_null() {
                        return (SCARD_E_INVALID_VALUE, None);
                    }
                    let name = cstr(r.reader);
                    if name == PNP {
                        // No reader comes or goes: the token is the reader.
                        r.event = r.current & !SCARD_STATE_CHANGED;
                        continue;
                    }
                    if r.current & SCARD_STATE_IGNORE != 0 {
                        r.event = SCARD_STATE_IGNORE;
                        continue;
                    }
                    if name != st.setup.name.as_bytes() {
                        r.event = SCARD_STATE_UNKNOWN | SCARD_STATE_CHANGED | SCARD_STATE_IGNORE;
                        changed = true;
                        continue;
                    }
                    let differs = (r.current & 0xFFFF & !SCARD_STATE_CHANGED) != (now & 0xFFFF)
                        || (r.current >> 16 != 0 && r.current >> 16 != now >> 16);
                    r.event = if differs { now | SCARD_STATE_CHANGED } else { now };
                    r.atr_len = atr.len().min(MAX_ATR_SIZE) as Dword;
                    r.atr[..atr.len().min(MAX_ATR_SIZE)].copy_from_slice(&atr[..atr.len().min(MAX_ATR_SIZE)]);
                    changed |= differs;
                }
                if changed || rs.is_empty() {
                    (SCARD_S_SUCCESS, None)
                } else {
                    (SCARD_E_TIMEOUT, Some(st.setup.sleep_ms))
                }
            });
            let Some(sleep) = sleep else { return r };
            if timeout != INFINITE && waited >= timeout as u64 {
                return SCARD_E_TIMEOUT;
            }
            let step = if timeout == INFINITE { 250 } else { (timeout as u64 - waited).min(250) };
            sleep(step as u32);
            waited += step;
        }
    })();
    trace("SCardGetStatusChange", r);
    r
}

pub unsafe extern "C" fn SCardTransmit(
    card: Handle,
    send_pci: *const IoRequest,
    send: *const u8,
    send_len: Dword,
    _recv_pci: *mut IoRequest,
    recv: *mut u8,
    recv_len: *mut Dword,
) -> Long {
    #[allow(clippy::redundant_closure_call)]
    let r = (|| {
        if send.is_null() || recv.is_null() || recv_len.is_null() || send_len == 0 {
            return SCARD_E_INVALID_PARAMETER;
        }
        with(|s| {
            let Some(st) = s else { return SCARD_E_INVALID_HANDLE };
            let Some(p) = connected(st, card) else { return SCARD_E_INVALID_HANDLE };
            if !send_pci.is_null() && (*send_pci).protocol != p && (*send_pci).protocol != SCARD_PROTOCOL_RAW {
                return SCARD_E_PROTO_MISMATCH;
            }
            if st.atr.is_none() {
                return SCARD_W_REMOVED_CARD;
            }
            let apdu = core::slice::from_raw_parts(send, send_len as usize);
            match st.setup.card.transmit(apdu) {
                Ok(r) => {
                    if (*recv_len as usize) < r.len() {
                        *recv_len = r.len() as Dword;
                        return SCARD_E_INSUFFICIENT_BUFFER;
                    }
                    core::ptr::copy_nonoverlapping(r.as_ptr(), recv, r.len());
                    *recv_len = r.len() as Dword;
                    SCARD_S_SUCCESS
                }
                Err(Error::NoCard) => {
                    st.atr = None;
                    st.events = st.events.wrapping_add(1);
                    SCARD_W_REMOVED_CARD
                }
                Err(e) => map(e),
            }
        })
    })();
    // The result code only: the bytes may hold a PIN.
    trace("SCardTransmit", r);
    r
}

/// Every `SCard*` the module may import, by name, for the host's resolver.
pub fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "SCardEstablishContext" => SCardEstablishContext as *const () as usize,
        "SCardReleaseContext" => SCardReleaseContext as *const () as usize,
        "SCardFreeMemory" => SCardFreeMemory as *const () as usize,
        "SCardListReaders" => SCardListReaders as *const () as usize,
        "SCardConnect" => SCardConnect as *const () as usize,
        "SCardReconnect" => SCardReconnect as *const () as usize,
        "SCardDisconnect" => SCardDisconnect as *const () as usize,
        "SCardBeginTransaction" => SCardBeginTransaction as *const () as usize,
        "SCardEndTransaction" => SCardEndTransaction as *const () as usize,
        "SCardStatus" => SCardStatus as *const () as usize,
        "SCardGetStatusChange" => SCardGetStatusChange as *const () as usize,
        "SCardTransmit" => SCardTransmit as *const () as usize,
        _ => return None,
    })
}

/// What the host's resolver asks first, with the import's symbol version: `SCard*` and
/// `curl_*` are imported unversioned (pcsc-lite and the module's libcurl have none),
/// libudev's as `@LIBUDEV_183`. `None` sends the import on to the shim.
pub fn resolve_import(name: &str, version: Option<&str>) -> Option<usize> {
    match version {
        None => resolve(name).or_else(|| platform::resolve(name)),
        Some("LIBUDEV_183") if name.starts_with("udev_") => platform::resolve(name),
        Some(_) => None,
    }
}

pub mod platform;

#[cfg(test)]
mod tests;
