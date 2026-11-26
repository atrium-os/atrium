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
}
