//! The chrome channel's records.

use fresco_split::chrome::ChromeMsg;
use fresco_split::MSG_BYTES;

#[test]
fn chrome_records_round_trip_and_refuse_the_rest() {
    for m in [
        ChromeMsg::OpenClient,
        ChromeMsg::Opened { handle: 0x0102_0304 },
        ChromeMsg::SetOrigin { handle: 7, window: 3, origin: "https://xn--bcher-kva.example:8443".into() },
        ChromeMsg::SetOrigin { handle: 7, window: 3, origin: String::new() },
    ] {
        assert_eq!(ChromeMsg::decode(&m.encode()), Ok(m.clone()), "{m:?}");
    }
    let bad = |origin: &str| ChromeMsg::SetOrigin { handle: 1, window: 1, origin: origin.into() }.encode();
    for o in ["https://Example.com", "https://exa mple.com", "https://bаnk.com" /* Cyrillic а */, "a\u{7}b"] {
        assert!(ChromeMsg::decode(&bad(o)).is_err(), "{o:?} accepted");
    }
    let mut r = ChromeMsg::OpenClient.encode();
    r[8] = 1;
    assert!(ChromeMsg::decode(&r).is_err(), "dirty field");
    let mut r = ChromeMsg::SetOrigin { handle: 1, window: 0, origin: "a".into() }.encode();
    assert!(ChromeMsg::decode(&r).is_err(), "window 0");
    r = [0; MSG_BYTES];
    assert!(ChromeMsg::decode(&r).is_err(), "kind 0");
}

/// As for the renderer records: whatever decodes re-encodes to the same
/// bytes, and only origins of the allowed characters get through.
#[test]
fn fuzzed_chrome_records_decode_canonically_or_not_at_all() {
    use fresco_compute::fuzz::Rng;
    let mut r = Rng(0xc4f3);
    let base: Vec<[u8; MSG_BYTES]> = [
        ChromeMsg::OpenClient,
        ChromeMsg::Opened { handle: 9 },
        ChromeMsg::SetOrigin { handle: 7, window: 3, origin: "https://a.example".into() },
        ChromeMsg::SetOrigin { handle: 1, window: 65535, origin: "x".repeat(64) },
    ]
    .iter()
    .map(ChromeMsg::encode)
    .collect();
    let (mut ok, mut refused) = (0, 0);
    for i in 0..200_000 {
        let mut b = base[i % base.len()];
        if i % 7 == 0 {
            b.iter_mut().for_each(|x| *x = r.next() as u8);
        } else {
            for _ in 0..1 + r.below(3) {
                b[r.below(MSG_BYTES as u32) as usize] = r.next() as u8;
            }
        }
        match ChromeMsg::decode(&b) {
            Ok(m) => {
                assert_eq!(m.encode(), b, "{m:?} is not canonical");
                if let ChromeMsg::SetOrigin { origin, window, .. } = &m {
                    assert!(origin.bytes().all(fresco_split::chrome::origin_char) && origin.len() <= 64);
                    assert!((1..=65535).contains(window));
                }
                ok += 1;
            }
            Err(_) => refused += 1,
        }
    }
    assert!(ok > 1000 && refused > 1000, "{ok} decoded, {refused} refused");
}
