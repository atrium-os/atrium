//! End-to-end: a C app's frames, inspected (dev-tools.md §3.6). The IDE's
//! socket is handed in at ATRIUM_INSPECT_FD as portcullisd does; libatrium
//! says hello, lists what the last frame drew with the line that drew it,
//! tells when it changes, and outlines every box on the next frame.

#![cfg(unix)]

use std::ffi::CString;
use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aqueduct::envelope::{self, Header};
use atrium_inspect::{encode, Ask, Decoder, Tell};

const OP_SCENE_NODE_SET: u16 = 0x0040;

fn spawn_stub(path: std::path::PathBuf) -> Arc<Mutex<Vec<u32>>> {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let ids2 = ids.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&path).expect("bind");
        let (mut s, _) = listener.accept().expect("accept");
        loop {
            let mut h = [0u8; envelope::HEADER_LEN];
            if s.read_exact(&mut h).is_err() { break; }
            let hdr = Header::decode(&h).expect("hdr");
            let mut p = vec![0u8; hdr.length as usize];
            if s.read_exact(&mut p).is_err() { break; }
            if hdr.op == OP_SCENE_NODE_SET {
                let n: fresco_protocol::SceneNodeSetPayload = postcard::from_bytes(&p).expect("node");
                ids2.lock().unwrap().push(n.node_id);
            }
        }
    });
    ids
}

fn frame(file: &CString, n: u32) {
    assert_eq!(atrium::atrium_window_frame_begin(3), 0);
    // SAFETY: a NUL-terminated string that outlives the call.
    unsafe { atrium::inspect::atrium__site(file.as_ptr(), 40) };
    assert_eq!(atrium::atrium_window_frame_rect(1, 0.0, 0.0, 100.0, 50.0, 1.0, 1.0, 1.0, 1.0), 0);
    unsafe { atrium::inspect::atrium__site(file.as_ptr(), 41) };
    assert_eq!(atrium::atrium_window_frame_rect(2, 10.0 * n as f32, 10.0, 20.0, 20.0, 0.2, 0.45, 0.85, 1.0), 0);
    assert_eq!(atrium::atrium_window_frame_end(), 0);
}

fn hear(ide: &mut UnixStream, d: &mut Decoder) -> Tell {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(t) = d.next::<Tell>().unwrap() {
            return t;
        }
        assert!(Instant::now() < deadline, "the app said nothing");
        let mut buf = [0u8; 4096];
        match ide.read(&mut buf) {
            Ok(n) => d.push(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn an_ide_reads_a_c_apps_frames_and_outlines_them() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let ids = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);
    let (app, mut ide) = UnixStream::pair().unwrap();
    std::env::set_var("ATRIUM_INSPECT_FD", app.into_raw_fd().to_string());
    ide.set_nonblocking(true).unwrap();
    let mut d = Decoder::default();
    let file = CString::new("app.c").unwrap();

    frame(&file, 1);
    assert!(matches!(hear(&mut ide, &mut d), Tell::Hello { .. }));
    ide.write_all(&encode(&Ask::Tree).unwrap()).unwrap();
    frame(&file, 1);
    let Tell::Tree(items) = hear(&mut ide, &mut d) else { panic!("not a tree") };
    assert_eq!(items.len(), 2);
    assert_eq!((items[1].id, items[1].kind.as_str(), items[1].rect), (2, "Rect", Some([10.0, 10.0, 20.0, 20.0])));
    assert_eq!(items[1].site.as_ref().map(|s| (s.file.as_str(), s.line)), Some(("app.c", 41)));
    assert_eq!(items[1].detail, "fill #3373d9ff");

    frame(&file, 2);
    assert_eq!(hear(&mut ide, &mut d), Tell::Changed, "a frame that drew something else");

    ide.write_all(&encode(&Ask::Bounds(true)).unwrap()).unwrap();
    frame(&file, 2); // hears the ask at its end
    let before = ids.lock().unwrap().len();
    frame(&file, 2);
    thread::sleep(Duration::from_millis(100));
    let drawn: Vec<u32> = ids.lock().unwrap()[before..].to_vec();
    let marks = drawn.iter().filter(|&&i| i >= atrium::inspect::MARK_BASE).count();
    assert_eq!(marks, 8, "two boxes, four sides each: {drawn:?}");
    ide.write_all(&encode(&Ask::Tree).unwrap()).unwrap();
    frame(&file, 2);
    let Tell::Tree(items) = hear(&mut ide, &mut d) else { panic!("not a tree") };
    assert_eq!(items.len(), 2, "outlines are not listed");
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
}
