use fenestra_link::{read, Input, Placed, Rec, HEADER, MAX_DATA, MAX_LAYOUT};

fn samples() -> Vec<Rec> {
    vec![
        Rec::Session { on: true },
        Rec::Session { on: false },
        Rec::Want { window: 3, ticket: 7, base: Some((0xfeed_0000_0000_0001, 99)) },
        Rec::Want { window: 3, ticket: 8, base: None },
        Rec::Changed { window: 65535 },
        Rec::Data { window: 1, ticket: 7, bytes: vec![1, 2, 3] },
        Rec::End { window: 1, ticket: 7, now: Some((5, 6)) },
        Rec::End { window: 1, ticket: 9, now: None },
        Rec::Layout { screen: (1280, 720), desktop: true, windows: vec![Placed { window: 2, x: -10, y: 30, width: 640, height: 480 }, Placed { window: 4, x: 0, y: 0, width: 1, height: 1 }] },
        Rec::Layout { screen: (1, 1), desktop: false, windows: vec![] },
        Rec::Output { width: 1600, height: 900, scale_milli: 2000 },
        Rec::Output { width: 64, height: 8192, scale_milli: 500 },
        Rec::Chrome(fenestra_link::chrome::Chrome::from_rects([10, 128, 140], (1280, 26), [([0.0, 0.0, 1280.0, 24.0], [0.08, 0.08, 0.1, 1.0], 0.0)]).unwrap()),
        Rec::Input(Input::Key { usage: 0x04, pressed: true, modifiers: 0x02 }),
        Rec::Input(Input::Key { usage: 0xffff, pressed: false, modifiers: 0xff }),
        Rec::Input(Input::Motion { x: 640.5, y: 0.0 }),
        Rec::Input(Input::Button { button: 1, pressed: true }),
        Rec::Input(Input::Button { button: 5, pressed: false }),
        Rec::Input(Input::Scroll { dx: 0.0, dy: -120.0 }),
        Rec::Input(Input::MotionIn { window: 3, x: 10.5, y: 0.0 }),
        Rec::Input(Input::Focus { window: 3 }),
        Rec::Input(Input::Resize { window: 3, width: 900, height: 600 }),
    ]
}

#[test]
fn records_round_trip() {
    let mut all = Vec::new();
    for r in samples() {
        all.extend(r.encode());
    }
    let mut cur = std::io::Cursor::new(all);
    for r in samples() {
        assert_eq!(read(&mut cur).unwrap(), Some(r));
    }
    assert_eq!(read(&mut cur).unwrap(), None);
}

fn header(r: &Rec) -> [u8; HEADER] {
    r.encode()[..HEADER].try_into().unwrap()
}

#[test]
fn bounds_are_checked_before_buffering() {
    let mut h = header(&Rec::Data { window: 1, ticket: 0, bytes: vec![0] });
    h[32..36].copy_from_slice(&((MAX_DATA + 1) as u32).to_le_bytes());
    assert!(Rec::payload_len(&h).is_err());
    let mut h = header(&Rec::Layout { screen: (10, 10), desktop: false, windows: vec![] });
    h[32..36].copy_from_slice(&((MAX_LAYOUT * 20 + 20) as u32).to_le_bytes());
    assert!(Rec::payload_len(&h).is_err());
    let mut h = header(&Rec::Changed { window: 1 });
    h[32..36].copy_from_slice(&1u32.to_le_bytes());
    assert!(Rec::payload_len(&h).is_err());
}

#[test]
fn hostile_records_are_refused() {
    // Window 0, a version without its flag, a flag of 2, reserved bytes,
    // a window placed twice, a zero-sized window, empty data.
    let bad: Vec<Vec<u8>> = vec![
        {
            let mut b = Rec::Changed { window: 1 }.encode();
            b[4..8].copy_from_slice(&0u32.to_le_bytes());
            b
        },
        {
            let mut b = Rec::Want { window: 1, ticket: 0, base: None }.encode();
            b[16] = 1;
            b
        },
        {
            let mut b = Rec::Session { on: true }.encode();
            b[12] = 2;
            b
        },
        {
            let mut b = Rec::Session { on: true }.encode();
            b[38] = 1;
            b
        },
        Rec::Layout { screen: (9, 9), desktop: false, windows: vec![Placed { window: 2, x: 0, y: 0, width: 1, height: 1 }; 2] }.encode(),
        Rec::Layout { screen: (9, 9), desktop: false, windows: vec![Placed { window: 2, x: 0, y: 0, width: 0, height: 1 }] }.encode(),
        Rec::Data { window: 1, ticket: 0, bytes: vec![] }.encode(),
        Rec::Layout { screen: (0, 9), desktop: false, windows: vec![] }.encode(),
        Rec::Output { width: 63, height: 900, scale_milli: 1000 }.encode(),
        Rec::Output { width: 1600, height: 9000, scale_milli: 1000 }.encode(),
        Rec::Output { width: 1600, height: 900, scale_milli: 4001 }.encode(),
        {
            // A chrome whose rect names a colour it does not have.
            let c = fenestra_link::chrome::Chrome::from_rects([0; 3], (9, 9), [([0.0, 0.0, 9.0, 9.0], [0.1, 0.1, 0.1, 1.0], 0.0)]).unwrap();
            let mut b = Rec::Chrome(c).encode();
            let n = b.len();
            b[n - 2] = 3;
            b
        },
        Rec::Input(Input::Motion { x: f32::NAN, y: 0.0 }).encode(),
        Rec::Input(Input::Motion { x: -1.0, y: 0.0 }).encode(),
        Rec::Input(Input::Button { button: 6, pressed: true }).encode(),
        Rec::Input(Input::Scroll { dx: 0.0, dy: 1e9 }).encode(),
        Rec::Input(Input::MotionIn { window: 0, x: 1.0, y: 1.0 }).encode(),
        Rec::Input(Input::MotionIn { window: 3, x: -1.0, y: 1.0 }).encode(),
        Rec::Input(Input::Focus { window: 0 }).encode(),
        Rec::Input(Input::Resize { window: 3, width: 63, height: 600 }).encode(),
        Rec::Input(Input::Resize { window: 3, width: 900, height: 8193 }).encode(),
        {
            // A window on input that is not pointer-in-a-window.
            let mut b = Rec::Input(Input::Key { usage: 4, pressed: true, modifiers: 0 }).encode();
            b[4] = 3;
            b
        },
        {
            // A layout whose mode is neither 0 nor 1.
            let mut b = Rec::Layout { screen: (9, 9), desktop: true, windows: vec![] }.encode();
            b[12] = 2;
            b
        },
        {
            let mut b = Rec::Input(Input::Key { usage: 4, pressed: true, modifiers: 0 }).encode();
            b[8] = 9;
            b
        },
    ];
    for b in bad {
        assert!(read(&mut std::io::Cursor::new(b)).is_err());
    }
}

#[test]
fn fuzzed_headers_decode_canonically_or_not_at_all() {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for r in samples() {
        let good = r.encode();
        for _ in 0..2000 {
            let mut b = good.clone();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = (x as usize) % b.len();
            b[i] ^= (x >> 32) as u8 | 1;
            if let Ok(Some(d)) = read(&mut std::io::Cursor::new(&b)) {
                assert_eq!(d.encode(), b[..d.encode().len()].to_vec(), "{d:?}");
            }
        }
    }
}
