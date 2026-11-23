//! Synthetic signed-parent driver for the disabled legacy approval sheet.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};
use limen::client::Parent;
use limen::proto::ParentEvent;

fn wait<T>(duration: Duration, mut next: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + duration;
    loop {
        if let Some(value) = next() {
            return value;
        }
        assert!(Instant::now() < until, "missing fixture event");
    }
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [fresco, limen, inject, expected] = args.as_slice() else {
        panic!("expected fresco, limen, input and expected result");
    };
    assert!(matches!(
        expected.as_str(),
        "opened" | "closed" | "cancelled"
    ));
    let mut win = Connection::connect(fresco).unwrap();
    let id = win
        .window_create(
            900,
            640,
            "Synthetic Mail reader",
            WindowHints {
                initial_position: Some((80, 30)),
                ..WindowHints::default()
            },
        )
        .unwrap();
    win.set_default_window(id as u16);
    let mut frame = win.frame().unwrap();
    frame
        .rect(RectParams {
            x: 0.,
            y: 0.,
            w: 900.,
            h: 640.,
            r: 0.9,
            g: 0.9,
            b: 0.9,
            a: 1.,
            radius: 0.,
        })
        .unwrap();
    frame.finish().unwrap();
    let token = wait(Duration::from_secs(8), || {
        match win.wait_event(Some(Duration::from_millis(100))).unwrap() {
            Some(Event::EmbedToken { token, .. }) => Some(token),
            _ => None,
        }
    });
    let mut app = Parent::at(limen).unwrap();
    let sheet = app.request("smime-legacy-read", token).unwrap();
    assert!(app.send(sheet, b"approve".to_vec()).is_err());
    app.place(sheet, Some([0., 0., 900., 640.])).unwrap();
    wait(Duration::from_secs(10), || {
        (app.poll(Duration::from_millis(100)).unwrap()
            == Some(ParentEvent::Attached { embed: sheet }))
        .then_some(())
    });
    assert!(
        app.poll(Duration::from_secs(2)).unwrap().is_none(),
        "legacy result before approval"
    );
    let press = if expected == "cancelled" { 41 } else { 40 };
    let mut input = UnixStream::connect(inject).unwrap();
    writeln!(input, "KEY {press} 1 0\nKEY {press} 0 0").unwrap();
    wait(Duration::from_secs(17), || {
        match app.poll(Duration::from_millis(100)).unwrap() {
            Some(ParentEvent::Message { embed, bytes }) => {
                assert_eq!(embed, sheet);
                assert_eq!(bytes, expected.as_bytes());
                Some(())
            }
            Some(ParentEvent::MessageFd { .. }) => panic!("Mail received a legacy descriptor"),
            _ => None,
        }
    });
    assert!(app.take_fd().is_none());
    app.detach(sheet).unwrap();
    println!("LEGACY_PARENT_{}", expected.to_ascii_uppercase());
}
