//! A complete TLS record must round-trip and all proper prefixes must wait.
#![no_main]

use crypta_gp::ssl::{self, Frame};
use libfuzzer_sys::fuzz_target;

fn check_frames(mut data: &[u8], split: usize) {
    while let Ok(Some((frame, rest))) = ssl::decode(data) {
        assert!(rest.len() < data.len());
        let consumed = data.len() - rest.len();
        let prefix = split % consumed;
        assert_eq!(ssl::decode(&data[..prefix]), Ok(None));
        assert_eq!(ssl::encode(frame).unwrap(), &data[..consumed]);
        data = rest;
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&split, payload)) = data.split_first() else {
        return;
    };
    check_frames(payload, usize::from(split));

    // Keep the length fields consistent so mutations reach the packet path.
    let payload = &payload[..payload.len().min(ssl::MAX_PACKET - 40)];
    let mut packet = vec![0; 40];
    packet[0] = 0x60;
    packet[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    packet.extend_from_slice(payload);
    let mut wire = ssl::encode(Frame::Packet(&packet)).unwrap();
    wire.extend_from_slice(&ssl::encode(Frame::Keepalive).unwrap());
    check_frames(&wire, usize::from(split));
});
