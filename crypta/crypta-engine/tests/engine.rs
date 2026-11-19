//! The engine binary on the host, as cryptad runs it: its control socket on
//! fd 0, a datagram socketpair standing in for the tun on fd 3, and a
//! crypta-wg peer on a UDP socket as the server. The test plays cryptad
//! (Start, then DH answers with a key it holds) and the kernel (packets in
//! and out of the "tun").

use std::io::{Read, Write};
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crypta_proto::engine::{self, DhShared, FromEngine, Start, ToEngine};
use crypta_proto::net::Prefix4;
use crypta_wg::{Action, Device, LocalKey, Peer, StaticKey};

fn packet(src: [u8; 4], dst: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 20];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&((20 + body.len()) as u16).to_be_bytes());
    p[8] = 64;
    p[9] = 253;
    p[12..16].copy_from_slice(&src);
    p[16..20].copy_from_slice(&dst);
    p.extend_from_slice(body);
    p
}

fn with_hdr(p: &[u8]) -> Vec<u8> {
    let mut f = 2u32.to_be_bytes().to_vec();
    f.extend_from_slice(p);
    f
}

#[test]
fn the_engine_carries_packets_with_its_key_held_elsewhere() {
    let ours = Arc::new(LocalKey::generate());
    let theirs = Arc::new(LocalKey::generate());
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut sdev = Device::new(theirs.clone());
    sdev.add_peer(Peer { public: ours.public(), psk: Some([7; 32]), persistent_keepalive: None });

    let (mut ctl, ctl_engine) = UnixStream::pair().unwrap();
    let (tun, tun_engine) = UnixDatagram::pair().unwrap();
    tun.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let (c, t) = (ctl_engine.as_raw_fd(), tun_engine.as_raw_fd());
    let mut child = unsafe {
        Command::new(env!("CARGO_BIN_EXE_atrium-crypta-wg"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .pre_exec(move || {
                if libc::dup2(c, 0) < 0 || libc::dup2(c, 1) < 0 || libc::dup2(t, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
            .unwrap()
    };
    drop(ctl_engine);
    drop(tun_engine);
    let Ok(std::net::SocketAddr::V4(endpoint)) = server.local_addr() else { panic!() };
    engine::write(
        &mut ctl,
        &ToEngine::Start(Start {
            our_public: ours.public(),
            peer_public: theirs.public(),
            psk: Some([7; 32]),
            endpoint,
            keepalive_s: None,
            allowed: vec![Prefix4::parse("10.99.0.0/24").unwrap()],
        }),
    )
    .unwrap();
    ctl.set_nonblocking(true).unwrap();
    let mut rd = engine::Reader::default();

    let us = [10, 99, 0, 2];
    let them = [10, 99, 0, 1];
    // Into the tunnel: one allowed packet, one to a destination it may not reach.
    tun.send(&with_hdr(&packet(us, [192, 0, 2, 1], b"not allowed"))).unwrap();
    tun.send(&with_hdr(&packet(us, them, b"hello"))).unwrap();

    let t0 = Instant::now();
    let (mut dh_answered, mut up, mut server_got, mut reply_seen) = (0, false, Vec::new(), false);
    let mut sent_reply = false;
    let mut buf = vec![0u8; 70000];
    while t0.elapsed() < Duration::from_secs(20) && !(up && reply_seen) {
        // cryptad's side: answer every DH with our key.
        let mut b = [0u8; 4096];
        if let Ok(n) = ctl.read(&mut b) {
            assert!(n > 0, "the engine closed its control socket");
            rd.push(&b[..n]);
        }
        while let Some(m) = rd.next::<FromEngine>().unwrap() {
            match m {
                FromEngine::Dh { point } => {
                    let s = ours.dh(&point).ok().map(|s| DhShared(*s));
                    dh_answered += 1; eprintln!("dh at {:?}", t0.elapsed());
                    ctl.set_nonblocking(false).unwrap();
                    engine::write(&mut ctl, &ToEngine::Dh(s)).unwrap();
                    ctl.set_nonblocking(true).unwrap();
                }
                FromEngine::Up => { up = true; eprintln!("up at {:?}", t0.elapsed()); }
                FromEngine::Down { why } => panic!("down: {why}"),
                FromEngine::Stats { .. } => {}
                other => panic!("not WireGuard's: {other:?}"),
            }
        }
        // The server.
        let now = t0.elapsed();
        let mut acts = sdev.peer(0).tick(now);
        if let Ok((n, from)) = server.recv_from(&mut buf) {
            let std::net::SocketAddr::V4(f) = from else { panic!() };
            let mut src = f.ip().octets().to_vec();
            src.extend_from_slice(&f.port().to_be_bytes());
            let (_, a) = sdev.decapsulate(&src, &buf[..n], now);
            acts.extend(a);
            for a in &acts {
                if let Action::Send(d) = a {
                    server.send_to(d, from).unwrap();
                }
            }
            for a in &acts {
                if let Action::Deliver(p) = a {
                    server_got.push(p.clone());
                }
            }
            if !sent_reply && !server_got.is_empty() {
                sent_reply = true;
                // One from an allowed source, one from outside the peer's range.
                for a in sdev.peer(0).encapsulate(&packet([198, 51, 100, 9], us, b"spoofed"), now) {
                    if let Action::Send(d) = a {
                        server.send_to(&d, from).unwrap();
                    }
                }
                for a in sdev.peer(0).encapsulate(&packet(them, us, b"world"), now) {
                    if let Action::Send(d) = a {
                        server.send_to(&d, from).unwrap();
                    }
                }
            }
        }
        for a in acts {
            if let Action::Deliver(p) = a {
                if !server_got.contains(&p) {
                    server_got.push(p);
                }
            }
        }
        if let Ok(n) = tun.recv(&mut buf) {
            assert_eq!(&buf[..4], &2u32.to_be_bytes());
            let p = &buf[4..n];
            assert_eq!(&p[12..16], &them, "only the peer's allowed sources reach the tun");
            assert_eq!(&p[20..], b"world");
            reply_seen = true; eprintln!("reply at {:?}", t0.elapsed());
        }
    }
    let _ = engine::write(&mut ctl, &ToEngine::Stop);
    let st = child.wait().unwrap();
    assert!(up, "no handshake");
    assert!(dh_answered >= 2, "the engine did its static-key work through us: {dh_answered}");
    assert_eq!(server_got.len(), 1, "only the allowed packet went through: {server_got:?}");
    assert_eq!(&server_got[0][20..], b"hello");
    assert!(reply_seen);
    assert!(st.success());
    let _ = Ipv4Addr::LOCALHOST;
    let _ = (&mut ctl).flush();
}
