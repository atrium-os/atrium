use atrium_message::{Error, Message};
use serde::{Deserialize, Serialize};

#[derive(Message, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[message(version = 3)]
struct Thumb {
    #[message(min = 1, max = 4096)]
    width: u32,
    height: u16,
    delta: i64,
    small: i8,
    ok: bool,
    scale: f32,
    #[message(max_len = 64)]
    pixels: Vec<u8>,
    #[message(max_len = 16, no_control)]
    title: String,
    tags: Option<Vec<String>>,
    #[message(max_len = 4, item_max_len = 8)]
    words: Vec<String>,
    shape: Shape,
    pair: Pair,
    rgb: [u8; 3],
    next: Option<Box<Thumb>>,
}

#[derive(Message, Serialize, Deserialize, Debug, Clone, PartialEq)]
enum Shape {
    Dot,
    Circle(u32),
    Rect { w: u32, #[message(min = -10, max = 10)] h: i32 },
}

#[derive(Message, Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Pair(u64, String);

fn sample() -> Thumb {
    Thumb {
        width: 640, height: 480, delta: -1234567890123, small: -5, ok: true, scale: 1.5,
        pixels: vec![1, 2, 3, 255], title: "photo\tone".into(), tags: Some(vec!["a".into(), "bé".into()]),
        words: vec!["x".into()], shape: Shape::Rect { w: 9, h: -3 }, pair: Pair(u64::MAX, "p".into()),
        rgb: [1, 2, 3],
        next: Some(Box::new(Thumb {
            width: 1, height: 0, delta: 0, small: 0, ok: false, scale: 0.0, pixels: vec![], title: String::new(),
            tags: None, words: vec![], shape: Shape::Dot, pair: Pair(0, String::new()), rgb: [0; 3], next: None,
        })),
    }
}

#[test]
fn it_is_postcard() {
    let t = sample();
    assert_eq!(t.to_bytes(), postcard::to_allocvec(&t).unwrap());
    for s in [Shape::Dot, Shape::Circle(300), Shape::Rect { w: 1, h: 10 }] {
        assert_eq!(s.to_bytes(), postcard::to_allocvec(&s).unwrap());
    }
    for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
        let t = Thumb { delta: v, ..sample() };
        assert_eq!(t.to_bytes(), postcard::to_allocvec(&t).unwrap());
    }
}

#[test]
fn round_trip_and_frames() {
    let t = sample();
    assert_eq!(Thumb::from_bytes(&t.to_bytes()).unwrap(), t);
    let f = t.to_frame();
    assert_eq!(f[0], 3);
    assert_eq!(Thumb::from_frame(&f, 1 << 16).unwrap(), t);
    assert_eq!(Thumb::from_frame(&f, 4), Err(Error::FrameTooLarge { len: f.len(), max: 4 }));
    let mut other = f.clone();
    other[0] = 2;
    assert_eq!(Thumb::from_frame(&other, 1 << 16), Err(Error::Version { got: 2, want: 3 }));
}

#[test]
fn what_decoding_refuses() {
    let ok = sample();
    let bad = |t: Thumb| Thumb::from_bytes(&t.to_bytes()).unwrap_err();
    assert_eq!(bad(Thumb { width: 0, ..ok.clone() }), Error::OutOfRange { field: "Thumb.width" });
    assert_eq!(bad(Thumb { width: 4097, ..ok.clone() }), Error::OutOfRange { field: "Thumb.width" });
    assert_eq!(bad(Thumb { pixels: vec![0; 65], ..ok.clone() }), Error::TooLong { field: "Thumb.pixels", len: 65, max: 64 });
    assert_eq!(bad(Thumb { title: "x".repeat(17), ..ok.clone() }), Error::TooLong { field: "Thumb.title", len: 17, max: 16 });
    assert_eq!(bad(Thumb { title: "a\nb".into(), ..ok.clone() }), Error::ControlChar { field: "Thumb.title" });
    assert_eq!(bad(Thumb { title: "a\u{85}".into(), ..ok.clone() }), Error::ControlChar { field: "Thumb.title" });
    assert_eq!(bad(Thumb { words: vec!["123456789".into()], ..ok.clone() }), Error::TooLong { field: "Thumb.words", len: 9, max: 8 });
    assert_eq!(bad(Thumb { words: vec![String::new(); 5], ..ok.clone() }), Error::TooLong { field: "Thumb.words", len: 5, max: 4 });
    assert_eq!(bad(Thumb { shape: Shape::Rect { w: 0, h: 11 }, ..ok.clone() }), Error::OutOfRange { field: "Shape::Rect.h" });
    // Default bound on unannotated strings and sequences.
    assert!(matches!(bad(Thumb { tags: Some(vec![String::new(); 65_537]), ..ok.clone() }), Error::TooLong { max: 65_536, .. }));

    let b = ok.to_bytes();
    assert_eq!(Thumb::from_bytes(&b[..b.len() - 1]), Err(Error::Truncated));
    let mut more = b.clone();
    more.push(0);
    assert_eq!(Thumb::from_bytes(&more), Err(Error::TrailingBytes));
    // A string that isn't UTF-8: title is after fixed fields; find it.
    let t = Thumb { title: "zz".into(), ..ok.clone() };
    let mut b = t.to_bytes();
    let at = b.windows(3).position(|w| w == [2, b'z', b'z']).unwrap();
    b[at + 1] = 0xff;
    assert_eq!(Thumb::from_bytes(&b), Err(Error::BadUtf8 { field: "Thumb.title" }));
    // An unknown variant.
    assert_eq!(Shape::from_bytes(&[3]), Err(Error::BadVariant { ty: "Shape", index: 3 }));
    // A bool that isn't 0 or 1; an option tag that isn't.
    let t = Thumb { ok: true, small: 0x55, ..ok.clone() };
    let mut b = t.to_bytes();
    let at = b.iter().position(|&x| x == 0x55).unwrap();
    b[at + 1] = 2;
    assert_eq!(Thumb::from_bytes(&b), Err(Error::BadTag));
}

#[test]
fn varints_are_strict() {
    #[derive(Message, Debug, PartialEq)]
    struct N(u16, u32);
    assert_eq!(N::from_bytes(&[0xff, 0xff, 0x03, 0]).unwrap(), N(65535, 0));
    assert_eq!(N::from_bytes(&[0xff, 0xff, 0x04, 0]), Err(Error::BadVarint)); // 2^16
    assert_eq!(N::from_bytes(&[0x80, 0x80, 0x80, 0]), Err(Error::BadVarint)); // too long
    assert_eq!(N::from_bytes(&[0, 0xff, 0xff, 0xff, 0xff, 0x0f]).unwrap(), N(0, u32::MAX));
    assert_eq!(N::from_bytes(&[0, 0xff, 0xff, 0xff, 0xff, 0x10]), Err(Error::BadVarint));
}

#[test]
fn hostile_lengths_allocate_nothing_big() {
    // A count of 2^60 items: refused by the bound before any allocation.
    #[derive(Message, Debug)]
    struct Big(#[message(max_len = u64::MAX)] Vec<u64>);
    let mut b = vec![];
    atrium_message::put_varint(&mut b, 1 << 60);
    assert_eq!(Big::from_bytes(&b).unwrap_err(), Error::Truncated);
}

#[test]
fn nesting_is_bounded() {
    #[derive(Message, Debug)]
    struct Node(Option<Box<Node>>);
    let mut b = vec![1u8; 100];
    b.push(0);
    assert_eq!(Node::from_bytes(&b).unwrap_err(), Error::TooDeep);
    let mut b = vec![1u8; 60];
    b.push(0);
    assert!(Node::from_bytes(&b).is_ok());
}

#[test]
fn schema_names_everything() {
    let s = Thumb::schema_json();
    for want in ["\"root\": \"Thumb\"", "\"version\": 3", "\"name\": \"Shape\", \"kind\": \"enum\"", "\"name\": \"Pair\"",
                 "{\"prim\": \"u32\", \"min\": 1, \"max\": 4096}", "{\"vec\": {\"prim\": \"u8\"}, \"max_len\": 64}",
                 "{\"string\": {\"max_len\": 16, \"no_control\": true}}", "{\"option\": {\"ref\": \"Thumb\"}}",
                 "{\"array\": {\"prim\": \"u8\"}, \"len\": 3}"] {
        assert!(s.contains(want), "{want} in {s}");
    }
    // Thumb once, though it nests itself.
    assert_eq!(s.matches("\"name\": \"Thumb\"").count(), 1);
}
