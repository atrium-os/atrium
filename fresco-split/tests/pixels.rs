//! Pixel slots (compute renderer pixel mode): what the broker accepts.
use fresco_split::slot::{pixel_slot_bytes, read_pixels, write_pixels};

#[test]
fn pixels_round_trip() {
    let px: Vec<u32> = (0..6 * 4).map(|i| 0xff00_0000 | i).collect();
    let mut s = vec![0u8; pixel_slot_bytes(6, 4)];
    write_pixels(&mut s, 6, 4, &px, 7).unwrap();
    assert_eq!(read_pixels(&s, 6, 4).unwrap(), (7, px));
}

#[test]
fn the_broker_refuses_what_it_did_not_decide() {
    let px = vec![0u32; 6 * 4];
    let mut s = vec![0u8; pixel_slot_bytes(8, 8)];
    write_pixels(&mut s, 6, 4, &px, 1).unwrap();
    // A size other than the window's, a short slot, a frame slot's magic,
    // reserved words set: all refused.
    assert!(read_pixels(&s, 8, 8).is_err());
    assert!(read_pixels(&s[..pixel_slot_bytes(6, 4) - 1], 6, 4).is_err());
    let mut t = s.clone();
    t[0] ^= 1;
    assert!(read_pixels(&t, 6, 4).is_err());
    let mut t = s.clone();
    t[16] = 1;
    assert!(read_pixels(&t, 6, 4).is_err());
    // The renderer cannot write past its slot either.
    assert!(write_pixels(&mut s[..pixel_slot_bytes(6, 4) - 1], 6, 4, &px, 1).is_err());
    assert!(write_pixels(&mut s, 6, 4, &px[1..], 1).is_err());
}
