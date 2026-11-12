//! Decoded frames against ffmpeg's reference hashes (scripts/media-test-clips.sh).
use atrium_media_decode::{audio, convert, video, Config};
use atrium_media_demux::{open, probe, Codec, Event, Kind, Limits, Sample, TrackInfo};
use md5::{Digest, Md5};
use std::path::PathBuf;

fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/media").join(name)
}

fn demux(name: &str) -> (Vec<TrackInfo>, Vec<Sample>) {
    let f = std::fs::read(asset(name)).unwrap();
    let mut d = open(probe(&f).unwrap(), Limits::default());
    let (mut tracks, mut samples) = (Vec::new(), Vec::new());
    let mut at = 0usize;
    loop {
        while let Some(ev) = d.poll().unwrap() {
            match ev {
                Event::Tracks(t) => tracks = t,
                Event::Sample(s) => samples.push(s),
                Event::Duration(_) | Event::InitData { .. } => {}
            }
        }
        if let Some(off) = d.wants_offset() {
            at = off as usize;
            d.input_at(off);
            continue;
        }
        if at >= f.len() {
            break;
        }
        d.push(&f[at..]);
        at = f.len();
    }
    (tracks, samples)
}

fn ref_hashes(name: &str) -> Vec<String> {
    std::fs::read_to_string(asset(&format!("{name}.video.framemd5")))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

fn check_video(name: &str, depth: u8) {
    let (tracks, samples) = demux(name);
    let t = tracks.iter().find(|t| t.codec.is_video()).unwrap();
    let mut dec = video(t, &Config::default()).unwrap();
    let mut got = Vec::new();
    let mut pts = Vec::new();
    for s in samples.iter().filter(|s| s.track == t.index) {
        loop {
            let taken = dec.send(&s.data, s.pts_ns).unwrap();
            while let Some(p) = dec.receive().unwrap() {
                assert_eq!((p.width, p.height, p.bit_depth), (160, 90, depth), "{name}");
                got.push(format!("{:x}", Md5::digest(convert::packed_planar(&p))));
                pts.push(p.pts_ns);
            }
            if taken {
                break;
            }
        }
    }
    while let Some(p) = dec.receive().unwrap() {
        got.push(format!("{:x}", Md5::digest(convert::packed_planar(&p))));
        pts.push(p.pts_ns);
    }
    assert_eq!(got, ref_hashes(name), "{name}: frame hashes");
    let want: Vec<_> = samples.iter().filter(|s| s.track == t.index).map(|s| s.pts_ns).collect();
    assert_eq!(pts, want, "{name}: pts carried through");
}

#[test]
fn vp9() {
    check_video("vp9-opus.webm", 8);
}

#[test]
fn vp9_10bit() {
    check_video("vp9-10bit.webm", 10);
}

#[test]
fn vp8() {
    check_video("vp8-vorbis.webm", 8);
}

#[test]
fn av1() {
    check_video("av1-opus.webm", 8);
    check_video("av1-opus.mp4", 8);
}

#[test]
fn vp9_fragmented_mp4() {
    check_video("vp9-opus-frag.mp4", 8);
}

/// 440 Hz in, 440 Hz out: count zero crossings over the middle second.
fn check_audio(name: &str, codec: Codec) {
    let (tracks, samples) = demux(name);
    let t = tracks.iter().find(|t| t.codec == codec).unwrap();
    let mut dec = audio(t).unwrap();
    assert_eq!(dec.channels(), 1);
    let rate = dec.rate() as usize;
    if let Kind::Audio { rate: r, .. } = t.kind {
        assert_eq!(r as usize, rate);
    }
    let mut pcm = Vec::new();
    for s in samples.iter().filter(|s| s.track == t.index) {
        dec.decode(&s.data, &mut pcm).unwrap();
    }
    let secs = pcm.len() as f64 / rate as f64;
    assert!((1.95..=2.1).contains(&secs), "{name}: {secs} s of audio");
    let mid = &pcm[rate / 2..rate / 2 + rate];
    let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    assert!((875..=885).contains(&crossings), "{name}: {crossings} crossings in 1 s");
    let peak = mid.iter().fold(0f32, |a, &x| a.max(x.abs()));
    assert!((0.05..=1.0).contains(&peak), "{name}: peak {peak}");
}

#[test]
fn opus() {
    check_audio("vp9-opus.webm", Codec::Opus);
    check_audio("av1-opus.mp4", Codec::Opus);
}

#[test]
fn vorbis() {
    check_audio("vp8-vorbis.webm", Codec::Vorbis);
}

#[test]
fn nv12_and_p010() {
    for (name, depth) in [("vp9-opus.webm", 8u8), ("vp9-10bit.webm", 10)] {
        let (tracks, samples) = demux(name);
        let t = tracks.iter().find(|t| t.codec.is_video()).unwrap();
        let mut dec = video(t, &Config::default()).unwrap();
        dec.send(&samples.iter().find(|s| s.track == t.index).unwrap().data, 0).unwrap();
        let p = dec.receive().unwrap().unwrap();
        for bps in [1usize, 2] {
            let (ylen, uvlen) = convert::layer_sizes(160, 90, bps);
            let (mut y, mut uv) = (vec![0u8; ylen], vec![0u8; uvlen]);
            {
                let (mut yp, mut uvp) = (convert::PlaneMut { data: &mut y, stride: 160 * bps }, convert::PlaneMut { data: &mut uv, stride: 160 * bps });
                if bps == 1 { convert::to_nv12(&p, &mut yp, &mut uvp) } else { convert::to_p010(&p, &mut yp, &mut uvp) }.unwrap();
            }
            // Luma and the first chroma pair match the source planes.
            let src = |plane: usize, i: usize| -> u32 {
                if depth > 8 { u16::from_le_bytes([p.planes[plane][2 * i], p.planes[plane][2 * i + 1]]) as u32 } else { p.planes[plane][i] as u32 }
            };
            let dst = |b: &[u8], i: usize| -> u32 {
                if bps == 2 { (u16::from_le_bytes([b[2 * i], b[2 * i + 1]]) >> 6) as u32 } else { b[i] as u32 }
            };
            let scale = |v: u32| -> u32 {
                match (depth, bps) {
                    (8, 2) => v << 2,
                    (10, 1) => (v + 2) >> 2,
                    _ => v,
                }
            };
            for i in [0usize, 1, 77, 159] {
                assert_eq!(dst(&y, i), scale(src(0, i)), "{name} bps {bps} luma {i}");
            }
            assert_eq!(dst(&uv, 0), scale(src(1, 0)));
            assert_eq!(dst(&uv, 1), scale(src(2, 0)));
            // Too small a destination is refused.
            let (mut small, mut uv2) = (vec![0u8; ylen - 1], vec![0u8; uvlen]);
            let mut sp = convert::PlaneMut { data: &mut small, stride: 160 * bps };
            let mut uvp = convert::PlaneMut { data: &mut uv2, stride: 160 * bps };
            let r = if bps == 1 { convert::to_nv12(&p, &mut sp, &mut uvp) } else { convert::to_p010(&p, &mut sp, &mut uvp) };
            assert!(r.is_err(), "{name}: short destination accepted");
        }
    }
}

/// Corrupted frames give errors or odd pictures, never a crash.
#[test]
fn corrupt_frames() {
    for name in ["vp9-opus.webm", "av1-opus.webm", "vp8-vorbis.webm", "vp9-10bit.webm"] {
        let (tracks, samples) = demux(name);
        let t = tracks.iter().find(|t| t.codec.is_video()).unwrap();
        for seed in 0..6u32 {
            let mut dec = video(t, &Config { threads: 2, ..Config::default() }).unwrap();
            for (i, s) in samples.iter().filter(|s| s.track == t.index).enumerate() {
                let mut data = s.data.clone();
                let n = data.len();
                for k in 0..(seed as usize + 1) {
                    let at = (i * 131 + k * 977 + seed as usize * 7) % n;
                    data[at] ^= 0x5a ^ (k as u8);
                }
                if dec.send(&data, 0).is_err() {
                    continue;
                }
                while let Ok(Some(_)) = dec.receive() {}
            }
        }
        let a = tracks.iter().find(|t| matches!(t.codec, Codec::Opus | Codec::Vorbis)).map(|t| t.index);
        if let Some(ai) = a {
            let mut dec = audio(&tracks[ai as usize]).unwrap();
            let mut pcm = Vec::new();
            for (i, s) in samples.iter().filter(|s| s.track == ai).enumerate() {
                let mut data = s.data.clone();
                let n = data.len();
                data[(i * 31) % n] ^= 0xff;
                let _ = dec.decode(&data, &mut pcm);
            }
        }
    }
}

/// A size limit below the clip's frame size is refused before decoding.
#[test]
fn size_limit() {
    for name in ["vp9-opus.webm", "av1-opus.webm", "vp8-vorbis.webm"] {
        let (tracks, samples) = demux(name);
        let mut t = tracks.iter().find(|t| t.codec.is_video()).unwrap().clone();
        let cfg = Config { max_width: 100, max_height: 100, ..Config::default() };
        assert!(video(&t, &cfg).is_err(), "{name}: track header over the limit");
        // A track header that lies about its size: the decoder still refuses.
        t.kind = Kind::Video { width: 64, height: 64 };
        let mut dec = video(&t, &cfg).unwrap();
        let s = samples.iter().find(|s| s.track == t.index).unwrap();
        let r = dec.send(&s.data, 0).and_then(|_| dec.receive().map(|p| p.is_some()));
        assert!(r.is_err() || r == Ok(false), "{name}: {r:?}");
    }
}

#[test]
fn mime_types() {
    use atrium_media_decode::mime_supported;
    assert!(mime_supported("video/webm; codecs=\"vp09.00.10.08, opus\""));
    assert!(mime_supported("video/mp4; codecs=\"av01.0.08M.08\""));
    assert!(mime_supported("audio/webm; codecs=vorbis"));
    assert!(!mime_supported("video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\""));
    assert!(!mime_supported("audio/mp4; codecs=\"mp4a.40.2\""));
    assert!(!mime_supported("video/x-matroska"));
}
