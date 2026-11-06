//! The demuxers against ffmpeg-made clips (scripts/media-test-clips.sh).
use atrium_media_demux::{open, probe, Codec, Demuxer, Event, Kind, Limits, Sample, TrackInfo};
use std::path::PathBuf;

fn clip(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/media").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Frames in ffmpeg's reference hashes.
fn ref_frames(name: &str) -> usize {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/media").join(name);
    std::fs::read_to_string(p).unwrap().lines().filter(|l| !l.starts_with('#')).count()
}

/// (pts ns, key) per video packet, from ffprobe.
fn ref_packets(name: &str) -> Vec<(i64, bool)> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/media").join(name);
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| {
            let (t, f) = l.split_once(',').unwrap();
            ((t.parse::<f64>().unwrap() * 1e9).round() as i64, f.starts_with('K'))
        })
        .collect()
}

#[derive(Default)]
struct Out {
    tracks: Vec<TrackInfo>,
    duration: Option<f64>,
    samples: Vec<Sample>,
    /// Offsets the parser asked for.
    jumps: Vec<u64>,
}

/// Play a whole file through a parser, in chunks of `chunk` bytes, serving
/// its seeks like a file descriptor would.
fn run(file: &[u8], chunk: usize, d: &mut dyn Demuxer) -> Out {
    let mut out = Out::default();
    let mut at = 0usize;
    loop {
        loop {
            match d.poll().expect("parses") {
                Some(Event::Tracks(t)) => out.tracks = t,
                Some(Event::Duration(x)) => out.duration = Some(x),
                Some(Event::Sample(s)) => out.samples.push(s),
                None => break,
            }
        }
        if let Some(off) = d.wants_offset() {
            out.jumps.push(off);
            at = off as usize;
            d.input_at(off);
            continue;
        }
        if at >= file.len() {
            d.finish().expect("ends cleanly");
            // A parser that went ahead for an index may come back.
            if d.wants_offset().is_some() {
                continue;
            }
            return out;
        }
        let end = (at + chunk).min(file.len());
        d.push(&file[at..end]);
        at = end;
    }
}

fn play(name: &str, chunk: usize) -> Out {
    let f = clip(name);
    let fmt = probe(&f).expect("probes");
    let mut d = open(fmt, Limits::default());
    run(&f, chunk, d.as_mut())
}

fn check(name: &str, video: Codec, audio: Option<Codec>) {
    let o = play(name, 1 << 20);
    let v = o.tracks.iter().find(|t| t.codec.is_video()).expect("video track");
    assert_eq!(v.codec, video, "{name}");
    assert_eq!(v.kind, Kind::Video { width: 160, height: 90 });
    if let Some(a) = audio {
        let at = o.tracks.iter().find(|t| matches!(t.kind, Kind::Audio { .. })).expect("audio track");
        assert_eq!(at.codec, a, "{name}");
        assert_eq!(at.kind, Kind::Audio { rate: 48000, channels: 1 });
        if a == Codec::Opus {
            assert_eq!(&at.config[..8], b"OpusHead");
        }
        let n = o.samples.iter().filter(|s| s.track == at.index).count();
        assert!(n > 50, "{name}: {n} audio packets");
    }
    let vs: Vec<_> = o.samples.iter().filter(|s| s.track == v.index).collect();
    
    assert_eq!(vs.len(), ref_frames(&format!("{name}.video.framemd5")), "{name}: video frames");
    // Timestamps and key flags as ffprobe reads them from the container.
    let refs = ref_packets(&format!("{name}.video.packets"));
    assert_eq!(refs.len(), vs.len());
    for (i, (s, (t, k))) in vs.iter().zip(&refs).enumerate() {
        assert!((s.pts_ns - t).abs() <= 1_000, "{name}: frame {i} at {} ns, ffprobe says {t}", s.pts_ns);
        assert_eq!(s.key, *k, "{name}: frame {i} key flag");
    }
    assert!(vs[0].key && vs[24].key, "{name}: keyframes every 24 (-g 24)");
    if let Some(d) = o.duration {
        assert!((1.9..=2.2).contains(&d), "{name}: duration {d}");
    }
}

#[test]
fn webm_vp9_opus() {
    check("vp9-opus.webm", Codec::Vp9, Some(Codec::Opus));
}

#[test]
fn webm_av1_opus() {
    check("av1-opus.webm", Codec::Av1, Some(Codec::Opus));
}

#[test]
fn webm_vp8_vorbis() {
    check("vp8-vorbis.webm", Codec::Vp8, Some(Codec::Vorbis));
    let o = play("vp8-vorbis.webm", 1 << 20);
    let a = o.tracks.iter().find(|t| t.codec == Codec::Vorbis).unwrap();
    // Three length-prefixed headers: identification, comment, setup.
    let c = &a.config;
    let l0 = u32::from_le_bytes(c[0..4].try_into().unwrap()) as usize;
    assert_eq!(&c[4..11], b"\x01vorbis");
    assert_eq!(c[4 + l0 + 4], 3);
}

#[test]
fn webm_vp9_10bit() {
    check("vp9-10bit.webm", Codec::Vp9, None);
}

#[test]
fn mp4_plain_moov_last() {
    check("av1-opus.mp4", Codec::Av1, Some(Codec::Opus));
}

#[test]
fn mp4_fragmented() {
    check("vp9-opus-frag.mp4", Codec::Vp9, Some(Codec::Opus));
}

/// Byte-at-a-time and odd-sized appends give the same samples as one push.
#[test]
fn chunking_does_not_matter() {
    for name in ["vp9-opus.webm", "av1-opus.webm", "vp9-opus-frag.mp4", "av1-opus.mp4"] {
        let whole = play(name, usize::MAX / 2);
        for chunk in [1, 7, 4096] {
            let o = play(name, chunk);
            assert_eq!(o.samples, whole.samples, "{name} in chunks of {chunk}");
            assert_eq!(o.tracks, whole.tracks);
        }
    }
}

/// Seeking with cues (WebM) and the sample table (plain MP4).
#[test]
fn seeks_land_on_a_keyframe_before_the_target() {
    for name in ["vp9-opus.webm", "av1-opus.mp4"] {
        let f = clip(name);
        let mut d = open(probe(&f).unwrap(), Limits::default());
        let all = run(&f, 1 << 20, d.as_mut());
        let vt = all.tracks.iter().find(|t| t.codec.is_video()).unwrap().index;
        assert!(d.seek(1_500_000_000), "{name}: seek");
        let off = d.wants_offset().expect("asks for an offset") as usize;
        d.input_at(off as u64);
        d.push(&f[off..]);
        let mut first = None;
        while let Some(ev) = d.poll().unwrap() {
            if let Event::Sample(s) = ev {
                if s.track == vt {
                    first.get_or_insert(s);
                }
            }
        }
        let s = first.expect("samples after seek");
        assert!(s.key, "{name}: first sample after seek is key");
        assert!((s.pts_ns - 1_000_000_000).abs() <= 1_000, "{name}: landed at {}", s.pts_ns);
    }
}

#[test]
fn limits_are_errors() {
    let f = clip("vp9-opus.webm");
    let lim = Limits { max_frame: 1000, ..Limits::default() };
    let mut d = open(probe(&f).unwrap(), lim);
    d.push(&f);
    let mut err = None;
    for _ in 0..10_000 {
        match d.poll() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(matches!(err, Some(atrium_media_demux::Error::TooBig(_))), "{err:?}");
    // Sticky: the parser stays failed.
    assert!(d.poll().is_err());

    let lim = Limits { max_width: 100, ..Limits::default() };
    for name in ["vp9-opus.webm", "vp9-opus-frag.mp4"] {
        let f = clip(name);
        let mut d = open(probe(&f).unwrap(), lim);
        d.push(&f);
        let r = (0..100).map(|_| d.poll()).find(|r| !matches!(r, Ok(Some(_))));
        assert!(matches!(r, Some(Err(atrium_media_demux::Error::TooBig(_)))), "{name}: {r:?}");
    }
}

/// Every truncation and a sweep of single-byte corruptions parse to an
/// error or to fewer samples, never a panic or a hang.
#[test]
fn corrupt_inputs_never_panic() {
    for name in ["vp9-opus.webm", "vp9-opus-frag.mp4", "av1-opus.mp4", "vp8-vorbis.webm"] {
        let f = clip(name);
        let fmt = probe(&f).unwrap();
        for cut in (0..f.len()).step_by(97) {
            let mut d = open(fmt, Limits::default());
            d.push(&f[..cut]);
            drain(d.as_mut());
            let _ = d.finish();
        }
        for i in (0..f.len().min(4096)).step_by(3) {
            for x in [0x00u8, 0xFF, 0x80, 0x01] {
                let mut g = f.clone();
                g[i] = x;
                let mut d = open(fmt, Limits::default());
                d.push(&g);
                drain(d.as_mut());
            }
        }
    }
}

fn drain(d: &mut dyn Demuxer) {
    for _ in 0..1_000_000 {
        match d.poll() {
            Ok(Some(_)) => {}
            _ => return,
        }
    }
    panic!("parser made no end");
}

/// Cues after the clusters (where ffmpeg puts them): a seekable input reads
/// them through the SeekHead before the first cluster, so a seek works at
/// once, and nothing else changes.
#[test]
fn webm_cues_at_the_end_are_read_first() {
    for name in ["vp9-opus.webm", "av1-opus.webm", "vp8-vorbis.webm"] {
        let f = clip(name);
        let plain = run(&f, 1 << 16, open(probe(&f).unwrap(), Limits::default()).as_mut());
        let mut d = open(probe(&f).unwrap(), Limits::default());
        d.set_seekable(true);
        // Up to the first sample: the parser asks for the Cues on the way.
        d.push(&f[..4096.min(f.len())]);
        let mut asked = None;
        let mut at = 4096usize;
        let mut first = None;
        while first.is_none() {
            while let Some(ev) = d.poll().unwrap() {
                if let Event::Sample(s) = ev {
                    first.get_or_insert(s);
                }
            }
            if let Some(off) = d.wants_offset() {
                asked.get_or_insert(off);
                d.input_at(off);
                at = off as usize;
            } else {
                d.push(&f[at..(at + 4096).min(f.len())]);
                at = (at + 4096).min(f.len());
            }
        }
        let cues_at = asked.expect("asks for the cues") as usize;
        assert!(cues_at > f.len() / 2, "{name}: the cues are at the end ({cues_at} of {})", f.len());
        assert!(d.seek(1_500_000_000), "{name}: can seek before reading on");
        // The whole file the seekable way: the same samples.
        let mut d = open(probe(&f).unwrap(), Limits::default());
        d.set_seekable(true);
        let seekable = run(&f, 1 << 16, d.as_mut());
        assert_eq!(seekable.samples.len(), plain.samples.len(), "{name}");
        assert!(seekable.samples.iter().zip(&plain.samples).all(|(a, b)| a.pts_ns == b.pts_ns && a.data == b.data), "{name}: same samples");
    }
}

/// A SeekHead pointing at nonsense (past the end, or not at Cues) is
/// ignored: the file plays the same.
#[test]
fn a_wrong_seekhead_is_ignored() {
    let f = clip("vp9-opus.webm");
    let plain = run(&f, 1 << 16, open(probe(&f).unwrap(), Limits::default()).as_mut());
    // Find the SeekHead's entry for the Cues and point it elsewhere.
    let id = [0x53, 0xAB, 0x84, 0x1C, 0x53, 0xBB, 0x6B];
    let i = f.windows(id.len()).position(|w| w == id).expect("a SeekHead entry for the Cues");
    let p = i + id.len();
    assert_eq!(&f[p..p + 2], &[0x53, 0xAC], "SeekPosition follows");
    let len = (f[p + 2] & 0x0F) as usize;
    let max = (1u64 << (8 * len)) - 1;
    // Past the end, mid-cluster, and the first byte of a cluster's payload.
    for bad in [max, (f.len() as u64 * 3 / 5).min(max), (f.len() as u64 / 3).min(max)] {
        let mut g = f.clone();
        for k in 0..len {
            g[p + 3 + k] = (bad >> (8 * (len - 1 - k))) as u8;
        }
        let mut d = open(probe(&g).unwrap(), Limits::default());
        d.set_seekable(true);
        let o = run(&g, 1 << 16, d.as_mut());
        assert_eq!(o.samples.len(), plain.samples.len(), "SeekPosition {bad:#x}");
        if bad != max {
            assert_eq!(o.jumps.len(), 2, "SeekPosition {bad:#x}: went there and came back ({:?})", o.jumps);
        }
    }
}

#[test]
fn colour_descriptions_are_read() {
    let o = play("color-hdr10.webm", 1 << 16);
    let c = o.tracks[0].color.expect("a Colour element");
    assert_eq!((c.primaries, c.transfer, c.matrix, c.range), (9, 16, 9, 1), "BT.2020, PQ, BT.2020 NCL, limited");
    let o = play("color-full.mp4", 1 << 16);
    let c = o.tracks[0].color.expect("a colr box");
    assert_eq!((c.primaries, c.transfer, c.matrix, c.range), (1, 1, 1, 2), "BT.709, full range");
}
