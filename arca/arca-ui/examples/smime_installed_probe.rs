//! Display driver and public-certificate assertion for the installed Mail gate.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::thread::sleep;
use std::time::{Duration, Instant};

use arcad::proto;
use fresco_client::Connection;

fn wait<T>(what: &str, mut get: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = get() {
            return value;
        }
        assert!(Instant::now() < until, "{what}");
        sleep(Duration::from_millis(10));
    }
}

fn inject_input(socket: &str, lines: &str) {
    let mut s = UnixStream::connect(socket).expect("input socket");
    s.write_all(lines.as_bytes()).unwrap();
}

fn input(socket: &str, lines: &str) {
    inject_input(socket, lines);
    sleep(Duration::from_millis(250));
}

fn key(socket: &str, usage: u16) {
    input(socket, &format!("KEY {usage} 1 0\nKEY {usage} 0 0\n"));
}

fn enter_for_picker(wm: &mut Connection, inject: &str, sheet: u32) {
    for _ in 0..4 {
        key(inject, 40);
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if wm
                .wm_enumerate_all()
                .unwrap()
                .embeds
                .iter()
                .any(|e| e[1] == sheet)
            {
                return;
            }
            sleep(Duration::from_millis(100));
        }
    }
    panic!("Scrinium did not attach within Arca");
}

fn settings_sheet(wm: &mut Connection, inject: &str, uid: u32, offset: i32) -> (u32, Duration) {
    let settings = wait("Mail Settings surface", || {
        let reply = wm.wm_enumerate_all().unwrap();
        reply
            .surfaces
            .into_iter()
            .filter(|w| w.owner_uid == uid)
            .find(|w| w.rect.w == 1040 && w.rect.h == 720)
    });
    // The WM can enumerate the surface before the first Settings frame.
    sleep(Duration::from_secs(3));
    let (x, y) = (settings.rect.x + offset, settings.rect.y + 222);
    let started = Instant::now();
    inject_input(inject, &format!("MOVE {x} {y}\nBTN 1 1\nBTN 1 0\n"));
    let sheet = wait("Mail Settings opens the Arca sheet", || {
        wm.wm_enumerate_all()
            .unwrap()
            .embeds
            .into_iter()
            .find(|e| e[1] == settings.surface_id)
            .map(|e| e[0])
    });
    (sheet, started.elapsed())
}

fn drive(wm_path: &str, inject: &str, uid: u32) {
    let mut wm = Connection::connect(wm_path).unwrap();
    let (sheet, _) = settings_sheet(&mut wm, inject, uid, 80);
    println!("MAIL_SETTINGS_ARCA_SHEET={sheet}");
    for certificate in [true, false] {
        sleep(Duration::from_millis(1800));
        if !certificate {
            key(inject, 43); // Tab to Choose PKCS#8 private key.
        }
        enter_for_picker(&mut wm, inject, sheet);
        sleep(Duration::from_millis(400));
        key(inject, 81); // Select the first visible fixture.
        if !certificate {
            key(inject, 77); // End selects the private-key fixture.
        }
        key(inject, 40);
        wait("Scrinium dismisses after selection", || {
            (!wm.wm_enumerate_all()
                .unwrap()
                .embeds
                .iter()
                .any(|e| e[1] == sheet))
            .then_some(())
        });
    }
    sleep(Duration::from_millis(1800));
    key(inject, 43); // Tab to Add identity.
    key(inject, 40);
    wait("Mail dismisses the completed Arca sheet", || {
        (!wm.wm_enumerate_all()
            .unwrap()
            .embeds
            .iter()
            .any(|e| e[0] == sheet))
        .then_some(())
    });
}

#[cfg(feature = "legacy-smime-integration")]
fn drive_legacy(wm_path: &str, inject: &str, uid: u32, action: &str) {
    let (offset, profile) = match action {
        "approve" | "cancel" => (80, None),
        "missing-key" => (80, Some(action)),
        "unsupported" => (380, Some(action)),
        _ => panic!("unknown legacy sheet action"),
    };
    let mut wm = Connection::connect(wm_path).unwrap();
    let (sheet, selection_elapsed) = settings_sheet(&mut wm, inject, uid, offset);
    sleep(Duration::from_secs(2));
    assert!(
        wm.wm_enumerate_all()
            .unwrap()
            .embeds
            .iter()
            .any(|e| e[0] == sheet),
        "legacy sheet resolved before the user's press"
    );
    let usage = if action == "cancel" { 41 } else { 40 };
    let started = Instant::now();
    inject_input(inject, &format!("KEY {usage} 1 0\nKEY {usage} 0 0\n"));
    wait("Mail dismisses the legacy Arca sheet", || {
        (!wm.wm_enumerate_all()
            .unwrap()
            .embeds
            .iter()
            .any(|e| e[0] == sheet))
        .then_some(())
    });
    if let Some(profile) = profile {
        let approval_elapsed = started.elapsed();
        println!(
            "MAIL_LEGACY_PATH_TIMING class={profile} select_us={} approve_us={} total_us={}",
            selection_elapsed.as_micros(),
            approval_elapsed.as_micros(),
            (selection_elapsed + approval_elapsed).as_micros()
        );
    }
}

#[cfg(feature = "legacy-smime-integration")]
fn expect_legacy_denied(socket: &str, uid: u32) {
    use std::io::Read;
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    proto::write_request(
        &mut stream,
        proto::op::U_SMIME_SYNTHETIC_OPEN,
        &uid.to_le_bytes(),
    )
    .unwrap();
    // Read the fixed result header in the Mail jail without locking memory.
    let mut reply = [0; 5];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply[..4], &1u32.to_le_bytes());
    assert_ne!(reply[4], proto::status::OK);
    println!("MAIL_SIGNED_LEGACY_DIRECT_DENIED");
}

fn expect_certs(socket: &str, expected: &str) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    proto::write_request(&mut stream, proto::op::MAIL_SMIME_CERTS, &[]).unwrap();
    let (status, body) =
        proto::read_app_reply(&mut stream, proto::AppReply::SmimeCertificates).unwrap();
    assert_eq!(status, proto::status::OK);
    let mut reader = arca_store::codec::Reader::new(&body);
    if expected == "empty" {
        assert_eq!(reader.u32().unwrap(), 0);
    } else {
        assert_eq!(reader.u32().unwrap(), 1);
        assert_eq!(
            reader.bytes(proto::SMIME_CERT_MAX).unwrap(),
            std::fs::read(expected).unwrap()
        );
    }
    reader.end().unwrap();
    println!(
        "MAIL_SMIME_PUBLIC_CERTS={}",
        if expected == "empty" { 0 } else { 1 }
    );
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, wm, inject, uid] if mode == "drive" => drive(wm, inject, uid.parse().unwrap()),
        [mode, socket, expected] if mode == "certs" => expect_certs(socket, expected),
        #[cfg(feature = "legacy-smime-integration")]
        [mode, wm, inject, uid, action] if mode == "drive-legacy" => {
            drive_legacy(wm, inject, uid.parse().unwrap(), action)
        }
        #[cfg(feature = "legacy-smime-integration")]
        [mode, socket, uid] if mode == "legacy-denied" => {
            expect_legacy_denied(socket, uid.parse().unwrap())
        }
        _ => panic!("use drive WM INPUT UID, or certs ARCA_SOCKET DER_OR_EMPTY"),
    }
}
