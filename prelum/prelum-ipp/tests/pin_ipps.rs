//! Host gate: real Arca request handling, framed Prelum worker, and IPPS simulator.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arca_store::codec::Writer;
use arcad::{label::Label, proto, server::State};
use base64::Engine;
use navigator_fetch::http::{Body, Framing, Head};
use prelum_ipp::worker;
use prelum_ipp_codec::ops::{op, Format, JobIdentity, Ticket};
use prelum_ipp_codec::{tag, Attribute, Group, Limits, Message, Value};
use prelum_proto::{
    frame,
    ipp::{Ev, LoginGrant, PinGrant, Req, Start},
};

const UUID: &str = "0bc4ab0f-6451-4bd5-a146-ca6ce118ba21";
const OTHER_UUID: &str = "55e86f65-fde1-497e-a716-569abe440755";
const PIN: &[u8] = b"0427";
const PDF: &[u8] = b"%PDF-1.4 simulator document";
const WAIT: Duration = Duration::from_secs(5);

fn accept<T>(mut get: impl FnMut() -> std::io::Result<T>) -> T {
    let until = Instant::now() + WAIT;
    loop {
        match get() {
            Ok(value) => return value,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("simulator accept failed: {e}"),
        }
    }
}

fn ipps(
    expected: &[(u16, u16)],
    login: Option<&'static [u8]>,
) -> (SocketAddr, [u8; 32], thread::JoinHandle<()>) {
    ipps_with_auth(expected, login, false)
}

fn ipps_with_auth(
    expected: &[(u16, u16)],
    login: Option<&'static [u8]>,
    digest: bool,
) -> (SocketAddr, [u8; 32], thread::JoinHandle<()>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["printer.local".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let spki = prelum_ipp::tls::spki_sha256(cert.der()).unwrap();
    let tls = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let expected = expected.to_vec();
    let task = thread::spawn(move || {
        for (code, status) in expected {
            let (socket, _) = accept(|| listener.accept());
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(WAIT)).unwrap();
            socket.set_write_timeout(Some(WAIT)).unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(tls.clone()).unwrap(),
                socket,
            );
            if code == 0 {
                // The client rejects this TLS key before sending any HTTP.
                assert!(matches!(stream.read(&mut [0u8; 1]), Ok(0) | Err(_)));
                continue;
            }
            serve_ipp(&mut stream, code, status, login, digest);
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    (addr, spki, task)
}

fn serve_ipp(
    stream: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
    code: u16,
    status: u16,
    login: Option<&[u8]>,
    digest: bool,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "POST /ipp/print HTTP/1.1\r\n");
    let mut framing = None;
    let mut authorization = None;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).unwrap();
        assert!((2..4096).contains(&n));
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Authorization: ") {
            authorization = Some(value.trim_end().to_string());
        } else if let Some(n) = line.strip_prefix("Content-Length: ") {
            framing = Some(Framing::Length(n.trim().parse().unwrap()));
        } else if line == "Transfer-Encoding: chunked\r\n" {
            framing = Some(Framing::Chunked);
        }
    }
    let expected_auth = login.filter(|_| !matches!(code, op::GET_PRINTER_ATTRIBUTES | op::VALIDATE_JOB));
    if digest && expected_auth.is_some() {
        use sha2::{Digest, Sha256};
        let value = authorization.as_deref().unwrap().strip_prefix("Digest ").unwrap();
        let params: std::collections::HashMap<_, _> = value.split(", ").map(|p| {
            let (key, value) = p.split_once('=').unwrap();
            (key, value.trim_matches('"'))
        }).collect();
        let hash = |value: &[u8]| format!("{:x}", Sha256::digest(value));
        assert_eq!(params["username"], "tester");
        assert_eq!(params["realm"], "fixture");
        assert_eq!(params["nonce"], "fresh");
        assert_eq!(params["opaque"], "echo");
        assert_eq!(params["uri"], "/ipp/print");
        assert_eq!(params["algorithm"], "SHA-256");
        assert_eq!(params["qop"], "auth");
        let count = if code == op::CREATE_JOB { "00000001" } else { "00000002" };
        assert_eq!(params["nc"], count);
        let a1 = hash(b"tester:fixture:fixture-pass");
        let a2 = hash(b"POST:/ipp/print");
        let expected = hash(format!("{a1}:fresh:{count}:{}:auth:{a2}", params["cnonce"]).as_bytes());
        assert!(params["response"] == expected, "invalid Digest response");
    } else {
        let expected = expected_auth.map(|value| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(value)));
        assert!(authorization == expected, "wrong Authorization header");
    }
    let head = Head {
        status: 200,
        headers: vec![],
        framing: framing.unwrap(),
    };
    let mut body = Body::new(&head, vec![]);
    let mut all = Vec::new();
    while let Some(bytes) = body.next(&mut reader, 4096).unwrap() {
        all.extend_from_slice(&bytes);
        assert!(all.len() < 8192);
    }
    let (req, offset) = Message::decode(&all, &Limits::default()).unwrap();
    assert_eq!(req.code, code);
    let password = req
        .attribute(tag::OPERATION, "job-password")
        .and_then(Attribute::first);
    if code == op::VALIDATE_JOB {
        assert!(password.is_none());
        assert!(all[offset..].is_empty());
        let challenge = if digest {
            "WWW-Authenticate: Basic realm=legacy, Digest realm=fixture, nonce=fresh, opaque=echo, algorithm=SHA-256, qop=\"auth-int, auth\"\r\n"
        } else {
            "WWW-Authenticate: Basic realm=fixture\r\n"
        };
        let stream = reader.into_inner();
        write!(stream, "HTTP/1.1 401 Unauthorized\r\n{challenge}Content-Length: 0\r\n\r\n").unwrap();
        stream.flush().unwrap();
        return;
    }
    let group = match code {
        op::GET_PRINTER_ATTRIBUTES => {
            assert!(password.is_none());
            assert!(all[offset..].is_empty());
            Group {
                tag: tag::PRINTER,
                attributes: vec![
                    Attribute::new("printer-uuid", Value::Uri(format!("urn:uuid:{UUID}"))),
                    Attribute::new(
                        "document-format-supported",
                        Value::MimeMediaType(Format::Pdf.mime().into()),
                    ),
                    Attribute::new("job-password-supported", Value::Integer(8)),
                    Attribute::many(
                        "operations-supported",
                        vec![
                            Value::Enum(op::CREATE_JOB.into()),
                            Value::Enum(op::SEND_DOCUMENT.into()),
                        ],
                    ),
                ],
            }
        }
        op::CREATE_JOB => {
            if login.is_some() {
                assert!(password.is_none());
            } else {
                assert!(matches!(password, Some(Value::OctetString(bytes)) if bytes == PIN));
            }
            assert_eq!(
                req.attribute(tag::OPERATION, "ipp-attribute-fidelity")
                    .and_then(Attribute::first),
                login.is_none().then_some(&Value::Boolean(true))
            );
            assert!(all[offset..].is_empty());
            Group {
                tag: tag::JOB,
                attributes: vec![Attribute::new("job-id", Value::Integer(17))],
            }
        }
        op::SEND_DOCUMENT => {
            assert!(password.is_none());
            assert_eq!(
                req.attribute(tag::OPERATION, "job-id")
                    .and_then(Attribute::first),
                Some(&Value::Integer(17))
            );
            assert_eq!(&all[offset..], PDF);
            Group {
                tag: tag::JOB,
                attributes: vec![],
            }
        }
        op::CANCEL_JOB => {
            assert!(password.is_none());
            assert_eq!(
                req.attribute(tag::OPERATION, "job-id")
                    .and_then(Attribute::first),
                Some(&Value::Integer(17))
            );
            assert!(all[offset..].is_empty());
            Group {
                tag: tag::JOB,
                attributes: vec![],
            }
        }
        _ => unreachable!(),
    };
    let reply = Message {
        version: (2, 0),
        code: status,
        request_id: req.request_id,
        groups: vec![group],
    }
    .encode();
    let stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.len()
    )
    .unwrap();
    stream.write_all(&reply).unwrap();
    stream.flush().unwrap();
}

fn job(addr: SocketAddr, key: [u8; 32], uuid: &str, handle: [u8; 16], arca: &Path) -> Vec<Ev> {
    submit(addr, key, uuid, handle, arca, false)
}

fn submit(
    addr: SocketAddr,
    key: [u8; 32],
    uuid: &str,
    handle: [u8; 16],
    arca: &Path,
    login: bool,
) -> Vec<Ev> {
    let mut frames = Vec::new();
    frame::write(
        &mut frames,
        &Start {
            uri: format!("ipps://{addr}/ipp/print"),
            addr,
            tls: true,
            key: Some(key),
            uuid: Some(uuid.into()),
            login: login.then_some(LoginGrant(handle)),
        },
    )
    .unwrap();
    let identity = JobIdentity {
        user: "atrium".into(),
        job_name: "Test".into(),
        job_password: None,
    };
    let ticket = Ticket::default();
    let request = if login {
        Req::Submit {
            identity,
            format: Format::Pdf,
            ticket,
        }
    } else {
        Req::SubmitPin {
            identity,
            format: Format::Pdf,
            ticket,
            grant: PinGrant {
                handle,
                user: "tester".into(),
                persona: "work".into(),
            },
        }
    };
    frame::write(&mut frames, &request).unwrap();
    frame::write(&mut frames, &Req::Data(PDF.to_vec())).unwrap();
    frame::write(&mut frames, &Req::End).unwrap();
    let mut out = Vec::new();
    worker::run(&mut &frames[..], &mut out, arca).unwrap();
    let mut response = &out[..];
    std::iter::from_fn(|| frame::read::<Ev>(&mut response).unwrap()).collect()
}

fn arca_call(path: &Path, op: u16, body: &[u8]) -> (u8, arca_store::secret::LockedBuf) {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    proto::write_request(&mut stream, op, body).unwrap();
    proto::read_reply(&mut stream).unwrap()
}

#[test]
fn arca_releases_once_after_key_and_uuid_match_then_ipps_receives_pin_before_pdf() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("arca.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = Mutex::new(State::new(dir.path().join("stores")).unwrap());
    let arca = thread::spawn(move || {
        // Standalone host test injects the roles normally assigned by peer UID.
        for label in [
            Label::Vestibulum,
            Label::Vestibulum,
            Label::TrustedUi,
            Label::PrintWorker,
            Label::PrintWorker,
            Label::TrustedUi,
            Label::PrintWorker,
        ] {
            let (stream, _) = accept(|| listener.accept());
            stream.set_nonblocking(false).unwrap();
            arcad::serve_conn(stream, &label, &state).unwrap();
        }
    });
    let (addr, key, printer) = ipps(
        &[
            (0, 0),
            (op::GET_PRINTER_ATTRIBUTES, 0),
            (op::GET_PRINTER_ATTRIBUTES, 0),
            (op::CREATE_JOB, 0),
            (op::SEND_DOCUMENT, 0),
            (op::GET_PRINTER_ATTRIBUTES, 0),
            (op::GET_PRINTER_ATTRIBUTES, 0),
            (op::CREATE_JOB, 1), // successful-ok-ignored-or-substituted-attributes
            (op::CANCEL_JOB, 0),
        ],
        None,
    );
    let mut root = Writer::new();
    root.str("tester");
    root.raw(&[5; 32]);
    for op in [proto::op::V_CREATE, proto::op::V_UNLOCK] {
        assert_eq!(arca_call(&socket, op, &root.buf).0, proto::status::OK);
    }
    let mut entry = Writer::new();
    entry.str("tester");
    entry.str("work");
    entry.raw(&key);
    entry.str(UUID);
    entry.bytes(PIN);
    let (st, reply) = arca_call(&socket, proto::op::U_HOLD_PRINT_PIN, &entry.buf);
    assert_eq!(st, proto::status::OK);
    let handle = reply.as_ref().try_into().unwrap();
    assert!(
        matches!(&job(addr, [0; 32], UUID, handle, &socket)[..], [Ev::IdentityChanged { key: Some(k), .. }] if *k == key)
    );
    assert!(
        matches!(&job(addr, key, OTHER_UUID, handle, &socket)[..], [Ev::IdentityChanged { uuid: Some(id), .. }] if id == UUID)
    );
    assert_eq!(
        job(addr, key, UUID, handle, &socket),
        [Ev::JobCreated { id: 17 }, Ev::Sent]
    );
    assert_eq!(
        job(addr, key, UUID, handle, &socket),
        [Ev::Failed(
            "Arca did not supply a usable PIN for this job".into()
        )]
    );
    let (st, reply) = arca_call(&socket, proto::op::U_HOLD_PRINT_PIN, &entry.buf);
    assert_eq!(st, proto::status::OK);
    assert_eq!(
        job(addr, key, UUID, reply.as_ref().try_into().unwrap(), &socket),
        [Ev::Failed("the printer did not accept the PIN job".into())]
    );
    printer.join().unwrap();
    arca.join().unwrap();
}

#[test]
fn saved_login_stays_out_of_identity_queries_and_authenticates_with_basic() {
    saved_login(false);
}

#[test]
fn saved_login_stays_out_of_identity_queries_and_authenticates_with_digest() {
    saved_login(true);
}

fn saved_login(digest: bool) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("arca.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = Mutex::new(State::new(dir.path().join("stores")).unwrap());
    let arca = thread::spawn(move || {
        for label in [
            Label::Vestibulum,
            Label::Vestibulum,
            Label::TrustedUi,
            Label::PrintBroker,
            Label::PrintWorker,
        ] {
            let (stream, _) = accept(|| listener.accept());
            stream.set_nonblocking(false).unwrap();
            arcad::serve_conn(stream, &label, &state).unwrap();
        }
    });
    let (addr, key, printer) = ipps_with_auth(
        &[
            (0, 0),
            (op::GET_PRINTER_ATTRIBUTES, 0),
            (op::GET_PRINTER_ATTRIBUTES, 0),
            (op::VALIDATE_JOB, 0),
            (op::CREATE_JOB, 0),
            (op::SEND_DOCUMENT, 0),
        ],
        Some(b"tester:fixture-pass"),
        digest,
    );
    let mut root = Writer::new();
    root.str("tester");
    root.raw(&[5; 32]);
    for op in [proto::op::V_CREATE, proto::op::V_UNLOCK] {
        assert_eq!(arca_call(&socket, op, &root.buf).0, proto::status::OK);
    }
    let mut entry = Writer::new();
    entry.str("tester");
    entry.str("work");
    entry.raw(&key);
    entry.str(UUID);
    let binding_end = entry.buf.len();
    entry.str("tester");
    entry.bytes(b"fixture-pass");
    assert_eq!(
        arca_call(&socket, proto::op::U_STORE_PRINT_LOGIN, &entry.buf).0,
        proto::status::OK
    );
    let (st, reply) = arca_call(
        &socket,
        proto::op::PRINT_LOGIN_GRANT,
        &entry.buf[..binding_end],
    );
    assert_eq!(st, proto::status::OK);
    let grant = reply.as_ref().try_into().unwrap();
    assert!(matches!(
        &submit(addr, [0; 32], UUID, grant, &socket, true)[..],
        [Ev::IdentityChanged { .. }]
    ));
    assert!(matches!(
        &submit(addr, key, OTHER_UUID, grant, &socket, true)[..],
        [Ev::IdentityChanged { .. }]
    ));
    assert_eq!(
        submit(addr, key, UUID, grant, &socket, true),
        [Ev::JobCreated { id: 17 }, Ev::Sent]
    );
    printer.join().unwrap();
    arca.join().unwrap();
}
