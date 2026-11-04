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
