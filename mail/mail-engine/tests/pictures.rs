//! Remote pictures (mail.md §7.4): nothing is fetched until asked; then
//! only the loadable pictures, through Nuntius, kept with the message so a
//! second view fetches nothing. A fake Nuntius answers on a socket pair.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use mail_engine::{pictures, Engine, Parser};
use mail_store::{Role, Store};
use mail_sync::{Fetched, RemoteFolder, Sink};
use nuntius::{Event, Request};

fn jail() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/Cargo.toml");
    let st = Command::new(env!("CARGO")).args(["build", "-q", "--bin", "mail-parse-jail", "--manifest-path"]).arg(&manifest).status().unwrap();
    assert!(st.success());
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mail-parse/target/debug/mail-parse-jail")
}

/// A Nuntius that answers every GET with `body` (404 for `/missing`), and
/// records the URLs asked for.
fn fake_nuntius(body: Vec<u8>) -> (nuntius::Client, Arc<Mutex<Vec<String>>>) {
    let (a, b) = UnixStream::pair().unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    std::thread::spawn(move || {
        let (mut rx, mut tx) = nuntius::server::channel(b).unwrap();
        while let Ok(Some(r)) = rx.recv() {
            let Request::Open(o) = r else { continue };
            seen.lock().unwrap().push(o.url.clone());
            let id = o.id;
            let status = if o.url.ends_with("/missing") { 404 } else { 200 };
            let _ = tx.send(Event::Head { id, status, final_url: o.url, redirects: 0, content_type: Some("image/png".into()), length: None, accept_ranges: false, validator: None });
            if status == 200 {
                for (k, c) in body.chunks(7).enumerate() {
                    let _ = tx.send(Event::Data { id, offset: (k * 7) as u64, bytes: c.to_vec() });
                }
                let _ = tx.send(Event::Done { id, bytes: body.len() as u64 });
            } else {
                let _ = tx.send(Event::Error { id, code: nuntius::ErrorCode::Http(404) });
            }
        }
    });
    (nuntius::Client::from_stream(a).unwrap(), asked)
}

/// The first bytes of a PNG of `w` × `h` (signature and header chunk).
fn png_head(w: u32, h: u32) -> Vec<u8> {
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend(13u32.to_be_bytes());
    v.extend(&ihdr);
    v.extend(crc32(&ihdr).to_be_bytes());
    v
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

const MSG: &str = "From: Priya <priya@example.com>\r\nTo: arun@example.org\r\nSubject: Q4 deck\r\nMessage-ID: <q4@example.com>\r\nDate: Mon, 23 Nov 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>The chart:</p><img src=\"https://pics.example/chart.png\" width=\"200\" height=\"100\"><img src=\"https://pics.example/missing\" width=\"20\" height=\"20\"><img src=\"https://track.example/open.gif\" width=\"1\" height=\"1\"><img src=\"https://track.example/hid.png\" style=\"display:none\"><div style=\"background:url(https://pics.example/bg.png)\">Thanks</div>\r\n";

#[test]
fn pictures_load_only_when_asked_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(Store::open(dir.path()).unwrap(), Parser { program: jail() });
    let account = engine.store.add_account("arun@example.org").unwrap();
    {
        let mut sink = engine.sink(account);
        sink.folders(&[RemoteFolder { name: "INBOX".into(), role: mail_sync::Role::Inbox, selectable: true }]).unwrap();
        let f = Fetched { uid: 1, flags: vec![], size: MSG.len() as u64, internal_date: None, server_id: None, bytes: MSG.as_bytes().to_vec(), full: true };
        sink.messages("INBOX", vec![f]).unwrap();
    }
    let inbox = engine.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let msg = engine.store.list(inbox, 0, 10).unwrap()[0].msg;

    // Opening fetches nothing; the pixel and the hidden picture are never on the list.
    let r = engine.open(msg).unwrap();
    assert!(r.document.is_some());
    let want = engine.pictures_to_load(msg).unwrap();
    let mut urls: Vec<&str> = want.iter().map(|w| w.1.as_str()).collect();
    urls.sort();
    assert_eq!(urls, ["https://pics.example/bg.png", "https://pics.example/chart.png", "https://pics.example/missing"]);
    let bg = want.iter().find(|w| w.1.ends_with("bg.png")).unwrap().0.clone();
    // Not drawn yet: its size is unknown until it is loaded.
    assert!(!r.document.as_ref().unwrap().html.contains(&format!("url({bg})")));

    // "Load pictures".
    let png = png_head(40, 30);
    let (mut client, asked) = fake_nuntius(png.clone());
    let got = pictures::fetch(&mut client, &want);
    assert_eq!(got.len(), 2, "the 404 stays a placeholder");
    assert_eq!(got[0].1, png);
    assert!(!asked.lock().unwrap().iter().any(|u| u.contains("track.example")));
    // Something the request could not have asked for is not kept.
    let mut extra = got.clone();
    extra.push(("mail-res:999".into(), vec![1, 2, 3]));
    assert_eq!(engine.keep_pictures(msg, extra).unwrap(), 2);
    // Converted again with the background's size, so it is drawn now.
    let html = engine.open(msg).unwrap().document.unwrap().html;
    assert!(html.contains(&format!("url({bg})")) || html.contains(&format!("url(\"{bg}\")")), "{html}");

    // A second view has them and fetches nothing more but the failed one.
    let mut kept = engine.pictures(msg).unwrap();
    let mut got = got;
    kept.sort();
    got.sort();
    assert_eq!(kept, got);
    let again = engine.pictures_to_load(msg).unwrap();
    assert_eq!(again.iter().map(|w| w.1.as_str()).collect::<Vec<_>>(), ["https://pics.example/missing"]);

    // The per-sender choice.
    assert!(!engine.store.always_pictures(account, "priya@example.com").unwrap());
    engine.store.write(|w| w.set_always_pictures(account, "Priya@Example.com", true)).unwrap();
    assert!(engine.store.always_pictures(account, "priya@example.com").unwrap());
}
