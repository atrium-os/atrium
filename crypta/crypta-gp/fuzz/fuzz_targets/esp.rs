//! Exercise datagram parsing and replay handling with a synthetic association.
#![no_main]

use crypta_gp::esp::{self, Error, Key, Receive, Send};
use libfuzzer_sys::fuzz_target;
use zeroize::Zeroizing;

fn key() -> Key {
    Key::new(
        0x0123_4567,
        Zeroizing::new([0x11; 16]),
        Zeroizing::new([0x22; 20]),
    )
    .unwrap()
}

fuzz_target!(|data: &[u8]| {
    let _ = Receive::new(key()).packet(data);

    // The synthetic sender gives the receiver valid packets as well as
    // arbitrary wire input. A rejected packet cannot consume its sequence.
    let payload = &data[..data.len().min(1480)];
    let mut packet = vec![0; 20];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    packet.extend_from_slice(payload);
    let wire = match Send::new(key()).packet(&packet) {
        Ok(wire) => wire,
        Err(Error::Entropy) => return,
        Err(error) => panic!("valid synthetic packet refused: {error:?}"),
    };
    assert!(wire.len() <= esp::MAX_WIRE);
    let mut receiver = Receive::new(key());
    let mut altered = wire.clone();
    let bit = data.first().copied().unwrap_or(0) & 7;
    *altered.last_mut().unwrap() ^= 1 << bit;
    assert_eq!(receiver.packet(&altered), Err(Error::Authentication));
    assert_eq!(receiver.packet(&wire).unwrap().as_slice(), packet);
    assert_eq!(receiver.packet(&wire), Err(Error::Replay));
});
