//! The resampler (src/resample.rs) on its own: the size rule, the
//! arithmetic, and DETERMINISM pinned to a digest — a scene that names
//! decoded pixels must get the same pixels on every machine.

mod common;
use common::images::pattern;
use navigator_backend::resample::{fit, resample, Filter};

#[test]
fn fit_takes_one_scale_from_the_axis_that_asks_for_more() {
    assert_eq!(fit((1000, 500), (100, 50)), (100, 50));
    assert_eq!(fit((1000, 500), (100, 10)), (100, 50));   // width asks more
    assert_eq!(fit((1000, 500), (10, 100)), (200, 100));  // height asks more
    assert_eq!(fit((1000, 500), (2000, 10)), (1000, 500)); // never up
    assert_eq!(fit((3, 3), (1, 1)), (1, 1));
    assert_eq!(fit((7, 3), (2, 1)), (3, 1));               // 1/3 > 2/7: height sets it, 7/3 → 3
    // ★ Whatever the box, one axis is at most the target (the broker's check).
    for (n, t) in [((640, 480), (33, 900)), ((17, 1), (3, 3)), ((1, 17), (3, 3)), ((4096, 4096), (1, 4096))] {
        let (w, h) = fit(n, t);
        assert!(w <= t.0 || h <= t.1, "{n:?} at {t:?} gave {w}x{h}");
        assert!(w <= n.0 && h <= n.1 && w >= 1 && h >= 1);
    }
}

#[test]
fn a_flat_field_stays_flat_under_every_filter() {
    let src: Vec<u8> = std::iter::repeat([12u8, 200, 77, 255]).take(37 * 23).flatten().collect();
    for f in Filter::ALL {
        let out = resample(&src, 37, 23, 10, 7, f);
        assert!(out.chunks_exact(4).all(|p| p == [12, 200, 77, 255]), "{}: {:?}", f.name(), &out[..8]);
    }
}

#[test]
fn a_fully_transparent_image_stays_transparent() {
    let src: Vec<u8> = std::iter::repeat([255u8, 0, 0, 0]).take(9 * 9).flatten().collect();
    assert!(resample(&src, 9, 9, 3, 3, Filter::DEFAULT).iter().all(|&v| v == 0));
}

/// The area average at a non-integer ratio: 3 → 2, each output pixel
/// covers one and a half source pixels.
#[test]
fn the_area_average_weights_partial_coverage() {
    let src = [0u8, 0, 0, 255, 90, 90, 90, 255, 180, 180, 180, 255];
    let out = resample(&src, 3, 1, 2, 1, Filter::Area);
    // (0·1 + 90·0.5) / 1.5 = 30 ; (90·0.5 + 180·1) / 1.5 = 150
    assert_eq!(out, [30, 30, 30, 255, 150, 150, 150, 255]);
}

/// ★★ The digest of each deterministic filter's output on a fixed input.
/// If this moves, every decoded image moves: it must move on purpose.
#[test]
fn the_output_is_pinned() {
    let src = pattern(97, 61);
    let digest = |f: Filter| blake3::hash(&resample(&src, 97, 61, 23, 17, f)).to_hex().to_string();
    let got: Vec<(String, String)> = [Filter::Area, Filter::Triangle, Filter::CatmullRom, Filter::Mitchell]
        .iter().map(|&f| (f.name().to_string(), digest(f)[..16].to_string())).collect();
    eprintln!("{got:?}");
    assert_eq!(got, PINNED.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>());
}

const PINNED: [(&str, &str); 4] = [("area", "8d6ca4f4fa9e6073"), ("triangle", "6e0f4404cfcd4f6d"), ("catmull-rom", "37c24cff1def971a"), ("mitchell", "f770767fc8f88859")];
