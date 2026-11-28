//! Drive the Files keyboard shortcuts and the confirmation button. The installed
//! gate supplies its live broker and two differently grouped fixture folders.
use pergola::{geom::Point, text_edit::TextEdit, App, Event, Key, KeyEventKind, Mutable};
use portcullis_ipc::{
    read_request, write_response, CrossingSwitch, Request, Response, PROTO_VERSION,
};
use scrinium::{
    browser::{Browser, Mode, Mods},
    places,
    view::{on_key, with, ScriniumView},
};
use std::io::BufReader;
use std::os::unix::{fs::MetadataExt, net::UnixListener};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};

fn broker(root: &Path) -> Arc<AtomicU8> {
    let socket = root.join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::env::set_var("SCRINIUM_PORTCULLIS_SOCKET", socket);
    let switch = Arc::new(AtomicU8::new(0));
    let active = switch.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = stream.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            assert!(matches!(
                read_request(&mut r).unwrap(),
                Request::Hello { .. }
            ));
            write_response(
                &mut s,
                &Response::Hello {
                    version: PROTO_VERSION,
                },
            )
            .unwrap();
            assert!(matches!(read_request(&mut r).unwrap(), Request::Crossings));
            let on = match active.load(Ordering::SeqCst) {
                0 => vec![],
                n => vec![CrossingSwitch {
                    from: "personal".into(),
                    to: "work".into(),
                    kind: if n == 1 { "share" } else { "files" }.into(),
                }],
            };
            write_response(
                &mut s,
                &Response::Crossings {
                    personas: vec![("work".into(), "blue".into())],
                    on,
                },
            )
            .unwrap();
        }
    });
    switch
}

#[test]
fn files_paste_checks_direction_and_creates_destination_owned_bytes() {
    let live = std::env::var_os("SCRINIUM_TEST_CROSSING_ROOT");
    let root = live
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let path = std::env::temp_dir().join(format!("scrinium-cross-{}", std::process::id()));
            fs::create_dir(&path).unwrap();
            path
        });
    let personal = root.join("home/Documents");
    let work = root.join("personas/work/Documents");
    let switch = if live.is_none() {
        fs::create_dir_all(&personal).unwrap();
        fs::create_dir_all(&work).unwrap();
        Some(broker(&root))
    } else {
        assert_ne!(
            personal.metadata().unwrap().gid(),
            work.metadata().unwrap().gid()
        );
        None
    };
    let name = "Quarterly report.txt";
    fs::write(personal.join(name), b"personal draft 231").unwrap();
    fs::write(work.join(name), b"work original").unwrap();
    let mut pl = places::home_in(&root.join("home"));
    pl.extend(places::personas_in(&root.join("personas")));
    let mut b = Browser::new(Mode::Manager);
    b.set_places(pl);
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(b, dirty.clone());
    let field = state.sibling(TextEdit::new(""));
    let view = ScriniumView::new(
        state.clone(),
        field.clone(),
        state.sibling(Default::default()),
        state.sibling((960., 600.)),
    );
    let mut app = App::new_with_flag(view, dirty);
    let key = |k, chars: &str, ctrl| {
        assert!(on_key(
            &state,
            &field,
            &Event::Key {
                key: k,
                chars: chars.into(),
                kind: KeyEventKind::Down,
                modifiers: pergola::Modifiers {
                    ctrl,
                    ..Default::default()
                }
            }
        ))
    };
    let paste = |src, dst, cut| {
        with(&state, |b| {
            b.open_place(src);
            let i = b.entries.iter().position(|e| e.name == name).unwrap();
            b.press(i, Mods::default(), 0.);
        });
        key(Key::Char, if cut { "x" } else { "c" }, true);
        with(&state, |b| b.open_place(dst));
        key(Key::Char, "v", true);
    };
    let press = |app: &mut App<ScriniumView>| {
        app.tick();
        let at = Point::new(912., 566.);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
    };
    let copy = work.join("Quarterly report copy.txt");
    if let Some(s) = &switch {
        for setting in [0, 1] {
            s.store(setting, Ordering::SeqCst);
            paste(0, 1, false);
            assert!(state.get_cloned().file_crossing.unwrap().refusal.is_some());
            key(Key::Escape, "", false);
            assert!(!copy.exists());
        }
        s.store(2, Ordering::SeqCst);
        paste(0, 1, false);
        s.store(0, Ordering::SeqCst);
        press(&mut app);
        assert!(state.get_cloned().file_crossing.unwrap().refusal.is_some());
        assert!(!copy.exists());
        key(Key::Escape, "", false);
        s.store(2, Ordering::SeqCst);
    }
    paste(1, 0, true);
    assert!(state.get_cloned().file_crossing.unwrap().refusal.is_some());
    assert!(!personal.join("Quarterly report copy.txt").exists());
    key(Key::Escape, "", false);
    paste(0, 1, false);
    assert!(!copy.exists());
    let ask = state.get_cloned().file_crossing.unwrap();
    assert!(ask.refusal.is_none(), "{:?}", ask.refusal);
    assert_eq!(
        (&ask.from[..], &ask.to[..], ask.names()),
        ("personal", "work", &[name.to_string()][..])
    );
    key(Key::Escape, "", false);
    assert!(!copy.exists());
    paste(0, 1, false);
    press(&mut app);
    assert_eq!(fs::read(&copy).unwrap(), b"personal draft 231");
    assert_eq!(fs::read(work.join(name)).unwrap(), b"work original");
    if switch.is_some() {
        paste(0, 1, false);
        fs::rename(personal.join(name), personal.join("first inode")).unwrap();
        fs::write(personal.join(name), b"replacement").unwrap();
        press(&mut app);
        assert!(!work.join("Quarterly report copy 2.txt").exists());
        fs::rename(personal.join("first inode"), personal.join(name)).unwrap();
    }
    let src = personal.join(name).metadata().unwrap();
    let dest = copy.metadata().unwrap();
    assert_ne!((src.dev(), src.ino()), (dest.dev(), dest.ino()));
    assert_eq!(dest.gid(), work.metadata().unwrap().gid());
    assert_eq!(dest.mode() & 0o7777, 0o660);
    paste(0, 1, true);
    press(&mut app);
    let moved = work.join("Quarterly report copy 2.txt");
    assert_eq!(fs::read(&moved).unwrap(), b"personal draft 231");
    assert_ne!(src.ino(), moved.metadata().unwrap().ino());
    assert_eq!(moved.metadata().unwrap().gid(), dest.gid());
    assert!(!personal.join(name).exists());
    if live.is_none() {
        fs::remove_dir_all(root).unwrap();
    }
}
