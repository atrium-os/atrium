//! WebM / Matroska (EBML). Segment and Cluster are *entered* (they may be
//! huge or of unknown size, as in live streams); every other element is
//! either buffered whole, under [`Limits::max_buffered`], and parsed, or
//! skipped by size.
use std::collections::{HashMap, VecDeque};

use crate::rd::{be_float, be_uint, Rd};
use crate::{to_ns, Codec, Demuxer, Error, Event, Input, Kind, Limits, Result, Sample, TrackInfo};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const CLUSTER: u32 = 0x1F43_B675;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CUES: u32 = 0x1C53_BB6B;
const SEEKHEAD: u32 = 0x114D_9B74;
const TIMECODE: u32 = 0xE7;
const SIMPLEBLOCK: u32 = 0xA3;
const BLOCKGROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCKDURATION: u32 = 0x9B;
const REFERENCEBLOCK: u32 = 0xFB;

/// Elements that may sit directly in a Cluster. Anything else ends an
/// unknown-size Cluster.
fn cluster_child(id: u32) -> bool {
    matches!(id, TIMECODE | SIMPLEBLOCK | BLOCKGROUP | 0xA7 | 0xAB | 0xEC | 0xBF | 0x5854 | 0xAF)
}

/// Segment children the parser reads; the rest are skipped by size.
fn parsed_in_segment(id: u32) -> bool {
    matches!(id, INFO | TRACKS | CUES | SEEKHEAD)
}

#[derive(Debug, Clone, Copy)]
struct Open {
    id: u32,
    /// Absolute end, or `None` for unknown size.
    end: Option<u64>,
}

pub struct Parser {
    lim: Limits,
    inp: Input,
    stack: Vec<Open>,
    /// Bytes still to skip (an element we don't read).
    skip: u64,
    header_seen: bool,
    seg_data: Option<u64>,
    /// The Segment, kept after it closes so seeks can re-enter it.
    segment: Option<Open>,
    timescale: u64,
    tracks: Vec<TrackInfo>,
    by_number: HashMap<u64, u8>,
    /// Each track's CodecDelay (ns): block times include it, media time
    /// doesn't (Opus pre-skip).
    delay: Vec<i64>,
    cluster_tc: Option<u64>,
    /// (time ns, absolute cluster offset), sorted by time.
    cues: Vec<(i64, u64)>,
    /// Where the SeekHead says the Cues are (absolute), and whether the
    /// input can be read out of order to fetch them from there.
    cues_at: Option<u64>,
    seekable: bool,
    /// Gone to read the Cues: the first Cluster's offset, to come back to.
    resume: Option<u64>,
    want: Option<u64>,
    out: VecDeque<Event>,
    /// The first error is sticky: the parser stops for good.
    failed: Option<Error>,
}

impl Parser {
    pub fn new(lim: Limits) -> Self {
        Parser {
            lim,
            inp: Input::new(),
            stack: Vec::new(),
            skip: 0,
            header_seen: false,
            seg_data: None,
            segment: None,
            timescale: 1_000_000,
            tracks: Vec::new(),
            by_number: HashMap::new(),
            delay: Vec::new(),
            cluster_tc: None,
            cues: Vec::new(),
            cues_at: None,
            seekable: false,
            resume: None,
            want: None,
            out: VecDeque::new(),
            failed: None,
        }
    }

    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    /// One unit of progress; `Ok(false)` when more bytes are needed.
    fn step(&mut self) -> Result<bool> {
        if self.skip > 0 {
            let n = (self.skip.min(self.inp.avail().len() as u64)) as usize;
            if n == 0 {
                return Ok(false);
            }
            self.inp.consume(n);
            self.skip -= n as u64;
            return Ok(true);
        }
        let pos = self.inp.pos();
        // Close masters we've reached the end of.
        while let Some(top) = self.stack.last() {
            match top.end {
                Some(e) if pos >= e => {
                    if pos > e {
                        return Err(Error::Malformed("child overruns its master"));
                    }
                    if top.id == CLUSTER {
                        self.cluster_tc = None;
                    }
                    self.stack.pop();
                }
                _ => break,
            }
        }
        let Some((id, size, hlen)) = header(self.inp.avail())? else { return Ok(false) };

        if !self.header_seen {
            if id != EBML {
                return Err(Error::NotThisFormat);
            }
            let size = size.ok_or(Error::Malformed("EBML header size"))?;
            if size > 4096 {
                return Err(Error::TooBig("EBML header"));
            }
            let Some(body) = self.whole(hlen, size) else { return Ok(false) };
            check_ebml_header(&body)?;
            self.inp.consume(hlen + size as usize);
            self.header_seen = true;
            return Ok(true);
        }

        let top = self.stack.last().copied();
        // Gone to read the Cues: anything else there means the SeekHead was
        // wrong, so go back and play without them.
        if let (Some(r), Some(SEGMENT)) = (self.resume, top.map(|t| t.id)) {
            if id != CUES {
                self.resume = None;
                self.want = Some(r);
                return Ok(true);
            }
        }
        // An unknown-size Cluster ends at the first element that can't be in it.
        if let Some(Open { id: CLUSTER, end: None }) = top {
            if !cluster_child(id) {
                self.stack.pop();
                self.cluster_tc = None;
                return Ok(true);
            }
        }
        let end = size.map(|s| pos + hlen as u64 + s);
        if let (Some(e), Some(Open { end: Some(pe), .. })) = (end, top) {
            if e > pe {
                return Err(Error::Malformed("element overruns its master"));
            }
        }
        match (top.map(|t| t.id), id) {
            (None, SEGMENT) => {
                self.inp.consume(hlen);
                self.seg_data = Some(pos + hlen as u64);
                self.stack.push(Open { id: SEGMENT, end });
                self.segment = Some(Open { id: SEGMENT, end });
                return Ok(true);
            }
            // A new init segment (MSE), or a chained segment in a file.
            (None, EBML) | (Some(SEGMENT), EBML) => {
                self.stack.clear();
                self.header_seen = false;
                self.cluster_tc = None;
                return Ok(true);
            }
            (None, 0xEC) => {}
            // Past the Segment's stated end: a media segment appended later
            // (MSE ignores the Segment's size), or a seek back into it.
            (None, CLUSTER | CUES) if self.segment.is_some() => {
                self.stack.push(Open { id: SEGMENT, end: None });
                return Ok(true);
            }
            (None, _) => return Err(Error::Malformed("top-level element")),
            (Some(SEGMENT), CLUSTER) => {
                // Cues after the clusters (as most muxers write them): read
                // them first if the input allows, then come back here.
                if self.seekable && self.cues.is_empty() && self.resume.is_none() {
                    if let Some(at) = self.cues_at.take() {
                        if at > pos && self.segment.and_then(|s| s.end).map_or(true, |e| at < e) {
                            self.resume = Some(pos);
                            self.want = Some(at);
                            return Ok(true);
                        }
                    }
                }
                self.inp.consume(hlen);
                self.cluster_tc = None;
                self.stack.push(Open { id: CLUSTER, end });
                return Ok(true);
            }
            _ => {}
        }
        let Some(size) = size else { return Err(Error::Malformed("unknown size")) };
        let in_cluster = top.map(|t| t.id) == Some(CLUSTER);
        let parsed = if in_cluster { matches!(id, TIMECODE | SIMPLEBLOCK | BLOCKGROUP) } else { parsed_in_segment(id) };
        if !parsed {
            self.inp.consume(hlen);
            self.skip = size;
            return Ok(true);
        }
        let cap = if in_cluster { self.lim.max_frame as u64 + 64 } else { self.lim.max_buffered as u64 };
        if size > cap {
            return Err(Error::TooBig("element"));
        }
        let Some(body) = self.whole(hlen, size) else { return Ok(false) };
        self.inp.consume(hlen + size as usize);
        match id {
            INFO => self.info(&body)?,
            TRACKS => self.parse_tracks(&body)?,
            CUES => {
                self.parse_cues(&body)?;
                if let Some(r) = self.resume.take() {
                    self.want = Some(r);
                }
            }
            SEEKHEAD => self.parse_seekhead(&body)?,
            TIMECODE => self.cluster_tc = Some(be_uint(&body)?),
            SIMPLEBLOCK => self.block(&body, None, None)?,
            BLOCKGROUP => self.block_group(&body)?,
            _ => {}
        }
        Ok(true)
    }

    /// The element's body if it's all buffered.
    fn whole(&self, hlen: usize, size: u64) -> Option<Vec<u8>> {
        let a = self.inp.avail();
        let need = hlen as u64 + size;
        (a.len() as u64 >= need).then(|| a[hlen..need as usize].to_vec())
    }

    fn info(&mut self, b: &[u8]) -> Result<()> {
        let mut duration = None;
        for el in children(b) {
            let (id, v) = el?;
            match id {
                0x2A_D7B1 => {
                    let ts = be_uint(v)?;
                    if ts == 0 || ts > 1_000_000_000 {
                        return Err(Error::Malformed("TimecodeScale"));
                    }
                    self.timescale = ts;
                }
                0x4489 => duration = Some(be_float(v)?),
                _ => {}
            }
        }
        if let Some(d) = duration {
            let secs = d * self.timescale as f64 / 1e9;
            if secs.is_finite() && secs >= 0.0 {
                self.out.push_back(Event::Duration(secs));
            }
        }
        Ok(())
    }

    fn parse_tracks(&mut self, b: &[u8]) -> Result<()> {
        let mut tracks = Vec::new();
        let mut by_number = HashMap::new();
        let mut delay = Vec::new();
        for el in children(b) {
            let (id, v) = el?;
            if id != 0xAE {
                continue;
            }
            if tracks.len() >= self.lim.max_tracks {
                return Err(Error::TooBig("tracks"));
            }
            let (t, d) = self.track_entry(v, tracks.len() as u8)?;
            delay.push(d);
            if by_number.insert(t.id, t.index).is_some() {
                return Err(Error::Malformed("duplicate track number"));
            }
            tracks.push(t);
        }
        self.tracks = tracks.clone();
        self.by_number = by_number;
        self.delay = delay;
        self.out.push_back(Event::Tracks(tracks));
        Ok(())
    }

    /// The track, and its CodecDelay in ns.
    fn track_entry(&self, b: &[u8], index: u8) -> Result<(TrackInfo, i64)> {
        let (mut number, mut ttype, mut codec_id, mut private) = (0u64, 0u64, String::new(), Vec::new());
        let (mut w, mut h, mut rate, mut channels) = (0u64, 0u64, 0f64, 1u64);
        let (mut default_duration, mut encrypted, mut codec_delay) = (None, false, 0u64);
        let mut color = None;
        for el in children(b) {
            let (id, v) = el?;
            match id {
                0xD7 => number = be_uint(v)?,
                0x83 => ttype = be_uint(v)?,
                0x86 => {
                    if v.len() > 64 {
                        return Err(Error::Malformed("CodecID"));
                    }
                    codec_id = String::from_utf8_lossy(v).into_owned();
                }
                0x63A2 => {
                    if v.len() > self.lim.max_config {
                        return Err(Error::TooBig("CodecPrivate"));
                    }
                    private = v.to_vec();
                }
                // CodecDelay; anything over a second is nonsense.
                0x56AA => codec_delay = be_uint(v)?.min(1_000_000_000),
                0x23_E383 => {
                    let d = be_uint(v)?;
                    default_duration = (d > 0).then_some(d);
                }
                0xE0 => {
                    for el in children(v) {
                        let (id, v) = el?;
                        match id {
                            0xB0 => w = be_uint(v)?,
                            0xBA => h = be_uint(v)?,
                            0x55B0 => color = Some(colour(v)?),
                            _ => {}
                        }
                    }
                }
                0xE1 => {
                    for el in children(v) {
                        let (id, v) = el?;
                        match id {
                            0xB5 => rate = be_float(v)?,
                            0x9F => channels = be_uint(v)?,
                            _ => {}
                        }
                    }
                }
                // ContentEncodings: encryption or compression of frames.
                0x6D80 => {
                    for enc in children(v) {
                        let (_, enc) = enc?;
                        for el in children(enc) {
                            let (id, _) = el?;
                            match id {
                                0x5035 => encrypted = true,
                                0x5034 => return Err(Error::Unsupported("compressed track")),
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if number == 0 {
            return Err(Error::Malformed("TrackNumber"));
        }
        let codec = match codec_id.as_str() {
            "V_AV1" => Codec::Av1,
            "V_VP9" => Codec::Vp9,
            "V_VP8" => Codec::Vp8,
            "V_MPEG4/ISO/AVC" => Codec::H264,
            "A_OPUS" => Codec::Opus,
            "A_VORBIS" => Codec::Vorbis,
            c if c.starts_with("A_AAC") => Codec::Aac,
            _ => Codec::Unknown,
        };
        let kind = match ttype {
            1 => {
                if w == 0 || h == 0 || w > self.lim.max_width as u64 || h > self.lim.max_height as u64 {
                    return Err(Error::TooBig("frame size"));
                }
                Kind::Video { width: w as u32, height: h as u32 }
            }
            2 => {
                if !(rate.is_finite() && (1.0..=768_000.0).contains(&rate)) || channels == 0 || channels > 255 {
                    return Err(Error::Malformed("audio track"));
                }
                Kind::Audio { rate: rate as u32, channels: channels as u16 }
            }
            _ => Kind::Other,
        };
        let config = match codec {
            Codec::Vorbis => vorbis_headers(&private)?,
            _ => private,
        };
        Ok((TrackInfo { index, id: number, codec, kind, config, default_duration_ns: default_duration, encrypted, color }, codec_delay as i64))
    }

    /// The SeekHead's entry for the Cues, if it has one.
    fn parse_seekhead(&mut self, b: &[u8]) -> Result<()> {
        let Some(seg) = self.seg_data else { return Ok(()) };
        for el in children(b) {
            let (id, v) = el?;
            if id != 0x4DBB {
                continue;
            }
            let (mut sid, mut spos) = (None, None);
            for el in children(v) {
                let (id, v) = el?;
                match id {
                    0x53AB => sid = Some(v),
                    0x53AC => spos = Some(be_uint(v)?),
                    _ => {}
                }
            }
            if let (Some(sid), Some(p)) = (sid, spos) {
                if sid == CUES.to_be_bytes() && self.cues_at.is_none() && self.cues.is_empty() {
                    self.cues_at = seg.checked_add(p);
                }
            }
        }
        Ok(())
    }

    fn parse_cues(&mut self, b: &[u8]) -> Result<()> {
        let Some(seg) = self.seg_data else { return Ok(()) };
        let mut cues = Vec::new();
        for el in children(b) {
            let (id, v) = el?;
            if id != 0xBB {
                continue;
            }
            if cues.len() >= self.lim.max_index {
                return Err(Error::TooBig("cues"));
            }
            let (mut time, mut pos) = (None, None);
            for el in children(v) {
                let (id, v) = el?;
                match id {
                    0xB3 => time = Some(be_uint(v)?),
                    0xB7 => {
                        for el in children(v) {
                            let (id, v) = el?;
                            if id == 0xF1 && pos.is_none() {
                                pos = Some(be_uint(v)?);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(t), Some(p)) = (time, pos) {
                cues.push((to_ns(t as i128 * self.timescale as i128, 1_000_000_000), seg.saturating_add(p)));
            }
        }
        cues.sort_unstable();
        self.cues = cues;
        Ok(())
    }

    fn block_group(&mut self, b: &[u8]) -> Result<()> {
        let (mut block, mut dur, mut referenced) = (None, None, false);
        for el in children(b) {
            let (id, v) = el?;
            match id {
                BLOCK => block = Some(v),
                BLOCKDURATION => dur = Some(be_uint(v)?),
                REFERENCEBLOCK => referenced = true,
                _ => {}
            }
        }
        match block {
            Some(blk) => self.block(blk, Some(!referenced), dur),
            None => Ok(()),
        }
    }

    /// A (Simple)Block. `key` is `None` for SimpleBlock (the flag says).
    fn block(&mut self, b: &[u8], key: Option<bool>, dur: Option<u64>) -> Result<()> {
        let mut r = Rd::new(b);
        let (number, n) = vint(r.rest())?.ok_or(Error::Malformed("block track"))?;
        r.skip(n)?;
        let number = number.ok_or(Error::Malformed("block track"))?;
        let rel = r.u16()? as i16 as i64;
        let flags = r.u8()?;
        let Some(&track) = self.by_number.get(&number) else { return Ok(()) };
        let cluster_tc = self.cluster_tc.ok_or(Error::Malformed("block before cluster timecode"))?;
        let key = key.unwrap_or(flags & 0x80 != 0);
        let frames = lace(r.rest(), flags, self.lim.max_frame)?;
        let ts = self.timescale as i128;
        let t0 = to_ns((cluster_tc as i128 + rel as i128) * ts, 1_000_000_000) - self.delay.get(track as usize).copied().unwrap_or(0);
        let info = &self.tracks[track as usize];
        let per = info.default_duration_ns.or_else(|| {
            dur.map(|d| to_ns(d as i128 * ts, 1_000_000_000).max(0) as u64 / frames.len().max(1) as u64)
        });
        for (i, f) in frames.into_iter().enumerate() {
            let t = t0.saturating_add(per.unwrap_or(0).saturating_mul(i as u64) as i64);
            self.out.push_back(Event::Sample(Sample {
                track,
                pts_ns: t,
                dts_ns: t,
                duration_ns: per,
                key: key && i == 0,
                data: f.to_vec(),
            }));
        }
        Ok(())
    }
}

impl Demuxer for Parser {
    fn push(&mut self, bytes: &[u8]) {
        self.inp.push(bytes);
    }

    fn poll(&mut self) -> Result<Option<Event>> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        loop {
            if let Some(ev) = self.out.pop_front() {
                return Ok(Some(ev));
            }
            if self.want.is_some() {
                return Ok(None);
            }
            match self.step() {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                // Gone to read the Cues and found junk: the SeekHead was
                // wrong, so go back and play without them.
                Err(_) if self.resume.is_some() => {
                    self.want = self.resume.take();
                    self.skip = 0;
                }
                Err(e) => {
                    self.failed = Some(e.clone());
                    return Err(e);
                }
            }
        }
    }

    fn wants_offset(&self) -> Option<u64> {
        self.want
    }

    fn input_at(&mut self, offset: u64) {
        self.want = None;
        self.inp.reset_at(offset);
        self.skip = 0;
        self.cluster_tc = None;
        self.stack = self.segment.into_iter().collect();
        self.out.clear();
    }

    fn seek(&mut self, t_ns: i64) -> bool {
        // Clusters interleave the tracks, so the cue at or before the sound's
        // pre-roll point also has the video's key frame before the target.
        let i = self.cues.partition_point(|&(t, _)| t <= t_ns - crate::AUDIO_PREROLL_NS);
        let Some(&(_, off)) = self.cues.get(i.saturating_sub(1)) else { return false };
        if self.segment.is_none() {
            return false;
        }
        self.want = Some(off);
        true
    }

    fn abort(&mut self) {
        // Back to "between elements" in the Segment: the next append starts
        // a new cluster or init segment.
        let pos = self.inp.pos();
        self.inp.reset_at(pos);
        self.skip = 0;
        self.cluster_tc = None;
        self.stack.truncate(1);
        self.failed = None;
    }

    fn set_seekable(&mut self, yes: bool) {
        self.seekable = yes;
    }

    fn finish(&mut self) -> Result<()> {
        // The SeekHead pointed past the end: play without the Cues.
        if let Some(r) = self.resume.take() {
            self.want = Some(r);
            return Ok(());
        }
        if !self.inp.is_empty() || self.skip > 0 {
            return Err(Error::Malformed("truncated at end"));
        }
        Ok(())
    }
}

/// A Video's Colour element. Its code points are H.273's.
fn colour(b: &[u8]) -> Result<crate::Color> {
    let mut c = crate::Color::default();
    let code = |v: &[u8]| -> Result<u8> { Ok(be_uint(v)?.min(255) as u8) };
    let (mut hdr, mut have_hdr) = (crate::Hdr::default(), false);
    // Chromaticity in 0.00002 units, luminance in cd/m² and 0.0001 cd/m².
    let unit = |v: &[u8], scale: f64| -> Result<u16> {
        let x = be_float(v)? * scale;
        Ok(if x.is_finite() { x.round().clamp(0.0, 65535.0) as u16 } else { 0 })
    };
    for el in children(b) {
        let (id, v) = el?;
        match id {
            0x55B1 => c.matrix = code(v)?,
            0x55B9 => {
                c.range = match be_uint(v)? {
                    1 => 1,
                    2 => 2,
                    _ => 0,
                }
            }
            0x55BA => c.transfer = code(v)?,
            0x55BB => c.primaries = code(v)?,
            0x55BC => {
                hdr.max_cll = be_uint(v)?.min(65535) as u16;
                have_hdr = true;
            }
            0x55BD => {
                hdr.max_fall = be_uint(v)?.min(65535) as u16;
                have_hdr = true;
            }
            0x55D0 => {
                for el in children(v) {
                    let (id, v) = el?;
                    match id {
                        0x55D1..=0x55D6 => {
                            let k = (id - 0x55D1) as usize;
                            let x = unit(v, 50_000.0)?;
                            if k % 2 == 0 {
                                hdr.primaries_x[k / 2] = x;
                            } else {
                                hdr.primaries_y[k / 2] = x;
                            }
                        }
                        0x55D7 => hdr.white_x = unit(v, 50_000.0)?,
                        0x55D8 => hdr.white_y = unit(v, 50_000.0)?,
                        0x55D9 => hdr.max_luminance = unit(v, 1.0)?,
                        0x55DA => hdr.min_luminance = unit(v, 10_000.0)?,
                        _ => continue,
                    }
                    have_hdr = true;
                }
            }
            _ => {}
        }
    }
    c.hdr = have_hdr.then_some(hdr);
    Ok(c)
}

fn check_ebml_header(b: &[u8]) -> Result<()> {
    let mut doctype_ok = false;
    for el in children(b) {
        let (id, v) = el?;
        match id {
            0x4282 => doctype_ok = matches!(v, b"webm" | b"matroska"),
            0x42F2 if be_uint(v)? > 4 => return Err(Error::Unsupported("EBMLMaxIDLength")),
            0x42F3 if be_uint(v)? > 8 => return Err(Error::Unsupported("EBMLMaxSizeLength")),
            _ => {}
        }
    }
    if !doctype_ok {
        return Err(Error::NotThisFormat);
    }
    Ok(())
}

/// An element header: (id, size or `None` for unknown, header length), or
/// `None` if more bytes are needed.
fn header(b: &[u8]) -> Result<Option<(u32, Option<u64>, usize)>> {
    let Some(&first) = b.first() else { return Ok(None) };
    let idlen = first.leading_zeros() as usize + 1;
    if idlen > 4 {
        return Err(Error::Malformed("element id"));
    }
    if b.len() < idlen {
        return Ok(None);
    }
    let id = b[..idlen].iter().fold(0u32, |a, &x| (a << 8) | x as u32);
    let Some((size, slen)) = vint(&b[idlen..])? else { return Ok(None) };
    Ok(Some((id, size, idlen + slen)))
}

/// An EBML variable-size integer: (value or `None` if all ones, length).
fn vint(b: &[u8]) -> Result<Option<(Option<u64>, usize)>> {
    let Some(&first) = b.first() else { return Ok(None) };
    if first == 0 {
        return Err(Error::Malformed("vint"));
    }
    let len = first.leading_zeros() as usize + 1;
    if b.len() < len {
        return Ok(None);
    }
    let mut v = (first as u64) & (0xFF >> len);
    for &x in &b[1..len] {
        v = (v << 8) | x as u64;
    }
    let all_ones = (1u64 << (7 * len)) - 1;
    Ok(Some((if v == all_ones { None } else { Some(v) }, len)))
}

/// Children of a buffered master element.
fn children(b: &[u8]) -> impl Iterator<Item = Result<(u32, &[u8])>> {
    let mut p = 0usize;
    let mut done = false;
    std::iter::from_fn(move || {
        if done || p >= b.len() {
            return None;
        }
        let r = (|| {
            let (id, size, hlen) = header(&b[p..])?.ok_or(Error::Malformed("truncated child"))?;
            let size = size.ok_or(Error::Malformed("unknown-size child"))?;
            let start = p + hlen;
            let end = (start as u64).checked_add(size).filter(|&e| e <= b.len() as u64).ok_or(Error::Malformed("child overruns"))? as usize;
            p = end;
            Ok((id, &b[start..end]))
        })();
        if r.is_err() {
            done = true;
        }
        Some(r)
    })
}

/// Split a block's payload into its frames.
fn lace(b: &[u8], flags: u8, max_frame: usize) -> Result<Vec<&[u8]>> {
    let check = |f: &[&[u8]]| -> Result<()> {
        if f.iter().any(|x| x.len() > max_frame) {
            return Err(Error::TooBig("frame"));
        }
        Ok(())
    };
    let mode = (flags >> 1) & 3;
    if mode == 0 {
        check(&[b])?;
        return Ok(vec![b]);
    }
    let mut r = Rd::new(b);
    let count = r.u8()? as usize + 1;
    let mut sizes = Vec::with_capacity(count);
    match mode {
        1 => {
            // Xiph
            for _ in 0..count - 1 {
                let mut s = 0usize;
                loop {
                    let x = r.u8()?;
                    s += x as usize;
                    if x != 255 {
                        break;
                    }
                }
                sizes.push(s);
            }
        }
        3 => {
            // EBML: first size, then signed differences.
            let (first, n) = vint(r.rest())?.ok_or(Error::Malformed("lace"))?;
            r.skip(n)?;
            let mut s = first.ok_or(Error::Malformed("lace"))? as i64;
            sizes.push(s as usize);
            for _ in 1..count - 1 {
                let (d, n) = vint(r.rest())?.ok_or(Error::Malformed("lace"))?;
                r.skip(n)?;
                let d = d.ok_or(Error::Malformed("lace"))? as i64 - ((1i64 << (7 * n - 1)) - 1);
                s = s.checked_add(d).filter(|&v| v >= 0).ok_or(Error::Malformed("lace size"))?;
                sizes.push(s as usize);
            }
        }
        _ => {
            // Fixed
            let total = r.left();
            if total % count != 0 {
                return Err(Error::Malformed("fixed lace"));
            }
            sizes = vec![total / count; count - 1];
        }
    }
    let used: usize = sizes.iter().try_fold(0usize, |a, &s| a.checked_add(s)).ok_or(Error::Malformed("lace"))?;
    if used > r.left() {
        return Err(Error::Malformed("lace overruns block"));
    }
    sizes.push(r.left() - used);
    let mut frames = Vec::with_capacity(count);
    for s in sizes {
        frames.push(r.bytes(s)?);
    }
    check(&frames)?;
    Ok(frames)
}

/// Matroska's Vorbis CodecPrivate is the three headers, Xiph-laced. Turn it
/// into three u32-LE-length-prefixed packets.
fn vorbis_headers(p: &[u8]) -> Result<Vec<u8>> {
    let mut r = Rd::new(p);
    if r.u8()? != 2 {
        return Err(Error::Malformed("Vorbis CodecPrivate"));
    }
    let mut sizes = [0usize; 2];
    for s in &mut sizes {
        loop {
            let x = r.u8()?;
            *s += x as usize;
            if x != 255 {
                break;
            }
        }
    }
    let third = r.left().checked_sub(sizes[0] + sizes[1]).ok_or(Error::Malformed("Vorbis headers"))?;
    let mut out = Vec::with_capacity(p.len() + 12);
    for s in [sizes[0], sizes[1], third] {
        out.extend_from_slice(&(s as u32).to_le_bytes());
        out.extend_from_slice(r.bytes(s)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints() {
        assert_eq!(vint(&[0x81]).unwrap(), Some((Some(1), 1)));
        assert_eq!(vint(&[0x40, 0x02]).unwrap(), Some((Some(2), 2)));
        assert_eq!(vint(&[0xFF]).unwrap(), Some((None, 1)));
        assert_eq!(vint(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap(), Some((None, 8)));
        assert_eq!(vint(&[0x40]).unwrap(), None);
        assert!(vint(&[0x00]).is_err());
    }

    #[test]
    fn lacing() {
        // Xiph: 3 frames of 2, 3, rest(1)
        let b = [2u8, 2, 3, 1, 1, 2, 2, 2, 3];
        let f = lace(&b, 0x02, 100).unwrap();
        assert_eq!(f, vec![&[1u8, 1][..], &[2, 2, 2][..], &[3][..]]);
        // Fixed: 2 frames of 2
        let f = lace(&[1, 7, 7, 8, 8], 0x04, 100).unwrap();
        assert_eq!(f, vec![&[7u8, 7][..], &[8, 8][..]]);
        // Overrun
        assert!(lace(&[1, 200, 1], 0x02, 100).is_err());
        // Fixed, indivisible
        assert!(lace(&[1, 7, 7, 8], 0x04, 100).is_err());
        // Frame over the cap
        assert_eq!(lace(&[0u8; 10], 0, 4), Err(Error::TooBig("frame")));
    }
}
