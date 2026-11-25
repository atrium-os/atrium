//! Artifex plugins in C (dev-tools.md §3.8): the `editor-extension`
//! session of `atrium-artifex-ext` behind the C ABI in
//! `include/atrium_plugin.h`.
//!
//! A handle owns one session. Every string it hands out stays valid
//! until the next `atrium_plugin_next` or `atrium_plugin_free` on that
//! handle. Everything Artifex sends has been bounded by limend and the
//! SDK (an LSP message at most 4 MiB); the JSON a plugin passes in is
//! parsed before it goes out, so a malformed message is refused here.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::time::{Duration, Instant};

use atrium_artifex_ext::{path_of, Channel, Error, Event, Plugin, Value, WORKSPACE};

/// limend's channel failed.
pub const ATRIUM_PLUGIN_ERR_CHANNEL: c_int = -60;
/// limend closed the channel: the plugin is done.
pub const ATRIUM_PLUGIN_ERR_CLOSED: c_int = -61;
/// Artifex sent something out of shape, or a message was too big.
pub const ATRIUM_PLUGIN_ERR_PROTOCOL: c_int = -62;
/// A NULL handle or pointer, text that isn't UTF-8, or JSON that isn't.
pub const ATRIUM_PLUGIN_ERR_INVALID: c_int = -63;
/// No panel: no frescod connection, or frescod gave no embed token.
pub const ATRIUM_PLUGIN_ERR_NO_PANEL: c_int = -64;
/// The buffer is too small.
pub const ATRIUM_PLUGIN_ERR_BUF_TOO_SMALL: c_int = -65;

/// An LSP message: `json`.
pub const ATRIUM_PLUGIN_EV_LSP: u32 = 1;
/// A palette command: `command`, `file`, `line`.
pub const ATRIUM_PLUGIN_EV_COMMAND: u32 = 2;
/// The panel was shown or hidden: `shown`.
pub const ATRIUM_PLUGIN_EV_PANEL_SHOWN: u32 = 3;
/// Artifex closed it or limend is ending it: exit.
pub const ATRIUM_PLUGIN_EV_DETACHING: u32 = 4;

/// How long `atrium_plugin_open_panel` waits for frescod's embed token.
const TOKEN_WAIT: Duration = Duration::from_secs(5);
/// The longest wait `atrium_plugin_next` takes (a minute; poll again).
const MAX_WAIT_MS: u32 = 60_000;

/// `atrium_plugin_event_t`.
#[repr(C)]
#[allow(missing_docs)] // documented in atrium_plugin.h
pub struct AtriumPluginEvent {
    pub kind: u32,
    pub line: u32,
    pub shown: i32,
    pub _pad: i32,
    pub json: *const c_char,
    pub json_len: usize,
    pub command: *const c_char,
    pub file: *const c_char,
}

/// `atrium_plugin_t`: one session and the strings last handed out.
pub struct AtriumPlugin {
    p: Plugin<Box<dyn Channel>>,
    workspace: CString,
    languages: Vec<CString>,
    held: Vec<CString>,
    /// The frescod connection limend handed over, until a panel adopts it.
    fresco: Option<std::os::unix::net::UnixStream>,
    panel: Option<u32>,
}

fn code(e: &Error) -> c_int {
    match e {
        Error::Channel(_) => ATRIUM_PLUGIN_ERR_CHANNEL,
        Error::Closed => ATRIUM_PLUGIN_ERR_CLOSED,
        Error::Protocol(_) => ATRIUM_PLUGIN_ERR_PROTOCOL,
    }
}

fn res(r: Result<(), Error>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => code(&e),
    }
}

/// A string with no NUL in it (names from limend never have one; a
/// file's URI could, and is then refused rather than cut short).
fn cstring(s: String) -> Result<CString, c_int> {
    CString::new(s).map_err(|_| ATRIUM_PLUGIN_ERR_PROTOCOL)
}

/// Wrap a session attached on `ch`. The C start goes through limend's
/// channel; tests pass an in-memory one.
#[doc(hidden)]
pub fn attach(ch: Box<dyn Channel>, fresco: Option<std::os::unix::net::UnixStream>) -> Result<Box<AtriumPlugin>, c_int> {
    let p = Plugin::attach(ch).map_err(|e| code(&e))?;
    let workspace = cstring(p.init.workspace.clone())?;
    let languages = p.init.languages.iter().cloned().map(cstring).collect::<Result<_, _>>()?;
    Ok(Box::new(AtriumPlugin { p, workspace, languages, held: Vec::new(), fresco, panel: None }))
}

/// # Safety
/// `p` is NULL or a handle from `atrium_plugin_start` not yet freed.
unsafe fn handle<'a>(p: *mut AtriumPlugin) -> Option<&'a mut AtriumPlugin> {
    // SAFETY: the caller passes NULL or a live handle it owns; the C API
    // takes no handle concurrently (documented in the header).
    unsafe { p.as_mut() }
}

/// # Safety
/// `s` is NULL or a NUL-terminated string.
unsafe fn text<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated, per the caller.
    unsafe { CStr::from_ptr(s) }.to_str().ok()
}

/// Parse JSON text a plugin hands in; `None` when it isn't JSON.
unsafe fn json(s: *const c_char) -> Option<Value> {
    // SAFETY: forwarded from the caller's contract.
    serde_json::from_str(unsafe { text(s) }?).ok()
}

/// Start as limend's child: take the channel, wait to be attached and
/// for Artifex's first message. Writes the handle to `*out`.
///
/// # Safety
/// `out` is a writable pointer.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_start(out: *mut *mut AtriumPlugin) -> c_int {
    if out.is_null() {
        return ATRIUM_PLUGIN_ERR_INVALID;
    }
    let (child, fresco) = match limen::client::Child::embedded() {
        Ok(c) => c,
        Err(_) => return ATRIUM_PLUGIN_ERR_CHANNEL,
    };
    match attach(Box::new(child), Some(fresco)) {
        Ok(h) => {
            // SAFETY: `out` is non-null and writable, per the contract.
            unsafe { *out = Box::into_raw(h) };
            0
        }
        Err(c) => c,
    }
}

/// # Safety
/// `p` is NULL or a live handle; it is invalid afterwards.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_free(p: *mut AtriumPlugin) {
    if !p.is_null() {
        // SAFETY: a handle from Box::into_raw in `atrium_plugin_start`,
        // freed once, per the contract.
        drop(unsafe { Box::from_raw(p) });
    }
}

/// The workspace's name (for display).
///
/// # Safety
/// `p` is NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_workspace_name(p: *mut AtriumPlugin) -> *const c_char {
    // SAFETY: per the contract.
    unsafe { handle(p) }.map_or(std::ptr::null(), |h| h.workspace.as_ptr())
}

/// Where the workspace is, read-only, when the person allowed this
/// plugin to read it: "/work". NULL otherwise.
///
/// # Safety
/// `p` is NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_workspace_dir(p: *mut AtriumPlugin) -> *const c_char {
    static DIR: &CStr = c"/work";
    debug_assert_eq!(DIR.to_str(), Ok(WORKSPACE));
    // SAFETY: per the contract.
    match unsafe { handle(p) } {
        Some(h) if h.p.workspace().is_some() => DIR.as_ptr(),
        _ => std::ptr::null(),
    }
}

/// The `i`th of its languages Artifex has files of open; NULL past the end.
///
/// # Safety
/// `p` is NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_language(p: *mut AtriumPlugin, i: usize) -> *const c_char {
    // SAFETY: per the contract.
    unsafe { handle(p) }.and_then(|h| h.languages.get(i)).map_or(std::ptr::null(), |s| s.as_ptr())
}

/// The next thing Artifex sent, waiting at most `wait_ms`. 1 with `*out`
/// filled, 0 when nothing came, negative on an error.
///
/// # Safety
/// `p` is NULL or a live handle; `out` is NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_next(p: *mut AtriumPlugin, wait_ms: u32, out: *mut AtriumPluginEvent) -> c_int {
    // SAFETY: per the contract.
    let Some(h) = (unsafe { handle(p) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    if out.is_null() {
        return ATRIUM_PLUGIN_ERR_INVALID;
    }
    h.held.clear();
    let ev = match h.p.next(Duration::from_millis(wait_ms.min(MAX_WAIT_MS) as u64)) {
        Ok(Some(e)) => e,
        Ok(None) => return 0,
        Err(e) => return code(&e),
    };
    let mut o = AtriumPluginEvent { kind: 0, line: 0, shown: 0, _pad: 0, json: std::ptr::null(), json_len: 0, command: std::ptr::null(), file: std::ptr::null() };
    match ev {
        Event::Lsp(v) => {
            // serde_json escapes U+0000, so the text has no NUL.
            let s = match cstring(v.to_string()) {
                Ok(s) => s,
                Err(c) => return c,
            };
            o.kind = ATRIUM_PLUGIN_EV_LSP;
            o.json_len = s.as_bytes().len();
            o.json = s.as_ptr();
            h.held.push(s);
        }
        Event::Command { id, file, line } => {
            let id = match cstring(id) {
                Ok(s) => s,
                Err(c) => return c,
            };
            o.kind = ATRIUM_PLUGIN_EV_COMMAND;
            o.line = line;
            o.command = id.as_ptr();
            h.held.push(id);
            if let Some(f) = file {
                let f = match cstring(f) {
                    Ok(s) => s,
                    Err(c) => return c,
                };
                o.file = f.as_ptr();
                h.held.push(f);
            }
        }
        Event::PanelShown(b) => {
            o.kind = ATRIUM_PLUGIN_EV_PANEL_SHOWN;
            o.shown = b as i32;
        }
        Event::Detaching => o.kind = ATRIUM_PLUGIN_EV_DETACHING,
    }
    // SAFETY: `out` is non-null and writable, per the contract.
    unsafe { out.write(o) };
    1
}

/// One whole LSP message (a JSON object) to Artifex.
///
/// # Safety
/// `p` is NULL or a live handle; `msg` NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_send(p: *mut AtriumPlugin, msg: *const c_char) -> c_int {
    // SAFETY: per the contract.
    let (Some(h), Some(v)) = (unsafe { handle(p) }, unsafe { json(msg) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    if !v.is_object() {
        return ATRIUM_PLUGIN_ERR_INVALID;
    }
    res(h.p.send(&v))
}

/// The answer to the request whose `id` is `id_json` (JSON: a number or
/// a string); `result_json` is the result's JSON (NULL: null).
///
/// # Safety
/// `p` is NULL or a live handle; the strings NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_reply(p: *mut AtriumPlugin, id_json: *const c_char, result_json: *const c_char) -> c_int {
    // SAFETY: per the contract.
    let (Some(h), Some(id)) = (unsafe { handle(p) }, unsafe { json(id_json) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    if !(id.is_number() || id.is_string()) {
        return ATRIUM_PLUGIN_ERR_INVALID;
    }
    let result = if result_json.is_null() {
        Value::Null
    } else {
        // SAFETY: per the contract.
        match unsafe { json(result_json) } {
            Some(v) => v,
            None => return ATRIUM_PLUGIN_ERR_INVALID,
        }
    };
    res(h.p.reply(&id, result))
}

/// A notification: `method`, and `params_json` (NULL: none).
///
/// # Safety
/// `p` is NULL or a live handle; the strings NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_notify(p: *mut AtriumPlugin, method: *const c_char, params_json: *const c_char) -> c_int {
    // SAFETY: per the contract.
    let (Some(h), Some(m)) = (unsafe { handle(p) }, unsafe { text(method) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    let params = if params_json.is_null() {
        Value::Null
    } else {
        // SAFETY: per the contract.
        match unsafe { json(params_json) } {
            Some(v) => v,
            None => return ATRIUM_PLUGIN_ERR_INVALID,
        }
    };
    res(h.p.notify(m, params))
}

/// A command for Artifex's palette (`id` must be in the manifest's
/// `commands`).
///
/// # Safety
/// `p` is NULL or a live handle; the strings NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_register_command(p: *mut AtriumPlugin, id: *const c_char, label: *const c_char) -> c_int {
    // SAFETY: per the contract.
    let (Some(h), Some(id), Some(label)) = (unsafe { handle(p) }, unsafe { text(id) }, unsafe { text(label) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    res(h.p.register_command(id, label))
}

/// A line in Artifex's status bar (cut to 120 bytes; "" clears it).
///
/// # Safety
/// `p` is NULL or a live handle; `line` NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_status(p: *mut AtriumPlugin, line: *const c_char) -> c_int {
    // SAFETY: per the contract.
    let (Some(h), Some(t)) = (unsafe { handle(p) }, unsafe { text(line) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    res(h.p.status(t))
}

/// Open the panel (manifest `panel`): a window of `width` x `height` on
/// the frescod connection limend handed over, named to Artifex as the
/// panel `title`. Returns the window's id for libatrium's
/// `atrium_window_frame_*` calls. One panel per plugin; again returns it.
///
/// # Safety
/// `p` is NULL or a live handle; `title` NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_open_panel(p: *mut AtriumPlugin, title: *const c_char, width: u32, height: u32) -> c_int {
    // SAFETY: per the contract.
    let (Some(h), Some(title)) = (unsafe { handle(p) }, unsafe { text(title) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    if let Some(w) = h.panel {
        return match h.p.open_panel(title) {
            Ok(()) => w as c_int,
            Err(e) => code(&e),
        };
    }
    if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
        return ATRIUM_PLUGIN_ERR_INVALID;
    }
    let Some(s) = h.fresco.take() else { return ATRIUM_PLUGIN_ERR_NO_PANEL };
    let Ok(conn) = fresco_client::Connection::wrap(s) else { return ATRIUM_PLUGIN_ERR_NO_PANEL };
    let hints = fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() };
    let token = crate::with_adopted_fresco(conn, |conn| {
        let id = conn.window_create(width, height, title.to_string(), hints).ok()?;
        let until = Instant::now() + TOKEN_WAIT;
        while Instant::now() < until {
            match conn.poll_event() {
                Ok(Some(fresco_client::Event::EmbedToken { window_id, token })) if window_id == id => return Some((id, token)),
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return None,
            }
        }
        None
    });
    let Some((id, token)) = token else { return ATRIUM_PLUGIN_ERR_NO_PANEL };
    let Ok(id_c) = c_int::try_from(id) else { return ATRIUM_PLUGIN_ERR_NO_PANEL };
    if let Err(e) = h.p.ready(token).and_then(|()| h.p.open_panel(title)) {
        return code(&e);
    }
    h.panel = Some(id);
    id_c
}

/// Take the panel's tab away (the window stays; open it again later).
///
/// # Safety
/// `p` is NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_close_panel(p: *mut AtriumPlugin) -> c_int {
    // SAFETY: per the contract.
    let Some(h) = (unsafe { handle(p) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    res(h.p.close_panel())
}

/// The path in the workspace an `artifex:///` URI names, into `buf`
/// (NUL-terminated). Returns its length; ATRIUM_PLUGIN_ERR_INVALID for
/// another URI or one that climbs out (`..`).
///
/// # Safety
/// `uri` NULL or NUL-terminated; `buf` NULL or `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_path_of(uri: *const c_char, buf: *mut c_char, len: usize) -> c_int {
    // SAFETY: per the contract.
    let Some(path) = (unsafe { text(uri) }).and_then(path_of) else { return ATRIUM_PLUGIN_ERR_INVALID };
    if path.contains('\0') {
        return ATRIUM_PLUGIN_ERR_INVALID;
    }
    let Ok(n) = c_int::try_from(path.len()) else { return ATRIUM_PLUGIN_ERR_BUF_TOO_SMALL };
    if buf.is_null() || len <= path.len() {
        return ATRIUM_PLUGIN_ERR_BUF_TOO_SMALL;
    }
    // SAFETY: `buf` has `len` > path.len() writable bytes, per the
    // contract and the check above; the regions can't overlap (`path` is
    // our own allocation).
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), buf.cast::<u8>(), path.len());
        *buf.add(path.len()) = 0;
    }
    n
}

/// Look up `path` in the JSON text `json` and write what is there into
/// `buf` (NUL-terminated, cut to fit), as snprintf does: returns the
/// whole value's length, so a return >= `len` means it was cut.
/// `path` is dot-separated members and array indexes
/// ("params.contentChanges.0.text"; "" is the whole value). With `raw`
/// 0 a string is written decoded, anything else as JSON; with `raw` 1
/// everything is written as JSON (a string keeps its quotes and escapes,
/// ready to put back into a message). ATRIUM_PLUGIN_ERR_INVALID when
/// `json` isn't JSON or nothing is at `path`; a decoded string holding a
/// NUL is refused too, so it can't be cut short unseen.
///
/// # Safety
/// `json` and `path` NULL or NUL-terminated; `buf` NULL or `len` writable
/// bytes.
#[no_mangle]
pub unsafe extern "C" fn atrium_plugin_json_get(json: *const c_char, path: *const c_char, raw: c_int, buf: *mut c_char, len: usize) -> c_int {
    // SAFETY: per the contract.
    let (Some(text), Some(path)) = (unsafe { text(json) }, unsafe { text(path) }) else { return ATRIUM_PLUGIN_ERR_INVALID };
    let Ok(v) = serde_json::from_str::<Value>(text) else { return ATRIUM_PLUGIN_ERR_INVALID };
    let mut at = &v;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        let next = match at {
            Value::Object(m) => m.get(seg),
            Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        };
        let Some(n) = next else { return ATRIUM_PLUGIN_ERR_INVALID };
        at = n;
    }
    let out = match at {
        Value::String(s) if raw == 0 => {
            if s.contains('\0') {
                return ATRIUM_PLUGIN_ERR_INVALID;
            }
            s.clone()
        }
        other => other.to_string(),
    };
    let Ok(n) = c_int::try_from(out.len()) else { return ATRIUM_PLUGIN_ERR_BUF_TOO_SMALL };
    if !buf.is_null() && len > 0 {
        // Cut at a character boundary that fits with the NUL.
        let mut k = out.len().min(len - 1);
        while !out.is_char_boundary(k) {
            k -= 1;
        }
        // SAFETY: `buf` has `len` writable bytes and k < len; `out` is our
        // own allocation, so the regions can't overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(out.as_ptr(), buf.cast::<u8>(), k);
            *buf.add(k) = 0;
        }
    }
    n
}
