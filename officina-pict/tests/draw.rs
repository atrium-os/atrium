//! Small metafiles built here, drawn, and their pixels checked.

fn wmf(placeable: Option<(i16, i16, i16, i16)>, records: &[(u16, Vec<i16>)]) -> Vec<u8> {
    let mut b = Vec::new();
    if let Some((l, t, r, bo)) = placeable {
        b.extend(0x9AC6_CDD7u32.to_le_bytes());
        b.extend([0, 0]);
        for v in [l, t, r, bo] {
            b.extend(v.to_le_bytes());
        }
        b.extend(1440u16.to_le_bytes());
        b.extend([0; 4]);
        b.extend([0, 0]);
    }
    b.extend([1, 0, 9, 0, 0, 3]);
    b.extend([0; 4]);
    b.extend(8u16.to_le_bytes());
    b.extend([0; 6]);
    for (f, p) in records {
        b.extend((3 + p.len() as u32).to_le_bytes());
        b.extend(f.to_le_bytes());
        for v in p {
            b.extend(v.to_le_bytes());
        }
    }
    b.extend([3, 0, 0, 0, 0, 0]);
    b
}

fn px(p: &officina_pict::Rgba, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * p.width + x) * 4) as usize;
    p.data[i..i + 4].try_into().unwrap()
}

/// A COLORREF as two words.
fn color(c: u32) -> [i16; 2] {
    [c as u16 as i16, (c >> 16) as u16 as i16]
}

#[test]
fn a_wmf_rectangle_fills_its_window() {
    let red = color(0x0000FF);
    let recs = vec![
        (0x020C, vec![100, 200]),            // window 200 × 100
        (0x02FC, vec![0, red[0], red[1], 0]), // red brush
        (0x012D, vec![0]),
        (0x02FA, vec![5, 0, 0, 0, 0]), // null pen
        (0x012D, vec![1]),
        (0x041B, vec![100, 100, 0, 0]), // left half
    ];
    let b = wmf(None, &recs);
    assert_eq!(officina_pict::natural_size(&b).map(|s| (s.1, s.2)), Some((200, 100)));
    let p = officina_pict::render(&b, 400, 200).unwrap();
    assert_eq!(px(&p, 50, 100), [255, 0, 0, 255]);
    assert_eq!(px(&p, 350, 100)[3], 0, "the right half is left clear");
}

#[test]
fn wmf_text_draws_in_the_text_colour() {
    let blue = color(0xFF0000);
    let mut face = vec![-40i16, 0, 0, 0, 700, 0, 0, 0];
    face.extend([i16::from_le_bytes(*b"Ar"), i16::from_le_bytes(*b"ia"), i16::from_le_bytes(*b"l\0")]);
    face.resize(25, 0);
    let recs = vec![
        (0x020C, vec![100, 200]),
        (0x02FB, face),
        (0x012D, vec![0]),
        (0x0209, blue.to_vec()),
        (0x0102, vec![1]),
        (0x0521, vec![2, i16::from_le_bytes(*b"HH"), 60, 20]), // "HH" at (20, 60)
    ];
    let p = officina_pict::render(&wmf(None, &recs), 200, 100).unwrap();
    let inked: Vec<[u8; 4]> = p.data.chunks(4).filter(|c| c[3] > 200).map(|c| c.try_into().unwrap()).collect();
    assert!(inked.len() > 200, "the letters are drawn");
    assert!(inked.iter().all(|c| c[2] > 200 && c[0] < 60), "in blue");
}

#[test]
fn damage_draws_nothing_and_never_panics() {
    let good = wmf(Some((0, 0, 100, 100)), &[(0x041B, vec![50, 50, 0, 0])]);
    for cut in 0..good.len() {
        let _ = officina_pict::render(&good[..cut], 64, 64);
    }
    assert!(officina_pict::render(b"not a picture", 64, 64).is_none());
    assert!(officina_pict::render(&good, 0, 64).is_none());
    assert!(officina_pict::render(&good, 100_000, 100_000).is_none());
}

/// An EMF: header, then records of (type, u32 words).
fn emf(records: &[(u32, Vec<u32>)]) -> Vec<u8> {
    let mut h = vec![0u32; 22];
    h[0] = 1;
    h[1] = 88;
    // bounds 0,0 - 99,99; frame 0,0 - 2645,2645 (100 px at 96 dpi)
    h[4] = 99;
    h[5] = 99;
    h[8] = 2645;
    h[9] = 2645;
    h[10] = u32::from_le_bytes(*b" EMF");
    h[11] = 0x10000;
    h[18] = 960; // device px over 254 mm: 96 dpi
    h[19] = h[18];
    h[20] = 254;
    h[21] = 254;
    let mut b: Vec<u8> = h.iter().flat_map(|v| v.to_le_bytes()).collect();
    for (t, p) in records.iter().chain(std::iter::once(&(14, vec![0, 0, 0]))) {
        b.extend(t.to_le_bytes());
        b.extend((8 + 4 * p.len() as u32).to_le_bytes());
        for v in p {
            b.extend(v.to_le_bytes());
        }
    }
    let len = b.len() as u32;
    b[48..52].copy_from_slice(&len.to_le_bytes());
    b
}

#[test]
fn office_xor_clipping_leaves_the_fill_inside_the_shape() {
    let poly = |pts: &[(u32, u32)]| {
        let mut v = vec![0, 0, 0, 0, pts.len() as u32];
        for &(x, y) in pts {
            v.extend([x, y]);
        }
        v
    };
    let fill = poly(&[(0, 0), (100, 0), (100, 100), (0, 100)]);
    let recs = vec![
        (39, vec![1, 0, 0x00FF00, 0]), // green brush
        (37, vec![1]),
        (37, vec![0x8000_0008]), // null pen
        (20, vec![7]),           // XOR
        (3, fill.clone()),
        (20, vec![13]),
        (37, vec![0x8000_0004]), // black brush
        (3, poly(&[(0, 0), (50, 0), (50, 100), (0, 100)])),
        (20, vec![7]),
        (37, vec![1]),
        (3, fill),
        (20, vec![13]),
    ];
    let b = emf(&recs);
    assert_eq!(officina_pict::sniff(&b), Some(officina_pict::Kind::Emf));
    let p = officina_pict::render(&b, 100, 100).unwrap();
    assert_eq!(px(&p, 25, 50), [0, 255, 0, 255], "inside the shape: the fill");
    assert_eq!(px(&p, 75, 50), [0; 4], "outside: nothing");
}
