#![cfg(target_os = "freebsd")]

use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arca_store::codec::Writer;
use arcad::label::Label;
use arcad::proto::{op, status};
use arcad::server::State;

#[test]
fn provisioning_tool_imports_from_read_only_files_and_forgets_by_public_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = State::new(dir.path().join("db")).unwrap();
    state.mail_sign_ins.mail_publisher = Some([7; 32]);
    let mut create = Writer::new();
    create.str("alice");
    create.raw(&[1; 32]);
    assert_eq!(
        state
            .handle(&Label::Vestibulum, op::V_CREATE, &create.buf)
            .status,
        status::OK
    );
    let state = Arc::new(Mutex::new(state));
    let socket = dir.path().join("arca.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let broker = state.clone();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(15);
        for _ in 0..2 {
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < until => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => panic!("Arca UI request did not arrive: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            arcad::serve_conn(stream, &Label::TrustedUi, &broker).unwrap();
        }
    });

    let mut key = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    key.write_all(include_bytes!(
        "../../../mail/mail-smime/tests/data/leaf.p8"
    ))
    .unwrap();
    let certificate = dir.path().join("leaf.der");
    let der = include_bytes!("../../../mail/mail-smime/tests/data/leaf.der");
    std::fs::write(&certificate, der).unwrap();
    let command = || {
        let app =
            std::env::var_os("ARCA_UI_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_arca-ui").into());
        let mut command = Command::new(app);
        command.env("ARCA_SOCKET", &socket);
        command
    };
    let import = || {
        let mut app = command();
        app.args(["smime-import", "alice", "work", "Synthetic key"])
            .arg(&certificate);
        app
    };
    let bad = import().stdin(Stdio::piped()).output().unwrap();
    assert!(
        !bad.status.success(),
        "a pipe must be refused before opening the Arca socket"
    );
    assert!(bad.stdout.is_empty());

    let result = import()
        .stdin(File::open(key.path()).unwrap())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let fingerprint = String::from_utf8(result.stdout).unwrap();
    let fingerprint = fingerprint.trim();
    assert_eq!(
        fingerprint,
        "cb932325632285657309a671a0661bef63b850c41e67fa8efa79684bf1744270"
    );

    let work = Label::App {
        user: "alice".into(),
        publisher: [7; 32],
        app_id: "org.atrium.mail".into(),
        persona: Some("work".into()),
    };
    let mut expected = Writer::new();
    expected.u32(1);
    expected.bytes(der);
    let reply = state
        .lock()
        .unwrap()
        .handle(&work, op::MAIL_SMIME_CERTS, &[]);
    assert_eq!(reply.status, status::OK);
    assert_eq!(reply.body.bytes(), expected.buf);
    let forgotten = command()
        .args(["smime-forget", "alice", "work", fingerprint])
        .output()
        .unwrap();
    assert!(
        forgotten.status.success(),
        "{}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    assert_eq!(
        state
            .lock()
            .unwrap()
            .handle(&work, op::MAIL_SMIME_CERTS, &[])
            .body
            .bytes(),
        [0; 4]
    );
    server.join().unwrap();
}
