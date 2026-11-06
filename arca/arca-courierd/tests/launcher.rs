//! The launcher's parts that don't need a jail.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;

use arca_courierd::*;
use navigator_fetch::addr::Policy;

#[test]
fn a_request_is_an_https_endpoint_and_nothing_more() {
    let f = encode_request("api.example", 443, None);
    let body = &f[4..];
    assert_eq!(u32::from_le_bytes(f[..4].try_into().unwrap()) as usize, body.len());
    assert_eq!(decode_request(body), Some(("api.example".into(), 443, None)));
    let w = encode_request("cdn.example", 443, Some("news.example"));
    assert_eq!(decode_request(&w[4..]), Some(("cdn.example".into(), 443, Some("news.example".into()))));
    let bad = encode_request("cdn.example", 443, Some("not a host"));
    assert_eq!(decode_request(&bad[4..]), None, "a partition is a site's host");
    let mut extra = body.to_vec();
    extra.push(0);
    assert_eq!(decode_request(&extra), None, "trailing bytes");
    let mut http = body.to_vec();
    http[0] = 0;
    assert_eq!(decode_request(&http), None, "not https");
    assert_eq!(decode_request(&body[..body.len() - 1]), None, "short");
    // An oversized frame is refused before it is read.
    let mut big = (100_000u32).to_le_bytes().to_vec();
    big.extend(std::iter::repeat_n(0u8, 16));
    assert!(read_request(&mut &big[..]).is_err());
}

#[test]
fn only_public_ipv4_addresses_reach_the_anchor() {
    let a = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let got = admit(
        [a("93.184.216.34:443"), a("127.0.0.1:443"), a("10.0.0.1:443"), a("169.254.169.254:443"), a("[2606:2800::1]:443"), a("93.184.216.34:443")],
        Policy { grant_private_network: false },
    );
    assert_eq!(got, vec![Ipv4Addr::new(93, 184, 216, 34)]);
    let lan = admit([a("10.0.2.2:443"), a("169.254.169.254:443")], Policy { grant_private_network: true });
    assert_eq!(lan, vec![Ipv4Addr::new(10, 0, 2, 2)], "a LAN grant never admits the metadata endpoint");
    let many: Vec<SocketAddr> = (1..=20).map(|i| SocketAddr::from(([93, 184, 216, i], 443))).collect();
    assert_eq!(admit(many, Policy { grant_private_network: false }).len(), MAX_ADDRS);
}

#[test]
fn uids_are_capped_per_app_and_reused_only_when_given_back() {
    let mut p = Pool::default();
    let first: Vec<u32> = (0..PER_APP).map(|_| p.take("a", PER_APP).unwrap()).collect();
    assert_eq!(first[0], arcad::courier::UID_FIRST);
    assert_eq!(p.take("a", PER_APP), Err(reply::TOO_MANY));
    let b = p.take("b", PER_APP).unwrap();
    assert!(!first.contains(&b));
    p.give_back(first[3]);
    assert_eq!(p.take("a", PER_APP), Ok(first[3]));
}

#[test]
fn the_courier_socket_rides_on_the_ok_byte() {
    let (l, r) = UnixStream::pair().unwrap();
    let (mut mine, theirs) = UnixStream::pair().unwrap();
    send_reply(&l, reply::OK, Some(theirs.as_fd())).unwrap();
    drop(theirs);
    let (code, fd) = recv_reply(&r).unwrap();
    assert_eq!(code, reply::OK);
    let mut got = UnixStream::from(fd.unwrap());
    mine.write_all(b"ping").unwrap();
    let mut b = [0u8; 4];
    got.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"ping");
    send_reply(&l, reply::NO_ADDRESS, None).unwrap();
    let (code, fd) = recv_reply(&r).unwrap();
    assert_eq!((code, fd.is_none()), (reply::NO_ADDRESS, true));
}

#[test]
fn the_registry_restarts_in_place() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("courier-registry");
    let p = p.to_str().unwrap();
    std::fs::write(p, "64000 alice app-00 https a.example 443\n").unwrap();
    let ino = std::fs::metadata(p).unwrap().ino();
    reset_registry(p).unwrap();
    assert_eq!(std::fs::read(p).unwrap(), b"");
    assert_eq!(std::fs::metadata(p).unwrap().ino(), ino, "same file, so arcad's mount still sees it");
}
