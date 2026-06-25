//! Real PAM authentication via libpam FFI — the `auth` + `account` facilities
//! only (Atrium does its own session establishment). OpenPAM ABI (FreeBSD base +
//! macOS host). Feature-gated (`--features pam`) so the default build links no
//! libpam.
//!
//! This lives in PORTCULLISD, the root broker, on purpose: `pam_unix`
//! crypt-compares against `master.passwd`, which needs root — and the session
//! launcher (ostiarius) is now a jailed, non-root process that can't read it. So
//! the jailed ostiarius forwards the credential over `VerifyCredential` and
//! portcullisd runs the PAM stack here; the shadow hashes never leave this
//! process. See docs/spec/ostiarius-privsep.md §2.2.
//!
//! Flow: `pam_start(service, user, &conv)` → `pam_authenticate` → `pam_acct_mgmt`
//! → `pam_end`. The conversation callback supplies the password (no tty prompt).

use libc::{c_char, c_int, c_void};
use std::ffi::CString;
use std::ptr;

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_CONV_ERR: c_int = 19;
const PAM_BUF_ERR: c_int = 5;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}
#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}
#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}
type ConvFn = extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

enum PamHandle {}

#[link(name = "pam")]
extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, pamh: *mut *mut PamHandle) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut PamHandle, status: c_int) -> c_int;
}

/// The conversation callback: for each ECHO_OFF prompt (the password), hand back a
/// `strdup` of the password from `appdata_ptr`. PAM owns and `free`s the response
/// array + strings, so they must come from libc `calloc`/`strdup`.
extern "C" fn converse(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || appdata.is_null() || msg.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    let responses = unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) as *mut PamResponse };
    if responses.is_null() {
        return PAM_BUF_ERR;
    }
    let password = appdata as *const c_char;
    for i in 0..n {
        let m = unsafe { *msg.add(i) };
        let r = unsafe { &mut *responses.add(i) };
        r.resp_retcode = 0;
        r.resp = if !m.is_null() && unsafe { (*m).msg_style } == PAM_PROMPT_ECHO_OFF {
            unsafe { libc::strdup(password) }
        } else {
            ptr::null_mut()
        };
    }
    unsafe { *resp = responses };
    PAM_SUCCESS
}

/// Authenticate `user`/`password` against the PAM `service` (`/etc/pam.d/<service>`):
/// run the auth stack, then account management. `Ok(())` only if both pass.
pub fn authenticate(service: &str, user: &str, password: &str) -> Result<(), String> {
    let svc = CString::new(service).map_err(|_| "bad service".to_string())?;
    let usr = CString::new(user).map_err(|_| "bad user".to_string())?;
    let pw = CString::new(password).map_err(|_| "bad password".to_string())?;

    let conv = PamConv { conv: converse, appdata_ptr: pw.as_ptr() as *mut c_void };
    let mut pamh: *mut PamHandle = ptr::null_mut();

    let rc = unsafe { pam_start(svc.as_ptr(), usr.as_ptr(), &conv, &mut pamh) };
    if rc != PAM_SUCCESS || pamh.is_null() {
        return Err(format!("pam_start failed ({rc})"));
    }
    let auth = unsafe { pam_authenticate(pamh, 0) };
    let acct = if auth == PAM_SUCCESS { unsafe { pam_acct_mgmt(pamh, 0) } } else { auth };
    unsafe { pam_end(pamh, acct) };

    if auth != PAM_SUCCESS {
        return Err("authentication failed".into());
    }
    if acct != PAM_SUCCESS {
        return Err("account not permitted".into());
    }
    Ok(())
}
