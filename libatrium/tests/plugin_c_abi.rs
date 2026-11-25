//! Artifex plugins in C (dev-tools.md §3.8): a session through the C
//! calls of atrium_plugin.h, on the SDK's in-memory channel, and a panel
//! opened against a stub frescod that hands out an embed token.

#![cfg(unix)]

use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::os::raw::c_char;
use std::os::unix::net::UnixStream;
use std::thread;

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, flag, Header};
use atrium::plugin::*;
use atrium_artifex_ext::testing::Memory;
use atrium_artifex_ext::{json, Init, Value};
use limen::editor_ext::{Panel, ToChild, ToParent};

fn init(read: bool) -> Init {
    Init { languages: vec!["c".into(), "markdown".into()], workspace: "proj".into(), read }
}

fn s(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string())
}

fn c(t: &str) -> CString {
    CString::new(t).unwrap()
}

fn empty() -> AtriumPluginEvent {
    AtriumPluginEvent { kind: 0, line: 0, shown: 0, _pad: 0, json: std::ptr::null(), json_len: 0, command: std::ptr::null(), file: std::ptr::null() }
}

#[test]
fn a_c_plugin_reads_events_and_answers() {
    let m = Memory::started(init(false));
    m.lsp(json!({ "jsonrpc": "2.0", "id": 4, "method": "initialize", "params": {} }));
    m.push(ToChild::Command { id: "hello.count".into(), file: Some("artifex:///src/a.c".into()), line: 9 });
    m.push(ToChild::PanelShown(true));
    let p = Box::into_raw(attach(Box::new(m.clone()), None).ok().unwrap());
    unsafe {
        assert_eq!(s(atrium_plugin_workspace_name(p)).as_deref(), Some("proj"));
        assert_eq!(s(atrium_plugin_language(p, 0)).as_deref(), Some("c"));
        assert_eq!(s(atrium_plugin_language(p, 1)).as_deref(), Some("markdown"));
        assert!(atrium_plugin_language(p, 2).is_null());
        // Not allowed to read: no workspace dir.
        assert!(atrium_plugin_workspace_dir(p).is_null());

        let mut ev = empty();
        assert_eq!(atrium_plugin_next(p, 0, &mut ev), 1);
        assert_eq!(ev.kind, ATRIUM_PLUGIN_EV_LSP);
        let got: Value = serde_json::from_str(&s(ev.json).unwrap()).unwrap();
        assert_eq!(got["method"], "initialize");
        assert_eq!(ev.json_len, s(ev.json).unwrap().len());

        assert_eq!(atrium_plugin_next(p, 0, &mut ev), 1);
        assert_eq!(ev.kind, ATRIUM_PLUGIN_EV_COMMAND);
        assert_eq!((s(ev.command).as_deref(), s(ev.file).as_deref(), ev.line), (Some("hello.count"), Some("artifex:///src/a.c"), 9));

        assert_eq!(atrium_plugin_next(p, 0, &mut ev), 1);
        assert_eq!((ev.kind, ev.shown), (ATRIUM_PLUGIN_EV_PANEL_SHOWN, 1));
        assert_eq!(atrium_plugin_next(p, 0, &mut ev), 0, "nothing more queued");

        // Answers.
        assert_eq!(atrium_plugin_reply(p, c("4").as_ptr(), c(r#"{"capabilities":{}}"#).as_ptr()), 0);
        assert_eq!(atrium_plugin_notify(p, c("textDocument/publishDiagnostics").as_ptr(), c(r#"{"uri":"artifex:///src/a.c","diagnostics":[]}"#).as_ptr()), 0);
        assert_eq!(atrium_plugin_send(p, c(r#"{"jsonrpc":"2.0","method":"x/y"}"#).as_ptr()), 0);
        assert_eq!(atrium_plugin_register_command(p, c("hello.count").as_ptr(), c("Count the lines").as_ptr()), 0);
        assert_eq!(atrium_plugin_status(p, c("12 lines").as_ptr()), 0);
        assert_eq!(atrium_plugin_close_panel(p), 0);

        // Refused before anything goes out.
        assert_eq!(atrium_plugin_send(p, c("{not json").as_ptr()), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_send(p, c("[1,2]").as_ptr()), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_reply(p, c("{}").as_ptr(), std::ptr::null()), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_notify(p, std::ptr::null(), std::ptr::null()), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_next(p, 0, std::ptr::null_mut()), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_next(std::ptr::null_mut(), 0, &mut ev), ATRIUM_PLUGIN_ERR_INVALID);
        // No frescod connection here: no panel.
        assert_eq!(atrium_plugin_open_panel(p, c("Hello").as_ptr(), 400, 200), ATRIUM_PLUGIN_ERR_NO_PANEL);
        atrium_plugin_free(p);
    }
    let lsp = m.sent_lsp();
    assert_eq!(lsp.len(), 3);
    assert_eq!((lsp[0]["id"].clone(), lsp[0]["result"]["capabilities"].clone()), (json!(4), json!({})));
    assert_eq!(lsp[1]["method"], "textDocument/publishDiagnostics");
    assert_eq!(lsp[2]["method"], "x/y");
    let sent = m.from_plugin.borrow();
    assert!(sent.iter().any(|t| matches!(t, ToParent::RegisterCommand { id, label } if id == "hello.count" && label == "Count the lines")));
    assert!(sent.iter().any(|t| matches!(t, ToParent::Status(l) if l == "12 lines")));
    assert!(sent.iter().any(|t| matches!(t, ToParent::Panel(Panel::Close))));
}

#[test]
fn detaching_and_paths() {
    let m = Memory::started(init(false));
    m.to_plugin.borrow_mut().push_back(limen::proto::ChildEvent::Detaching { embed: atrium_artifex_ext::testing::EMBED });
    let p = Box::into_raw(attach(Box::new(m), None).ok().unwrap());
    unsafe {
        let mut ev = empty();
        assert_eq!(atrium_plugin_next(p, 0, &mut ev), 1);
        assert_eq!(ev.kind, ATRIUM_PLUGIN_EV_DETACHING);
        atrium_plugin_free(p);

        let mut buf = [0 as c_char; 32];
        let n = atrium_plugin_path_of(c("artifex:///src/my%20file.c").as_ptr(), buf.as_mut_ptr(), buf.len());
        assert_eq!(n, 13);
        assert_eq!(s(buf.as_ptr()).as_deref(), Some("src/my file.c"));
        assert_eq!(atrium_plugin_path_of(c("artifex:///../etc/passwd").as_ptr(), buf.as_mut_ptr(), buf.len()), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_path_of(c("file:///etc/passwd").as_ptr(), buf.as_mut_ptr(), buf.len()), ATRIUM_PLUGIN_ERR_INVALID);
        // "src/my file.c" is 13 bytes: 13 leaves no room for the NUL.
        assert_eq!(atrium_plugin_path_of(c("artifex:///src/my%20file.c").as_ptr(), buf.as_mut_ptr(), 13), ATRIUM_PLUGIN_ERR_BUF_TOO_SMALL);
    }
}

/// A frescod that answers one WINDOW_CREATE with `id` and then sends the
/// window's embed token; returns the title and size it was asked for.
fn stub_frescod(mut s: UnixStream, id: u32, token: [u8; 16]) -> thread::JoinHandle<(String, u32, u32, bool)> {
    thread::spawn(move || {
        let mut h = [0u8; envelope::HEADER_LEN];
        s.read_exact(&mut h).unwrap();
        let hdr = Header::decode(&h).unwrap();
        let mut p = vec![0u8; hdr.length as usize];
        s.read_exact(&mut p).unwrap();
        assert_eq!((hdr.opcode_class, hdr.op), (CLASS_DISPLAY, fresco_protocol::control::OP_WINDOW_CREATE));
        let w: fresco_protocol::WindowCreatePayload = postcard::from_bytes(&p).unwrap();
        let mut send = |op, flags, payload: Vec<u8>| {
            let mut out = Header::new(CLASS_DISPLAY, op, flags, payload.len() as u32).encode().to_vec();
            out.extend_from_slice(&payload);
            s.write_all(&out).unwrap();
        };
        send(fresco_protocol::control::OP_WINDOW_CREATE, flag::IS_RESPONSE, postcard::to_stdvec(&fresco_protocol::WindowCreateReply { window_id: id, scale_milli: 1000 }).unwrap());
        send(fresco_protocol::control::EV_WINDOW_EMBED_TOKEN, flag::ASYNC_EVENT, postcard::to_stdvec(&fresco_protocol::EmbedTokenEvent { window_id: id, token }).unwrap());
        (w.title, w.width, w.height, w.hints.embed_pending)
    })
}

#[test]
fn a_panel_is_a_window_on_limends_frescod_connection() {
    let m = Memory::started(init(false));
    let (ours, theirs) = UnixStream::pair().unwrap();
    let frescod = stub_frescod(theirs, 3, [7; 16]);
    let p = Box::into_raw(attach(Box::new(m.clone()), Some(ours)).ok().unwrap());
    unsafe {
        assert_eq!(atrium_plugin_open_panel(p, c("Hello").as_ptr(), 0, 200), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_open_panel(p, c("Hello").as_ptr(), 400, 200), 3);
        // Again: the same window, shown again.
        assert_eq!(atrium_plugin_open_panel(p, c("Hello").as_ptr(), 400, 200), 3);
        atrium_plugin_free(p);
    }
    assert_eq!(frescod.join().unwrap(), ("Hello".into(), 400, 200, true));
    assert_eq!(*m.ready.borrow(), Some([7; 16]), "limend was told the window's token");
    let opens = m.from_plugin.borrow().iter().filter(|t| matches!(t, ToParent::Panel(Panel::Open { title }) if title == "Hello")).count();
    assert_eq!(opens, 2);
    atrium::atrium_window_disconnect();
}

#[test]
fn json_fields_for_c() {
    let msg = c(r#"{"jsonrpc":"2.0","id":"a\"b","method":"textDocument/didChange","params":{"textDocument":{"uri":"artifex:///x.c"},"contentChanges":[{"text":"one\ntwo\n"}]}}"#);
    let mut buf = [0 as c_char; 64];
    unsafe {
        let get = |path: &str, raw: i32, buf: &mut [c_char]| atrium_plugin_json_get(msg.as_ptr(), c(path).as_ptr(), raw, buf.as_mut_ptr(), buf.len());
        assert_eq!(get("method", 0, &mut buf), 22);
        assert_eq!(s(buf.as_ptr()).as_deref(), Some("textDocument/didChange"));
        assert_eq!(get("params.contentChanges.0.text", 0, &mut buf), 8);
        assert_eq!(s(buf.as_ptr()).as_deref(), Some("one\ntwo\n"));
        // raw: a string keeps its quotes and escapes, ready to send back.
        assert_eq!(get("id", 1, &mut buf), 6);
        assert_eq!(s(buf.as_ptr()).as_deref(), Some(r#""a\"b""#));
        assert_eq!(get("params.textDocument", 0, &mut buf), 24);
        assert_eq!(s(buf.as_ptr()).as_deref(), Some(r#"{"uri":"artifex:///x.c"}"#));
        // Missing, wrong kind of step, not JSON.
        assert_eq!(get("params.nothing", 0, &mut buf), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(get("params.contentChanges.x", 0, &mut buf), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(get("params.contentChanges.1", 0, &mut buf), ATRIUM_PLUGIN_ERR_INVALID);
        assert_eq!(atrium_plugin_json_get(c("{oops").as_ptr(), c("").as_ptr(), 0, buf.as_mut_ptr(), buf.len()), ATRIUM_PLUGIN_ERR_INVALID);
        // Cut to fit, as snprintf: the whole length comes back.
        let mut small = [0 as c_char; 5];
        assert_eq!(get("method", 0, &mut small), 22);
        assert_eq!(s(small.as_ptr()).as_deref(), Some("text"));
        // A decoded NUL would cut the string short unseen: refused.
        assert_eq!(atrium_plugin_json_get(c(r#"{"t":"a\u0000b"}"#).as_ptr(), c("t").as_ptr(), 0, buf.as_mut_ptr(), buf.len()), ATRIUM_PLUGIN_ERR_INVALID);
    }
}
