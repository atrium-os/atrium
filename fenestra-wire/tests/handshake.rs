//! The session handshake on loopback (fenestra.md §11.2): pinned raw
//! public keys both ways, the K_sess proof bound to the connection, and
//! the stream limits each side enforces.

use std::time::Duration;

use fenestra_wire::{frame, Host, SessionKey, SessionSecret, Viewer, WireError};

fn secret(k: u8) -> SessionSecret {
    SessionSecret { id: [7; 16], k_sess: [k; 32] }
}

struct Pair {
    host_key: SessionKey,
    viewer_key: SessionKey,
}

fn keys() -> Pair {
    Pair { host_key: SessionKey::generate().unwrap(), viewer_key: SessionKey::generate().unwrap() }
}

fn bind(host_key: &SessionKey, viewer_pub: &[u8], s: SessionSecret) -> Host {
    Host::bind("127.0.0.1:0".parse().unwrap(), host_key, viewer_pub, s).unwrap()
}

/// Run the host's accept and the viewer's connect together.
/// The host's end is kept until both are done (dropping it closes the
/// connection, maybe before the welcome arrives).
async fn attempt(host: &Host, viewer: &Viewer, s: &SessionSecret) -> (Result<bool, WireError>, Result<fenestra_wire::HostConn, WireError>) {
    let addr = host.local_addr().unwrap();
    let h = async {
        match tokio::time::timeout(Duration::from_secs(15), host.accept()).await {
            Ok(r) => r,
            Err(_) => Err(WireError::Timeout),
        }
    };
    let v = viewer.connect(addr, s);
    let (h, v) = tokio::join!(h, v);
    (h.map(|c| c.is_some()), v)
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_keys_and_the_proof_connect() {
    let k = keys();
    let host = bind(&k.host_key, k.viewer_key.public(), secret(1));
    let viewer = Viewer::new(&k.viewer_key, k.host_key.public()).unwrap();
    let addr = host.local_addr().unwrap();
    let s1 = secret(1);
    let (hc, vc) = tokio::join!(host.accept(), viewer.connect(addr, &s1));
    let mut hc = hc.unwrap().expect("a connection");
    let mut vc = vc.unwrap();
    // A window stream from the host, and control both ways.
    let mut uni = hc.conn.open_uni().await.unwrap();
    uni.write_all(b"scene").await.unwrap();
    uni.finish().unwrap();
    let mut r = vc.conn.accept_uni().await.unwrap();
    assert_eq!(r.read_to_end(1024).await.unwrap(), b"scene");
    vc.control.send(&frame::Msg::App(b"ack".to_vec())).await.unwrap();
    assert_eq!(hc.control.recv().await.unwrap(), frame::Msg::App(b"ack".to_vec()));
    // Datagrams (acks, pointer motion).
    vc.conn.send_datagram(bytes(b"dg")).unwrap();
    assert_eq!(&hc.conn.read_datagram().await.unwrap()[..], b"dg");
    // Both ends see the same binding; another connection has another.
    let a = fenestra_wire::connection_binding(&hc.conn, &[7; 16]).unwrap();
    let b = fenestra_wire::connection_binding(&vc.conn, &[7; 16]).unwrap();
    assert_eq!(a, b);
    let (hc2, vc2) = tokio::join!(host.accept(), viewer.connect(addr, &s1));
    let hc2 = hc2.unwrap().unwrap();
    let _vc2 = vc2.unwrap();
    assert_ne!(fenestra_wire::connection_binding(&hc2.conn, &[7; 16]).unwrap(), a);
}

fn bytes(b: &[u8]) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(b)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_key_not_pinned_is_refused() {
    let k = keys();
    let other = SessionKey::generate().unwrap();
    let host = bind(&other, k.viewer_key.public(), secret(1));
    let viewer = Viewer::new(&k.viewer_key, k.host_key.public()).unwrap();
    let (h, v) = attempt(&host, &viewer, &secret(1)).await;
    assert!(v.is_err(), "the viewer took a host key it had not pinned");
    assert!(!matches!(h, Ok(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_key_not_pinned_is_refused() {
    let k = keys();
    let other = SessionKey::generate().unwrap();
    let host = bind(&k.host_key, k.viewer_key.public(), secret(1));
    let viewer = Viewer::new(&other, k.host_key.public()).unwrap();
    let (h, v) = attempt(&host, &viewer, &secret(1)).await;
    assert!(!matches!(h, Ok(true)), "the host took a viewer key it had not pinned");
    assert!(v.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_session_secret_is_refused() {
    let k = keys();
    let host = bind(&k.host_key, k.viewer_key.public(), secret(1));
    let viewer = Viewer::new(&k.viewer_key, k.host_key.public()).unwrap();
    let (h, v) = attempt(&host, &viewer, &secret(2)).await;
    assert!(matches!(h, Err(WireError::Proof)), "{:?}", h.err());
    assert!(v.is_err());
    // And a wrong session id with the right key.
    let wrong_id = SessionSecret { id: [8; 16], k_sess: [1; 32] };
    let (h, v) = attempt(&host, &viewer, &wrong_id).await;
    assert!(matches!(h, Err(WireError::Proof)));
    assert!(v.is_err());
    // The right one still works after.
    let (h, v) = attempt(&host, &viewer, &secret(1)).await;
    assert!(matches!(h, Ok(true)), "{:?}", h.err());
    assert!(v.is_ok(), "{:?}", v.err());
}

/// The host takes no stream from a viewer but the control stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_cannot_open_more_streams() {
    let k = keys();
    let host = bind(&k.host_key, k.viewer_key.public(), secret(1));
    let viewer = Viewer::new(&k.viewer_key, k.host_key.public()).unwrap();
    let addr = host.local_addr().unwrap();
    let s1 = secret(1);
    let (hc, vc) = tokio::join!(host.accept(), viewer.connect(addr, &s1));
    let _hc = hc.unwrap().unwrap();
    let vc = vc.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(500), vc.conn.open_uni()).await.is_err(), "a uni stream opened");
    assert!(tokio::time::timeout(Duration::from_millis(500), vc.conn.open_bi()).await.is_err(), "a second bi stream opened");
}

/// Control frames past the bound are refused before they are buffered.
#[test]
fn control_frames_decode_strictly() {
    assert!(frame::Msg::decode(2, &[0; 31]).is_err());
    assert!(frame::Msg::decode(3, &[0; 47]).is_err());
    assert!(frame::Msg::decode(1, &[0]).is_err());
    assert!(frame::Msg::decode(99, &[]).is_err());
    let m = frame::Msg::Proof { id: [1; 16], tag: [2; 32] };
    let e = m.encode();
    assert_eq!(frame::Msg::decode(e[4], &e[5..]).unwrap(), m);
}

#[test]
fn scene_framing_round_trips_and_refuses() {
    use fenestra_wire::scene::{Ack, Control, WindowHeader};
    let h = WindowHeader { window: 9, epoch: 77 };
    assert_eq!(WindowHeader::decode(&h.encode()).unwrap(), h);
    let mut bad = h.encode();
    bad[0] ^= 1;
    assert!(WindowHeader::decode(&bad).is_err());
    for a in [Ack::Holds { window: 3, epoch: 4, version: 5 }, Ack::Whole { window: 3 }] {
        assert_eq!(Ack::decode(&a.encode()).unwrap(), a);
    }
    assert!(Ack::decode(&[1; 20]).is_err());
    assert!(Ack::decode(&[9; 21]).is_err());
    let r = Control::Resume(vec![(1, 2, 3), (4, 5, 6)]);
    assert_eq!(Control::decode(&r.encode()).unwrap(), r);
    let mut lie = r.encode();
    lie[1] = 200;
    assert!(Control::decode(&lie).is_err());
    assert!(Control::decode(&[]).is_err());
    assert_eq!(Control::decode(&Control::Closed(5).encode()).unwrap(), Control::Closed(5));
    let l = Control::Layout { screen: (1280, 720), desktop: true, windows: vec![(2, -5, 30, 640, 480), (7, 0, 0, 1, 1)] };
    assert_eq!(Control::decode(&l.encode()).unwrap(), l);
    // A mode not 0 or 1, a count past the body, a window placed twice, a
    // zero side: refused.
    let mut lie = l.encode();
    lie[9] = 2;
    assert!(Control::decode(&lie).is_err());
    let mut lie = l.encode();
    lie[10] = 3;
    assert!(Control::decode(&lie).is_err());
    let twice = Control::Layout { screen: (9, 9), desktop: false, windows: vec![(2, 0, 0, 1, 1); 2] };
    assert!(Control::decode(&twice.encode()).is_err());
    let zero = Control::Layout { screen: (9, 9), desktop: false, windows: vec![(2, 0, 0, 0, 1)] };
    assert!(Control::decode(&zero.encode()).is_err());
    // A viewer's output: in range, or refused.
    let o = Control::Output { width: 1600, height: 900, scale_milli: 2000 };
    assert_eq!(Control::decode(&o.encode()).unwrap(), o);
    for bad in [(63, 900, 1000), (1600, 8193, 1000), (1600, 900, 499), (1600, 900, 4001)] {
        let c = Control::Output { width: bad.0, height: bad.1, scale_milli: bad.2 };
        assert!(Control::decode(&c.encode()).is_err(), "{bad:?}");
    }
    assert!(Control::decode(&o.encode()[..12]).is_err());
    use fenestra_wire::scene::Input;
    for i in [
        Input::Key { usage: 0x28, pressed: true, modifiers: 2 },
        Input::Motion { x: 12.5, y: 700.0 },
        Input::Button { button: 1, pressed: false },
        Input::Scroll { dx: 0.0, dy: -120.0 },
        Input::MotionIn { window: 4, x: 0.0, y: 31.5 },
        Input::Focus { window: 4 },
        Input::Resize { window: 4, width: 900, height: 600 },
    ] {
        assert_eq!(Control::decode(&Control::Input(i).encode()).unwrap(), Control::Input(i));
    }
    assert!(Control::decode(&Control::Input(Input::Motion { x: f32::INFINITY, y: 0.0 }).encode()).is_err());
    assert!(Control::decode(&Control::Input(Input::Button { button: 0, pressed: true }).encode()).is_err());
    assert!(Control::decode(&[4, 1, 0, 2, 0]).is_err());
    assert!(Control::decode(&Control::Input(Input::MotionIn { window: 0, x: 1.0, y: 1.0 }).encode()).is_err(), "window 0");
    assert!(Control::decode(&Control::Input(Input::MotionIn { window: 70000, x: 1.0, y: 1.0 }).encode()).is_err(), "past u16");
    assert!(Control::decode(&Control::Input(Input::MotionIn { window: 4, x: -0.5, y: 1.0 }).encode()).is_err(), "off the window");
    assert!(Control::decode(&Control::Input(Input::Focus { window: 0 }).encode()).is_err());
    assert!(Control::decode(&Control::Input(Input::Resize { window: 4, width: 63, height: 600 }).encode()).is_err(), "too small");
    assert!(Control::decode(&Control::Input(Input::Resize { window: 4, width: 900, height: 9000 }).encode()).is_err(), "too large");
}

#[test]
fn mint_lines_round_trip_and_refuse_the_rest() {
    use fenestra_wire::mint::{self, DevMint};
    let m = DevMint { addr: "192.0.2.7:24300".parse().unwrap(), host_key: vec![7; 44], secret: mint::fresh_secret().unwrap() };
    let back = DevMint::parse_line(&m.line()).unwrap();
    assert_eq!((back.addr, back.host_key.clone(), back.secret.id, back.secret.k_sess), (m.addr, m.host_key.clone(), m.secret.id, m.secret.k_sess));
    let line = m.line();
    let w: Vec<&str> = line.split(' ').collect();
    let bad = [
        line.replace("k_sess", "k_sess2"),                              // unknown field
        w[..6].join(" "),                                               // a field missing
        format!("{} addr {}", w[..6].join(" "), w[1]),                  // a field twice, one missing
        line.replace(&format!("id {}", w[5]), &format!("id {}", &w[5][2..])), // id of the wrong length
        line.replace(w[1], "not-an-address"),
        format!("{line} {}", "a".repeat(mint::MAX_LINE)),
    ];
    for b in bad {
        assert!(DevMint::parse_line(&b).is_err(), "took {b:?}");
    }
}

/// UDP blocked (§8.6): the same connection over TCP on the session's
/// port, the same pins and proof; a wrong secret is refused there too,
/// and a peer that sends an oversized datagram is cut off.
#[tokio::test(flavor = "multi_thread")]
async fn the_tcp_fallback_carries_the_same_session() {
    let k = keys();
    let host = bind(&k.host_key, k.viewer_key.public(), secret(1));
    assert!(host.has_tcp());
    let viewer = Viewer::new(&k.viewer_key, k.host_key.public()).unwrap();
    let addr = host.local_addr().unwrap();
    let s1 = secret(1);
    let (hc, vc) = tokio::join!(host.accept(), viewer.connect_tcp(addr, &s1));
    let hc = hc.unwrap().expect("a connection over TCP");
    let vc = vc.unwrap();
    let mut uni = hc.conn.open_uni().await.unwrap();
    uni.write_all(&vec![9u8; 300_000]).await.unwrap();
    uni.finish().unwrap();
    let mut r = vc.conn.accept_uni().await.unwrap();
    assert_eq!(r.read_to_end(1 << 20).await.unwrap(), vec![9u8; 300_000]);
    drop((hc, vc));

    let bad = secret(2);
    let (h, v) = tokio::join!(
        async { tokio::time::timeout(Duration::from_secs(15), host.accept()).await.unwrap_or(Err(WireError::Timeout)) },
        viewer.connect_tcp(addr, &bad)
    );
    assert!(matches!(h, Err(WireError::Proof)), "{:?}", h.map(|c| c.is_some()));
    assert!(v.is_err());

    // A raw TCP peer declaring a datagram past the bound is dropped.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(&((fenestra_wire::tcp::MAX_DATAGRAM + 1) as u16).to_be_bytes()).await.unwrap();
    let mut b = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b)).await.expect("cut off, not left open");
    assert!(matches!(r, Ok(0) | Err(_)));
}

/// The TCP port is open before any proof: one address holds at most
/// PER_SOURCE peers, and a peer that proves nothing is closed after
/// PROVE_WITHIN, so the real viewer gets in afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_peers_that_prove_nothing_are_bounded_and_closed() {
    use fenestra_wire::tcp::{PER_SOURCE, PROVE_WITHIN};
    use tokio::io::AsyncReadExt;
    let k = keys();
    let host = std::sync::Arc::new(bind(&k.host_key, k.viewer_key.public(), secret(1)));
    let addr = host.local_addr().unwrap();
    let mut squatters = Vec::new();
    for _ in 0..PER_SOURCE {
        squatters.push(tokio::net::TcpStream::connect(addr).await.unwrap());
    }
    // One more from the same address is closed at once.
    let mut extra = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut b = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_secs(5), extra.read(&mut b)).await.expect("refused, not held");
    assert!(matches!(r, Ok(0) | Err(_)));
    // The squatters are closed once their time to prove is up.
    for s in &mut squatters {
        let r = tokio::time::timeout(PROVE_WITHIN + Duration::from_secs(5), s.read(&mut b)).await.expect("closed after the deadline");
        assert!(matches!(r, Ok(0) | Err(_)));
    }
    // And the viewer gets in, and stays past the deadline once proven.
    let viewer = Viewer::new(&k.viewer_key, k.host_key.public()).unwrap();
    let s1 = secret(1);
    let h = host.clone();
    let accepting = tokio::spawn(async move { h.accept().await });
    let vc = viewer.connect_tcp(addr, &s1).await.expect("the viewer over TCP");
    let hc = accepting.await.unwrap().unwrap().expect("a connection");
    tokio::time::sleep(PROVE_WITHIN + Duration::from_secs(2)).await;
    let mut uni = hc.conn.open_uni().await.unwrap();
    uni.write_all(b"still here").await.unwrap();
    uni.finish().unwrap();
    let mut r = vc.conn.accept_uni().await.unwrap();
    assert_eq!(r.read_to_end(64).await.unwrap(), b"still here");
}
