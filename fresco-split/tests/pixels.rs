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

#[test]
fn damage_rect_round_trips_and_only_it_is_copied() {
    use fresco_split::slot::{copy_pixels, pixel_header, write_pixels_damage};
    let a: Vec<u32> = (0..24).collect();
    let mut b = a.clone();
    b[6 + 2] = 99; // (2, 1)
    b[2 * 6 + 3] = 98; // (3, 2)
    let mut s = vec![0u8; pixel_slot_bytes(6, 4)];
    write_pixels_damage(&mut s, 6, 4, &a, 1, None, None).unwrap();
    assert_eq!(pixel_header(&s, 6, 4).unwrap(), (1, None));
    write_pixels_damage(&mut s, 6, 4, &b, 2, Some([2, 1, 4, 3]), Some([2, 1, 4, 3])).unwrap();
    assert_eq!(pixel_header(&s, 6, 4).unwrap(), (2, Some([2, 1, 4, 3])));
    // The slot holds the whole new drawing.
    assert_eq!(read_pixels(&s, 6, 4).unwrap(), (2, b.clone()));
    // A broker holding `a` patches just the rect.
    let mut held = a.clone();
    copy_pixels(&s, 6, 4, &mut held, Some([2, 1, 4, 3])).unwrap();
    assert_eq!(held, b);
    // Rects outside the window are refused on both sides.
    assert!(write_pixels_damage(&mut s, 6, 4, &b, 3, None, Some([0, 0, 7, 1])).is_err());
    assert!(write_pixels_damage(&mut s, 6, 4, &b, 3, Some([2, 2, 2, 3]), None).is_err());
    let mut t = s.clone();
    t[24..28].copy_from_slice(&9u32.to_le_bytes()); // x1 past the width
    assert!(pixel_header(&t, 6, 4).is_err());
}
