//! The PKCS#11 calls Sigillum makes, and no others (sigillum.md §16.5).
//!
//! [`Module::new`] copies fourteen entries out of the module's
//! `CK_FUNCTION_LIST` and drops the list: `C_InitToken`, `C_InitPIN`,
//! `C_SetPIN`, object creation and deletion, key generation, and the
//! vendor's `C_InnaIT*` exports are never reachable from here. The mechanisms
//! are fixed too: `CKM_RSA_PKCS` over a `DigestInfo`, `CKM_RSA_PKCS_PSS`,
//! `CKM_ECDSA`, all over a digest Sigillum computed.
//!
//! Linux PKCS#11 ABI: `CK_ULONG` is 64 bits, structures are not packed.
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use sigillum_proto::driver::{HashAlg, KeyKind, SignScheme, TokenCert, RETRIES_UNKNOWN};
use sigillum_proto::driver::{DriverError, Pin};

pub type Ulong = u64;
pub type Rv = Ulong;

pub const CKR_OK: Rv = 0;
pub const CKR_PIN_INCORRECT: Rv = 0xA0;
pub const CKR_PIN_LEN_RANGE: Rv = 0xA2;
pub const CKR_PIN_EXPIRED: Rv = 0xA3;
pub const CKR_PIN_LOCKED: Rv = 0xA4;
pub const CKR_USER_ALREADY_LOGGED_IN: Rv = 0x100;
pub const CKR_DEVICE_REMOVED: Rv = 0x32;
pub const CKR_TOKEN_NOT_PRESENT: Rv = 0xE0;
pub const CKR_CRYPTOKI_ALREADY_INITIALIZED: Rv = 0x191;

const CKF_OS_LOCKING_OK: Ulong = 2;
const CKF_SERIAL_SESSION: Ulong = 4;
const CKF_LOGIN_REQUIRED: Ulong = 0x4;
const CKF_USER_PIN_FINAL_TRY: Ulong = 0x2_0000;
const CKF_USER_PIN_LOCKED: Ulong = 0x4_0000;
const CKU_USER: Ulong = 1;

const CKA_CLASS: Ulong = 0x000;
const CKA_LABEL: Ulong = 0x003;
const CKA_VALUE: Ulong = 0x011;
const CKA_KEY_TYPE: Ulong = 0x100;
const CKA_ID: Ulong = 0x102;
const CKA_MODULUS_BITS: Ulong = 0x121;
const CKA_EC_PARAMS: Ulong = 0x180;
const CKO_CERTIFICATE: Ulong = 1;
const CKO_PRIVATE_KEY: Ulong = 3;
const CKK_RSA: Ulong = 0;
const CKK_EC: Ulong = 3;
/// `CK_UNAVAILABLE_INFORMATION`.
const UNAVAILABLE: Ulong = Ulong::MAX;

const CKM_RSA_PKCS: Ulong = 0x0001;
const CKM_RSA_PKCS_PSS: Ulong = 0x000D;
const CKM_ECDSA: Ulong = 0x1041;
const CKM_SHA256: Ulong = 0x250;
const CKM_SHA384: Ulong = 0x260;
const CKM_SHA512: Ulong = 0x270;
const CKG_MGF1_SHA256: Ulong = 2;
const CKG_MGF1_SHA384: Ulong = 3;
const CKG_MGF1_SHA512: Ulong = 4;

/// Longest signature we take (RSA 4096).
const MAX_SIG: usize = 512;
/// Longest attribute we read (a certificate).
const MAX_ATTR: usize = sigillum_proto::driver::cap::CERT;

#[repr(C)]
struct Attribute {
    kind: Ulong,
    value: *mut c_void,
    len: Ulong,
}
#[repr(C)]
struct Mechanism {
    kind: Ulong,
    param: *const c_void,
    param_len: Ulong,
}
#[repr(C)]
struct PssParams {
    hash: Ulong,
    mgf: Ulong,
    salt_len: Ulong,
}
#[repr(C)]
struct InitArgs {
    create_mutex: *const c_void,
    destroy_mutex: *const c_void,
    lock_mutex: *const c_void,
    unlock_mutex: *const c_void,
    flags: Ulong,
    reserved: *const c_void,
}
/// `CK_TOKEN_INFO`: 208 bytes on LP64.
#[repr(C)]
pub struct TokenInfo {
    pub label: [u8; 32],
    pub manufacturer: [u8; 32],
    pub model: [u8; 16],
    pub serial: [u8; 16],
    pub flags: Ulong,
    counts: [Ulong; 10],
    versions: [u8; 4],
    utc: [u8; 16],
}
const _: () = assert!(core::mem::size_of::<TokenInfo>() == 208);

/// `CK_FUNCTION_LIST`: a version, then 68 entries in the standard's order.
#[repr(C)]
struct FunctionList {
    version: [u8; 2],
    f: [usize; 68],
}

// Indices into `CK_FUNCTION_LIST` (PKCS#11 2.40 §5): the allow-list.
const C_INITIALIZE: usize = 0;
const C_FINALIZE: usize = 1;
const C_GET_SLOT_LIST: usize = 4;
const C_GET_TOKEN_INFO: usize = 6;
const C_OPEN_SESSION: usize = 12;
const C_CLOSE_SESSION: usize = 13;
const C_LOGIN: usize = 18;
const C_LOGOUT: usize = 19;
const C_GET_ATTRIBUTE_VALUE: usize = 24;
const C_FIND_OBJECTS_INIT: usize = 26;
const C_FIND_OBJECTS: usize = 27;
const C_FIND_OBJECTS_FINAL: usize = 28;
const C_SIGN_INIT: usize = 42;
const C_SIGN: usize = 43;
const ALLOWED: [usize; 14] = [
    C_INITIALIZE,
    C_FINALIZE,
    C_GET_SLOT_LIST,
    C_GET_TOKEN_INFO,
    C_OPEN_SESSION,
    C_CLOSE_SESSION,
    C_LOGIN,
    C_LOGOUT,
    C_GET_ATTRIBUTE_VALUE,
    C_FIND_OBJECTS_INIT,
    C_FIND_OBJECTS,
    C_FIND_OBJECTS_FINAL,
    C_SIGN_INIT,
    C_SIGN,
];

type F0 = unsafe extern "C" fn(*const c_void) -> Rv;
type FSlots = unsafe extern "C" fn(u8, *mut Ulong, *mut Ulong) -> Rv;
type FToken = unsafe extern "C" fn(Ulong, *mut TokenInfo) -> Rv;
type FOpen = unsafe extern "C" fn(Ulong, Ulong, *const c_void, *const c_void, *mut Ulong) -> Rv;
type FSess = unsafe extern "C" fn(Ulong) -> Rv;
type FLogin = unsafe extern "C" fn(Ulong, Ulong, *const u8, Ulong) -> Rv;
type FAttr = unsafe extern "C" fn(Ulong, Ulong, *mut Attribute, Ulong) -> Rv;
type FFindInit = unsafe extern "C" fn(Ulong, *const Attribute, Ulong) -> Rv;
type FFind = unsafe extern "C" fn(Ulong, *mut Ulong, Ulong, *mut Ulong) -> Rv;
type FSignInit = unsafe extern "C" fn(Ulong, *const Mechanism, Ulong) -> Rv;
type FSign = unsafe extern "C" fn(Ulong, *const u8, Ulong, *mut u8, *mut Ulong) -> Rv;

/// The fourteen allowed entries, nothing else.
pub struct Module {
    f: [usize; 14],
    slot: Ulong,
}

/// Why a call failed: the module's `CK_RV`, or a driver error to report.
#[derive(Debug)]
pub enum Fail {
    Rv(&'static str, Rv),
    Driver(DriverError),
    /// No PIN was given and the token needs one; how many tries it says are left.
    NeedPin(u8),
}

fn check(what: &'static str, rv: Rv) -> Result<(), Fail> {
    match rv {
        CKR_OK => Ok(()),
        CKR_DEVICE_REMOVED | CKR_TOKEN_NOT_PRESENT => Err(Fail::Driver(DriverError::Gone)),
        _ => Err(Fail::Rv(what, rv)),
    }
}

fn trim(b: &[u8]) -> String {
    let s = core::str::from_utf8(b).unwrap_or("");
    String::from(s.trim_end_matches([' ', '\0']))
}

impl Module {
    /// Copy the allowed entries out of `C_GetFunctionList`'s list, initialise the
    /// module (OS locking: it has threads) and find the token's slot.
    ///
    /// # Safety
    /// `get_function_list` is the module's exported `C_GetFunctionList`.
    pub unsafe fn new(get_function_list: usize) -> Result<Module, Fail> {
        let gfl: unsafe extern "C" fn(*mut *const FunctionList) -> Rv = core::mem::transmute(get_function_list);
        let mut list: *const FunctionList = core::ptr::null();
        check("C_GetFunctionList", gfl(&mut list))?;
        if list.is_null() || (*list).version[0] != 2 {
            return Err(Fail::Rv("C_GetFunctionList: not a PKCS#11 2.x list", 0));
        }
        let mut f = [0usize; 14];
        for (i, &k) in ALLOWED.iter().enumerate() {
            f[i] = (*list).f[k];
            if f[i] == 0 {
                return Err(Fail::Rv("C_GetFunctionList: an allowed entry is missing", 0));
            }
        }
        let mut m = Module { f, slot: 0 };
        let args = InitArgs {
            create_mutex: core::ptr::null(),
            destroy_mutex: core::ptr::null(),
            lock_mutex: core::ptr::null(),
            unlock_mutex: core::ptr::null(),
            flags: CKF_OS_LOCKING_OK,
            reserved: core::ptr::null(),
        };
        match m.call::<F0>(C_INITIALIZE)(&args as *const InitArgs as *const c_void) {
            CKR_OK | CKR_CRYPTOKI_ALREADY_INITIALIZED => {}
            rv => return Err(Fail::Rv("C_Initialize", rv)),
        }
        let mut n: Ulong = 0;
        check("C_GetSlotList", m.call::<FSlots>(C_GET_SLOT_LIST)(1, core::ptr::null_mut(), &mut n))?;
        if n == 0 {
            return Err(Fail::Driver(DriverError::Gone));
        }
        let mut slots = vec![0 as Ulong; n.min(16) as usize];
        let mut n2 = slots.len() as Ulong;
        check("C_GetSlotList", m.call::<FSlots>(C_GET_SLOT_LIST)(1, slots.as_mut_ptr(), &mut n2))?;
        m.slot = slots[0];
        Ok(m)
    }

    /// Entry `k` of the allow-list (an index into `CK_FUNCTION_LIST`).
    unsafe fn call<T: Copy>(&self, k: usize) -> T {
        let i = ALLOWED.iter().position(|&a| a == k).expect("allowed");
        let p = self.f[i];
        core::mem::transmute_copy::<usize, T>(&p)
    }

    pub fn token_info(&self) -> Result<TokenInfo, Fail> {
        // Safety: zeroed CK_TOKEN_INFO is a valid out-parameter.
        let mut ti: TokenInfo = unsafe { core::mem::zeroed() };
        check("C_GetTokenInfo", unsafe { self.call::<FToken>(C_GET_TOKEN_INFO)(self.slot, &mut ti) })?;
        Ok(ti)
    }

    fn open(&self) -> Result<Session<'_>, Fail> {
        let mut h: Ulong = 0;
        check("C_OpenSession", unsafe {
            self.call::<FOpen>(C_OPEN_SESSION)(self.slot, CKF_SERIAL_SESSION, core::ptr::null(), core::ptr::null(), &mut h)
        })?;
        Ok(Session { m: self, h, logged_in: false })
    }

    /// The token's label and its certificates, read without logging in.
    pub fn certificates(&self) -> Result<(String, Vec<TokenCert>), Fail> {
        let label = trim(&self.token_info()?.label);
        let s = self.open()?;
        let mut out = Vec::new();
        for o in s.find(CKO_CERTIFICATE, None, sigillum_proto::driver::cap::CERTS)? {
            let (Some(cert), Some(id)) = (s.attr(o, CKA_VALUE)?, s.attr(o, CKA_ID)?) else { continue };
            if cert.is_empty() || id.is_empty() || id.len() > sigillum_proto::driver::cap::KEY_ID {
                continue;
            }
            let label = s.attr(o, CKA_LABEL)?.map(|l| trim(&l)).unwrap_or_default();
            let label: String = label.chars().take(sigillum_proto::driver::cap::LABEL).collect();
            let key = s.key_kind(&id)?;
            out.push(TokenCert { key_id: id, label, cert, key });
        }
        Ok((label, out))
    }

    /// Sign `digest` with the key `key_id` names. With no PIN and a token that needs one,
    /// `Fail::NeedPin`.
    pub fn sign(&self, key_id: &[u8], scheme: SignScheme, digest: &[u8], pin: Option<&Pin>) -> Result<Vec<u8>, Fail> {
        let hash = match scheme {
            SignScheme::RsaPkcs1(h) | SignScheme::RsaPss(h) | SignScheme::Ecdsa(h) => h,
        };
        if digest.len() != hash.len() {
            return Err(Fail::Driver(DriverError::Device));
        }
        let ti = self.token_info()?;
        let mut s = self.open()?;
        if ti.flags & CKF_LOGIN_REQUIRED != 0 {
            let Some(pin) = pin else { return Err(Fail::NeedPin(retries(ti.flags))) };
            s.login(pin)?;
        }
        let keys = s.find(CKO_PRIVATE_KEY, Some(key_id), 2)?;
        let Some(&key) = keys.first() else { return Err(Fail::Driver(DriverError::NoKey)) };
        let pss;
        let (mech, input): (Mechanism, Vec<u8>) = match scheme {
            SignScheme::RsaPkcs1(h) => {
                (Mechanism { kind: CKM_RSA_PKCS, param: core::ptr::null(), param_len: 0 }, digest_info(h, digest))
            }
            SignScheme::RsaPss(h) => {
                let (hm, mgf) = match h {
                    HashAlg::Sha256 => (CKM_SHA256, CKG_MGF1_SHA256),
                    HashAlg::Sha384 => (CKM_SHA384, CKG_MGF1_SHA384),
                    HashAlg::Sha512 => (CKM_SHA512, CKG_MGF1_SHA512),
                };
                pss = PssParams { hash: hm, mgf, salt_len: h.len() as Ulong };
                (
                    Mechanism {
                        kind: CKM_RSA_PKCS_PSS,
                        param: &pss as *const PssParams as *const c_void,
                        param_len: core::mem::size_of::<PssParams>() as Ulong,
                    },
                    digest.to_vec(),
                )
            }
            SignScheme::Ecdsa(_) => (Mechanism { kind: CKM_ECDSA, param: core::ptr::null(), param_len: 0 }, digest.to_vec()),
        };
        check("C_SignInit", unsafe { self.call::<FSignInit>(C_SIGN_INIT)(s.h, &mech, key) })?;
        let mut sig = vec![0u8; MAX_SIG];
        let mut n = sig.len() as Ulong;
        check("C_Sign", unsafe { self.call::<FSign>(C_SIGN)(s.h, input.as_ptr(), input.len() as Ulong, sig.as_mut_ptr(), &mut n) })?;
        if n == 0 || n as usize > sig.len() {
            return Err(Fail::Driver(DriverError::Device));
        }
        sig.truncate(n as usize);
        Ok(sig)
    }

    pub fn finalize(self) {
        // Safety: allowed entry 1; no session is open.
        let _ = unsafe { self.call::<F0>(C_FINALIZE)(core::ptr::null()) };
    }
}

/// Tries left as the token's flags tell them: only "final try" is a number.
fn retries(flags: Ulong) -> u8 {
    if flags & CKF_USER_PIN_FINAL_TRY != 0 {
        1
    } else {
        RETRIES_UNKNOWN
    }
}

/// RFC 8017 §9.2's `DigestInfo` prefix, then the digest.
pub fn digest_info(h: HashAlg, digest: &[u8]) -> Vec<u8> {
    let prefix: &[u8] = match h {
        HashAlg::Sha256 => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20],
        HashAlg::Sha384 => &[0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30],
        HashAlg::Sha512 => &[0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40],
    };
    let mut v = prefix.to_vec();
    v.extend_from_slice(digest);
    v
}

struct Session<'a> {
    m: &'a Module,
    h: Ulong,
    logged_in: bool,
}

impl Session<'_> {
    fn login(&mut self, pin: &Pin) -> Result<(), Fail> {
        let p = &pin.0;
        let rv = unsafe { self.m.call::<FLogin>(C_LOGIN)(self.h, CKU_USER, p.as_ptr(), p.len() as Ulong) };
        match rv {
            CKR_OK | CKR_USER_ALREADY_LOGGED_IN => {
                self.logged_in = true;
                Ok(())
            }
            CKR_PIN_LOCKED => Err(Fail::Driver(DriverError::PinBlocked)),
            CKR_PIN_INCORRECT | CKR_PIN_LEN_RANGE | CKR_PIN_EXPIRED => {
                let flags = self.m.token_info().map(|t| t.flags).unwrap_or(0);
                if flags & CKF_USER_PIN_LOCKED != 0 {
                    Err(Fail::Driver(DriverError::PinBlocked))
                } else {
                    Err(Fail::Driver(DriverError::PinInvalid { retries: retries(flags) }))
                }
            }
            rv => check("C_Login", rv),
        }
    }

    /// Up to `max` objects of `class`, with `CKA_ID` = `id` if given.
    fn find(&self, class: Ulong, id: Option<&[u8]>, max: usize) -> Result<Vec<Ulong>, Fail> {
        let mut c = class;
        let mut t = vec![Attribute { kind: CKA_CLASS, value: &mut c as *mut Ulong as *mut c_void, len: 8 }];
        if let Some(id) = id {
            t.push(Attribute { kind: CKA_ID, value: id.as_ptr() as *mut c_void, len: id.len() as Ulong });
        }
        let f = &self.m;
        check("C_FindObjectsInit", unsafe { f.call::<FFindInit>(C_FIND_OBJECTS_INIT)(self.h, t.as_ptr(), t.len() as Ulong) })?;
        let mut objs = vec![0 as Ulong; max];
        let mut n: Ulong = 0;
        let r = check("C_FindObjects", unsafe { f.call::<FFind>(C_FIND_OBJECTS)(self.h, objs.as_mut_ptr(), max as Ulong, &mut n) });
        let _ = unsafe { f.call::<FSess>(C_FIND_OBJECTS_FINAL)(self.h) };
        r?;
        objs.truncate((n as usize).min(max));
        Ok(objs)
    }

    /// One attribute's bytes, `None` if the object doesn't have it (or it is sensitive).
    fn attr(&self, o: Ulong, kind: Ulong) -> Result<Option<Vec<u8>>, Fail> {
        let get = unsafe { self.m.call::<FAttr>(C_GET_ATTRIBUTE_VALUE) };
        let mut a = Attribute { kind, value: core::ptr::null_mut(), len: 0 };
        if unsafe { get(self.h, o, &mut a, 1) } != CKR_OK || a.len == UNAVAILABLE || a.len as usize > MAX_ATTR {
            return Ok(None);
        }
        let mut buf = vec![0u8; a.len as usize];
        a.value = buf.as_mut_ptr() as *mut c_void;
        check("C_GetAttributeValue", unsafe { get(self.h, o, &mut a, 1) })?;
        if a.len as usize > buf.len() {
            return Err(Fail::Driver(DriverError::Device));
        }
        buf.truncate(a.len as usize);
        Ok(Some(buf))
    }

    fn ulong(&self, o: Ulong, kind: Ulong) -> Result<Option<Ulong>, Fail> {
        Ok(self.attr(o, kind)?.filter(|v| v.len() == 8).map(|v| u64::from_ne_bytes(v[..8].try_into().unwrap())))
    }

    /// The private key's kind and size, if the key is visible without logging in.
    fn key_kind(&self, id: &[u8]) -> Result<KeyKind, Fail> {
        let Some(&k) = self.find(CKO_PRIVATE_KEY, Some(id), 1)?.first() else { return Ok(KeyKind::Other) };
        Ok(match self.ulong(k, CKA_KEY_TYPE)? {
            Some(CKK_RSA) => match self.ulong(k, CKA_MODULUS_BITS)? {
                Some(b) if b <= u16::MAX as u64 => KeyKind::Rsa { bits: b as u16 },
                _ => KeyKind::Other,
            },
            Some(CKK_EC) => match self.attr(k, CKA_EC_PARAMS)?.as_deref() {
                // The named curves' OIDs, DER.
                Some([0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]) => KeyKind::Ec { bits: 256 },
                Some([0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22]) => KeyKind::Ec { bits: 384 },
                Some([0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23]) => KeyKind::Ec { bits: 521 },
                _ => KeyKind::Other,
            },
            _ => KeyKind::Other,
        })
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // Safety: allowed entries; the handle is ours.
        unsafe {
            if self.logged_in {
                let _ = self.m.call::<FSess>(C_LOGOUT)(self.h);
            }
            let _ = self.m.call::<FSess>(C_CLOSE_SESSION)(self.h);
        }
    }
}
