//! Pergola's clipboard client against the real `tabulad` server code
//! (with a stand-in frescod): what TextField's Ctrl-C/V do on the wire.

use fresco_protocol::activation::kind;
use pergola::clipboard::{Backend, Error, Tabula};

fn server() -> (tempfile::TempDir, std::path::PathBuf, tabula::gate::Fake) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tabulad.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let fake = tabula::gate::Fake::default();
    let shared = tabula::server::Shared::new(Box::new(fake.clone()));
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let s = s.unwrap();
            let shared = shared.clone();
            // The test process is both apps; Tabula sees its real uid.
            let uid = unsafe { libc::getuid() };
            std::thread::spawn(move || {
                let _ = tabula::server::serve(aqueduct::Connection::wrap(s).unwrap(), uid, shared);
            });
        }
    });
    (dir, path, fake)
}

#[test]
fn copy_and_paste_through_tabula() {
    let (_dir, path, fake) = server();
    let me = unsafe { libc::getuid() };
    let mut a = Tabula::at(&path).unwrap();
    let mut b = Tabula::at(&path).unwrap();
    let rich = "application/vnd.atrium.sheets.cells";
    let html = b"<table><tr><td>from a</td></tr></table>";
    a.set(fake.mint(me, kind::KEY), &[("text/plain", b"from a"), ("text/html", html), (rich, b"rich bytes")], false).unwrap();
    let paste = fake.mint(me, kind::KEY);
    let got = b.get(paste, &[rich, "text/html", "text/plain"]).unwrap();
    assert_eq!(got, Some((rich.into(), b"rich bytes".to_vec())));
    assert!(matches!(b.get(paste, &["text/plain"]), Err(Error::Refused { .. })));
    let got = b.get(fake.mint(me, kind::KEY), &["text/html", "text/plain"]).unwrap();
    assert_eq!(got, Some(("text/html".into(), html.to_vec())));
    let got = b.get(fake.mint(me, kind::KEY), &["text/plain"]).unwrap();
    assert_eq!(got, Some(("text/plain".into(), b"from a".to_vec())));
    // A token that frescod never minted.
    assert!(matches!(b.get([1; 16], &["text/plain"]), Err(Error::Refused { .. })));
}

#[test]
fn a_drop_is_taken_with_frescods_drop_token() {
    use aqueduct::flag;
    use tabula::proto::{op, DragBegin, Format, Reply, CLASS};
    let (_dir, path, fake) = server();
    let me = unsafe { libc::getuid() };
    // The source starts the drag (Pergola starts none yet: raw wire).
    let mut src = aqueduct::Connection::connect(&path).unwrap();
    let hash = src.upload_blob(b"dragged").unwrap();
    let req = DragBegin { token: fake.mint(me, kind::BUTTON), formats: vec![Format { mime: "text/plain".into(), hash }] };
    src.send_message(CLASS, op::DRAG_BEGIN, flag::RESPONSE_EXPECTED, &postcard::to_stdvec(&req).unwrap()).unwrap();
    let drag_id = loop {
        let m = src.recv_message_or_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        if m.flags & flag::IS_RESPONSE != 0 {
            match postcard::from_bytes::<Reply>(&m.payload).unwrap() {
                Reply::Dragging { drag_id } => break drag_id,
                other => panic!("{other:?}"),
            }
        }
    };
    let mut target = Tabula::at(&path).unwrap();
    // A press is not a drop.
    assert!(matches!(target.drop_get(fake.mint(me, kind::BUTTON), &["text/plain"]), Err(Error::Refused { .. })));
    let drop = fake.mint_drop(me, drag_id);
    assert_eq!(target.drop_get(drop, &["text/plain"]).unwrap(), Some(("text/plain".into(), b"dragged".to_vec())));
    // Once.
    assert!(target.drop_get(drop, &["text/plain"]).is_err());
}

#[test]
fn rich_text_field_round_trips_emphasis_through_tabula() {
    use pergola::clipboard::set_backend;
    use pergola::event::{Event, Key, KeyEventKind, Modifiers};
    use pergola::text_edit::TextEdit;
    use pergola::widgets::TextField;
    use pergola::{App, Mutable};

    let (_dir, path, fake) = server();
    let me = unsafe { libc::getuid() };
    let mut edit = TextEdit::rich("é\ntext", [(0..2, 3), (3..7, 12)]);
    edit.select_all();
    let expected: Vec<_> = edit.inline_runs().collect();
    let source_text = Mutable::new(edit);
    let mut source = App::new(TextField::new(source_text.clone()).autofocus(true));
    let mut before = TextEdit::rich("original", [(0..8, 1)]);
    before.select_all();
    let target = Mutable::new(before);
    let mut destination = App::new(TextField::new(target.clone()).autofocus(true));
    let chord = |key: &str| Event::Key {
        kind: KeyEventKind::Down, key: Key::Char, chars: key.into(),
        modifiers: Modifiers { ctrl: true, ..Default::default() },
    };
    source.tick();
    set_backend(Box::new(Tabula::at(&path).unwrap()));
    source.handle_event_activated(chord("c"), Some(fake.mint(me, kind::KEY)));
    destination.tick();
    set_backend(Box::new(Tabula::at(&path).unwrap()));
    destination.handle_event_activated(chord("v"), Some(fake.mint(me, kind::KEY)));
    assert_eq!(target.get_cloned().text(), "é\ntext");
    assert_eq!(target.get_cloned().inline_runs().collect::<Vec<_>>(), expected);
    destination.handle_event(chord("z"));
    assert_eq!(target.get_cloned().text(), "original");
    assert_eq!(target.get_cloned().inline_runs().collect::<Vec<_>>(), vec![(0..8, 1)]);

    let mut unstyled = TextEdit::rich("plain", []);
    unstyled.select_all();
    source_text.set(unstyled);
    set_backend(Box::new(Tabula::at(&path).unwrap()));
    source.handle_event_activated(chord("c"), Some(fake.mint(me, kind::KEY)));
    set_backend(Box::new(Tabula::at(&path).unwrap()));
    destination.handle_event_activated(chord("v"), Some(fake.mint(me, kind::KEY)));
    assert_eq!(target.get_cloned().text(), "plain");
    assert_eq!(target.get_cloned().inline_runs().collect::<Vec<_>>(), vec![(0..5, 0)]);
}
