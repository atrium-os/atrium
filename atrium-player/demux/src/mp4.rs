//! MP4 (ISO BMFF): plain files with `moov` sample tables, and fragmented
//! files and MSE segments (`moof` + `mdat`). Boxes other than `moov` and
//! `moof` are skipped by size, and `mdat` is streamed: only the samples the
//! tables point at are read out of it, one at a time.
use std::collections::{HashMap, VecDeque};

use crate::rd::Rd;
use crate::{to_ns, Codec, Demuxer, Error, Event, Input, Kind, Limits, Result, Sample, TrackInfo};

#[derive(Debug, Clone, Copy)]
struct Pending {
    track: u8,
    offset: u64,
    size: u32,
    dts: i64,
    pts: i64,
    dur: Option<u64>,
    key: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Trex {
    sdi: u32,
    dur: u32,
    size: u32,
    flags: u32,
}

#[derive(Debug, Clone)]
struct Trk {
    timescale: u64,
    trex: Trex,
    /// Next decode time for fragments without `tfdt`, in timescale units.
    next_dts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Top,
    /// Inside an `mdat` payload ending here (`None`: to end of input).
    Mdat(Option<u64>),
}

pub struct Parser {
    lim: Limits,
    inp: Input,
    state: State,
    skip: u64,
    tracks: Vec<TrackInfo>,
    trks: Vec<Trk>,
    by_id: HashMap<u32, u8>,
    /// Samples to read out of the coming (or current) `mdat`s, by offset.
    pending: VecDeque<Pending>,
    /// A plain file's whole sample table, by offset, for seeking.
    index: Vec<Pending>,
    /// `mdat` payloads seen: (start, end).
    mdats: Vec<(u64, Option<u64>)>,
    moov_seen: bool,
    want: Option<u64>,
    out: VecDeque<Event>,
    failed: Option<Error>,
}

type FourCc = [u8; 4];

impl Parser {
    pub fn new(lim: Limits) -> Self {
        Parser {
            lim,
            inp: Input::new(),
            state: State::Top,
            skip: 0,
            tracks: Vec::new(),
            trks: Vec::new(),
            by_id: HashMap::new(),
            pending: VecDeque::new(),
            index: Vec::new(),
            mdats: Vec::new(),
            moov_seen: false,
            want: None,
            out: VecDeque::new(),
            failed: None,
        }
    }

    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }

    fn step(&mut self) -> Result<bool> {
        if self.skip > 0 {
            let n = self.skip.min(self.inp.avail().len() as u64) as usize;
            if n == 0 {
                return Ok(false);
            }
            self.inp.consume(n);
            self.skip -= n as u64;
            return Ok(true);
        }
        let pos = self.inp.pos();
        if let State::Mdat(end) = self.state {
            if end.is_some_and(|e| pos >= e) {
                self.state = State::Top;
                return Ok(true);
            }
            // Drop samples that point behind us (overlapping or out of order).
            while self.pending.front().is_some_and(|s| s.offset < pos) {
                self.pending.pop_front();
            }
            let next = self.pending.front().copied().filter(|s| end.map_or(true, |e| s.offset < e));
            let Some(s) = next else {
                // Nothing more in this mdat.
                match end {
                    Some(e) => self.skip = e - pos,
                    None => self.skip = u64::MAX,
                }
                return Ok(true);
            };
            if end.is_some_and(|e| s.offset + s.size as u64 > e) {
                return Err(Error::Malformed("sample overruns mdat"));
            }
            if s.offset > pos {
                self.skip = s.offset - pos;
                return Ok(true);
            }
            let a = self.inp.avail();
            if a.len() < s.size as usize {
                return Ok(false);
            }
            let data = a[..s.size as usize].to_vec();
            self.inp.consume(s.size as usize);
            self.pending.pop_front();
            self.out.push_back(Event::Sample(Sample {
                track: s.track,
                pts_ns: s.pts,
                dts_ns: s.dts,
                duration_ns: s.dur,
                key: s.key,
                data,
            }));
            return Ok(true);
        }

        let Some((ty, size, hlen)) = box_header(self.inp.avail())? else { return Ok(false) };
        let body_start = pos + hlen as u64;
        match &ty {
            b"mdat" => {
                self.inp.consume(hlen);
                let end = size.map(|s| body_start + s);
                self.mdats.push((body_start, end));
                if self.mdats.len() > 1 << 20 {
                    return Err(Error::TooBig("mdat count"));
                }
                if !self.moov_seen {
                    // A plain file with its moov after the mdat: read the moov first.
                    match end {
                        Some(e) => self.want = Some(e),
                        None => return Err(Error::Malformed("mdat to end before moov")),
                    }
                    return Ok(true);
                }
                self.state = State::Mdat(end);
                Ok(true)
            }
            b"moov" | b"moof" => {
                let size = size.ok_or(Error::Malformed("box to end"))?;
                if size > self.lim.max_buffered as u64 {
                    return Err(Error::TooBig("box"));
                }
                let a = self.inp.avail();
                if (a.len() as u64) < hlen as u64 + size {
                    return Ok(false);
                }
                let body = a[hlen..hlen + size as usize].to_vec();
                self.inp.consume(hlen + size as usize);
                if &ty == b"moov" {
                    // A plain file's moov is read once (we may pass it again
                    // after reading its mdat); a segment feed may send a new
                    // init segment at any time.
                    if self.index.is_empty() {
                        self.moov(&body)?;
                    }
                } else {
                    self.moof(&body, pos)?;
                }
                Ok(true)
            }
            _ => {
                self.inp.consume(hlen);
                self.skip = size.unwrap_or(u64::MAX);
                Ok(true)
            }
        }
    }

    fn moov(&mut self, b: &[u8]) -> Result<()> {
        let mut tracks = Vec::new();
        let mut trks = Vec::new();
        let mut by_id = HashMap::new();
        let mut index = Vec::new();
        let mut trex: HashMap<u32, Trex> = HashMap::new();
        let mut duration = None;
        for bx in boxes(b) {
            let (ty, v) = bx?;
            match &ty {
                b"mvhd" => duration = mvhd(v)?,
                b"mvex" => {
                    for bx in boxes(v) {
                        let (ty, v) = bx?;
                        if &ty == b"trex" {
                            let mut r = Rd::new(v);
                            r.skip(4)?;
                            let id = r.u32()?;
                            trex.insert(id, Trex { sdi: r.u32()?, dur: r.u32()?, size: r.u32()?, flags: r.u32()? });
                        }
                    }
                }
                b"trak" => {
                    if tracks.len() >= self.lim.max_tracks {
                        return Err(Error::TooBig("tracks"));
                    }
                    let idx = tracks.len() as u8;
                    let (info, ts, samples) = self.trak(v, idx, self.lim.max_index.saturating_sub(index.len()))?;
                    if by_id.insert(info.id as u32, idx).is_some() {
                        return Err(Error::Malformed("duplicate track id"));
                    }
                    tracks.push(info);
                    trks.push(Trk { timescale: ts, trex: Trex::default(), next_dts: 0 });
                    index.extend(samples);
                }
                _ => {}
            }
        }
        for (id, &i) in &by_id {
            if let Some(t) = trex.get(id) {
                trks[i as usize].trex = *t;
            }
        }
        index.sort_by_key(|s| s.offset);
        self.tracks = tracks.clone();
        self.trks = trks;
        self.by_id = by_id;
        self.moov_seen = true;
        self.out.push_back(Event::Tracks(tracks));
        if let Some(d) = duration {
            self.out.push_back(Event::Duration(d));
        }
        if !index.is_empty() {
            self.pending = index.iter().copied().collect();
            let first = index[0].offset;
            self.index = index;
            if first < self.inp.pos() {
                // The mdat came first: go back for it.
                self.want = Some(first);
            }
        }
        Ok(())
    }

    /// (track, timescale, plain sample table).
    fn trak(&self, b: &[u8], idx: u8, room: usize) -> Result<(TrackInfo, u64, Vec<Pending>)> {
        let mut id = 0u32;
        let mut ts = 0u64;
        let mut handler = [0u8; 4];
        let mut stbl: Option<&[u8]> = None;
        for bx in boxes(b) {
            let (ty, v) = bx?;
            match &ty {
                b"tkhd" => {
                    let mut r = Rd::new(v);
                    let ver = r.u8()?;
                    r.skip(3)?;
                    r.skip(if ver == 1 { 16 } else { 8 })?;
                    id = r.u32()?;
                }
                b"mdia" => {
                    for bx in boxes(v) {
                        let (ty, v) = bx?;
                        match &ty {
                            b"mdhd" => {
                                let mut r = Rd::new(v);
                                let ver = r.u8()?;
                                r.skip(3)?;
                                r.skip(if ver == 1 { 16 } else { 8 })?;
                                ts = r.u32()? as u64;
                            }
                            b"hdlr" => {
                                let mut r = Rd::new(v);
                                r.skip(8)?;
                                handler.copy_from_slice(r.bytes(4)?);
                            }
                            b"minf" => {
                                for bx in boxes(v) {
                                    let (ty, v) = bx?;
                                    if &ty == b"stbl" {
                                        stbl = Some(v);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if id == 0 || ts == 0 {
            return Err(Error::Malformed("trak without id or timescale"));
        }
        let stbl = stbl.ok_or(Error::Malformed("trak without stbl"))?;
        let mut info = TrackInfo { index: idx, id: id as u64, codec: Codec::Unknown, kind: Kind::Other, config: Vec::new(), default_duration_ns: None, encrypted: false };
        let samples = self.stbl(stbl, &mut info, &handler, ts, room)?;
        Ok((info, ts, samples))
    }

    fn stbl(&self, b: &[u8], info: &mut TrackInfo, handler: &FourCc, ts: u64, room: usize) -> Result<Vec<Pending>> {
        let (mut stts, mut ctts, mut stss, mut stsz, mut stsc, mut stco) = (None, None, None, None, None, None);
        let mut co64 = false;
        for bx in boxes(b) {
            let (ty, v) = bx?;
            match &ty {
                b"stsd" => self.stsd(v, info, handler)?,
                b"stts" => stts = Some(v),
                b"ctts" => ctts = Some(v),
                b"stss" => stss = Some(v),
                b"stsz" | b"stz2" => stsz = Some((ty, v)),
                b"stsc" => stsc = Some(v),
                b"stco" => stco = Some(v),
                b"co64" => {
                    stco = Some(v);
                    co64 = true;
                }
                _ => {}
            }
        }
        let Some((szty, szv)) = stsz else { return Ok(Vec::new()) };
        let sizes = sample_sizes(&szty, szv, room, self.lim.max_frame)?;
        let n = sizes.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let (stts, stsc, stco) = match (stts, stsc, stco) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Err(Error::Malformed("sample table incomplete")),
        };
        // Decode times.
        let mut dts = Vec::with_capacity(n);
        let mut durs = Vec::with_capacity(n);
        let mut t: u64 = 0;
        let mut r = full_box(stts)?;
        let count = r.u32()? as usize;
        for _ in 0..count {
            let (c, d) = (r.u32()? as usize, r.u32()? as u64);
            for _ in 0..c.min(n - dts.len()) {
                dts.push(t);
                durs.push(d);
                t = t.saturating_add(d);
            }
            if dts.len() == n {
                break;
            }
        }
        if dts.len() != n {
            return Err(Error::Malformed("stts shorter than stsz"));
        }
        // Composition offsets.
        let mut cto = vec![0i64; n];
        if let Some(c) = ctts {
            let mut r = full_box(c)?;
            let count = r.u32()? as usize;
            let mut i = 0usize;
            for _ in 0..count {
                let (c, o) = (r.u32()? as usize, r.u32()? as i32 as i64);
                for _ in 0..c.min(n - i) {
                    cto[i] = o;
                    i += 1;
                }
                if i == n {
                    break;
                }
            }
        }
        // Sync samples.
        let mut key = vec![stss.is_none(); n];
        if let Some(s) = stss {
            let mut r = full_box(s)?;
            let count = r.u32()? as usize;
            for _ in 0..count.min(n) {
                let k = r.u32()? as usize;
                if k >= 1 && k <= n {
                    key[k - 1] = true;
                }
            }
        }
        // Chunk offsets.
        let mut r = full_box(stco)?;
        let chunks = r.u32()? as usize;
        if chunks > self.lim.max_index || chunks.saturating_mul(if co64 { 8 } else { 4 }) > r.left() {
            return Err(Error::Malformed("chunk table"));
        }
        let mut chunk_off = Vec::with_capacity(chunks);
        for _ in 0..chunks {
            chunk_off.push(if co64 { r.u64()? } else { r.u32()? as u64 });
        }
        let mut r = full_box(stsc)?;
        let runs = r.u32()? as usize;
        if runs.saturating_mul(12) > r.left() {
            return Err(Error::Malformed("stsc"));
        }
        let mut stsc_v = Vec::with_capacity(runs);
        for _ in 0..runs {
            let (first, spc) = (r.u32()? as usize, r.u32()? as usize);
            r.skip(4)?;
            stsc_v.push((first, spc));
        }
        let mut out = Vec::with_capacity(n);
        let mut s = 0usize;
        for (ri, &(first, spc)) in stsc_v.iter().enumerate() {
            if first == 0 || spc == 0 {
                return Err(Error::Malformed("stsc entry"));
            }
            let last = stsc_v.get(ri + 1).map_or(chunks, |&(f, _)| f.saturating_sub(1)).min(chunks);
            for c in first..=last {
                let mut off = chunk_off[c - 1];
                for _ in 0..spc {
                    if s == n {
                        break;
                    }
                    let d = dts[s] as i128;
                    out.push(Pending {
                        track: info.index,
                        offset: off,
                        size: sizes[s],
                        dts: to_ns(d, ts),
                        pts: to_ns(d + cto[s] as i128, ts),
                        dur: Some(to_ns(durs[s] as i128, ts).max(0) as u64),
                        key: key[s],
                    });
                    off = off.checked_add(sizes[s] as u64).ok_or(Error::Malformed("sample offset"))?;
                    s += 1;
                }
            }
        }
        if s != n {
            return Err(Error::Malformed("chunks hold fewer samples than stsz"));
        }
        Ok(out)
    }

    fn stsd(&self, b: &[u8], info: &mut TrackInfo, handler: &FourCc) -> Result<()> {
        let mut r = full_box(b)?;
        if r.u32()? == 0 {
            return Ok(());
        }
        // The first sample entry describes the track.
        let Some(entry) = boxes(r.rest()).next() else { return Ok(()) };
        let (mut ty, v) = entry?;
        let video = handler == b"vide";
        let audio = handler == b"soun";
        let mut r = Rd::new(v);
        r.skip(8)?; // reserved, data_reference_index
        let kind;
        if video {
            r.skip(16)?;
            let (w, h) = (r.u16()? as u32, r.u16()? as u32);
            r.skip(50)?;
            if w == 0 || h == 0 || w > self.lim.max_width || h > self.lim.max_height {
                return Err(Error::TooBig("frame size"));
            }
            kind = Kind::Video { width: w, height: h };
        } else if audio {
            let ver = r.u16()?;
            r.skip(6)?;
            let ch = r.u16()?;
            r.skip(6)?;
            let rate = r.u32()? >> 16;
            r.skip(match ver {
                1 => 16,
                2 => 36,
                _ => 0,
            })?;
            if ch == 0 {
                return Err(Error::Malformed("audio channels"));
            }
            kind = Kind::Audio { rate, channels: ch };
        } else {
            info.kind = Kind::Other;
            return Ok(());
        }
        let mut config = Vec::new();
        let mut opus_head = None;
        for bx in boxes(r.rest()) {
            let (cty, cv) = bx?;
            if cv.len() > self.lim.max_config {
                return Err(Error::TooBig("codec config"));
            }
            match &cty {
                b"av1C" | b"vpcC" | b"avcC" | b"esds" => config = cv.to_vec(),
                b"dOps" => opus_head = Some(dops_to_opushead(cv)?),
                b"sinf" => {
                    info.encrypted = true;
                    for bx in boxes(cv) {
                        let (sty, sv) = bx?;
                        if &sty == b"frma" && sv.len() >= 4 {
                            ty.copy_from_slice(&sv[..4]);
                        }
                    }
                }
                _ => {}
            }
        }
        if matches!(&ty, b"encv" | b"enca") {
            info.encrypted = true;
        }
        info.codec = match &ty {
            b"av01" => Codec::Av1,
            b"vp09" => Codec::Vp9,
            b"vp08" => Codec::Vp8,
            b"avc1" | b"avc3" => Codec::H264,
            b"Opus" => Codec::Opus,
            b"mp4a" => Codec::Aac,
            _ => Codec::Unknown,
        };
        info.config = opus_head.unwrap_or(config);
        info.kind = kind;
        Ok(())
    }

    fn moof(&mut self, b: &[u8], moof_start: u64) -> Result<()> {
        if !self.moov_seen {
            return Err(Error::Malformed("moof before moov"));
        }
        let mut new = Vec::new();
        for bx in boxes(b) {
            let (ty, v) = bx?;
            if &ty != b"traf" {
                continue;
            }
            let mut tf: Option<(u8, Trex, u64)> = None; // track, defaults, base offset
            let mut base_time = None;
            let mut data_ptr: Option<u64> = None;
            for bx in boxes(v) {
                let (ty, v) = bx?;
                match &ty {
                    b"tfhd" => {
                        let mut r = Rd::new(v);
                        let flags = r.u32()? & 0xFF_FFFF;
                        let id = r.u32()?;
                        let Some(&ti) = self.by_id.get(&id) else { return Err(Error::Malformed("traf for unknown track")) };
                        let mut d = self.trks[ti as usize].trex;
                        let mut base = moof_start;
                        if flags & 0x1 != 0 {
                            base = r.u64()?;
                        }
                        if flags & 0x2 != 0 {
                            d.sdi = r.u32()?;
                        }
                        if flags & 0x8 != 0 {
                            d.dur = r.u32()?;
                        }
                        if flags & 0x10 != 0 {
                            d.size = r.u32()?;
                        }
                        if flags & 0x20 != 0 {
                            d.flags = r.u32()?;
                        }
                        tf = Some((ti, d, base));
                    }
                    b"tfdt" => {
                        let mut r = Rd::new(v);
                        let ver = r.u8()?;
                        r.skip(3)?;
                        base_time = Some(if ver == 1 { r.u64()? } else { r.u32()? as u64 });
                    }
                    b"trun" => {
                        let Some((ti, d, base)) = tf else { return Err(Error::Malformed("trun before tfhd")) };
                        let trk = &mut self.trks[ti as usize];
                        if let Some(bt) = base_time.take() {
                            trk.next_dts = bt;
                        }
                        let mut r = Rd::new(v);
                        let vf = r.u32()?;
                        let (ver, flags) = (vf >> 24, vf & 0xFF_FFFF);
                        let count = r.u32()? as usize;
                        if count > self.lim.max_index || new.len() + count > self.lim.max_index {
                            return Err(Error::TooBig("trun"));
                        }
                        let mut ptr = match (flags & 0x1 != 0, data_ptr) {
                            (true, _) => {
                                let off = r.u32()? as i32 as i64;
                                base.checked_add_signed(off).ok_or(Error::Malformed("data offset"))?
                            }
                            (false, Some(p)) => p,
                            (false, None) => base,
                        };
                        let first_flags = if flags & 0x4 != 0 { Some(r.u32()?) } else { None };
                        let per = [0x100, 0x200, 0x400, 0x800].iter().filter(|&&f| flags & f != 0).count();
                        if count.saturating_mul(per * 4) > r.left() {
                            return Err(Error::Malformed("trun overruns"));
                        }
                        for i in 0..count {
                            let dur = if flags & 0x100 != 0 { r.u32()? } else { d.dur };
                            let size = if flags & 0x200 != 0 { r.u32()? } else { d.size };
                            let mut sf = if flags & 0x400 != 0 { r.u32()? } else { d.flags };
                            if i == 0 {
                                if let Some(f) = first_flags {
                                    sf = f;
                                }
                            }
                            let cto = if flags & 0x800 != 0 {
                                let c = r.u32()?;
                                if ver == 0 { c as i64 } else { c as i32 as i64 }
                            } else {
                                0
                            };
                            if size as usize > self.lim.max_frame {
                                return Err(Error::TooBig("frame"));
                            }
                            let dt = trk.next_dts as i128;
                            new.push(Pending {
                                track: ti,
                                offset: ptr,
                                size,
                                dts: to_ns(dt, trk.timescale),
                                pts: to_ns(dt + cto as i128, trk.timescale),
                                dur: Some(to_ns(dur as i128, trk.timescale).max(0) as u64),
                                key: sf & 0x0001_0000 == 0,
                            });
                            trk.next_dts = trk.next_dts.saturating_add(dur as u64);
                            ptr = ptr.checked_add(size as u64).ok_or(Error::Malformed("sample offset"))?;
                        }
                        data_ptr = Some(ptr);
                    }
                    _ => {}
                }
            }
        }
        new.sort_by_key(|s| s.offset);
        self.pending.extend(new);
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
        self.state = match self.mdats.iter().find(|&&(s, e)| offset >= s && e.map_or(true, |e| offset < e)) {
            Some(&(_, e)) => State::Mdat(e),
            None => State::Top,
        };
    }

    fn seek(&mut self, t_ns: i64) -> bool {
        if self.index.is_empty() {
            return false;
        }
        // The key sample of the first video track (or the first track) at or before t.
        let vt = self.tracks.iter().find(|t| t.codec.is_video()).map_or(0, |t| t.index);
        let Some(k) = self.index.iter().filter(|s| s.track == vt && s.key && s.pts <= t_ns).max_by_key(|s| s.pts).copied()
            .or_else(|| self.index.iter().find(|s| s.track == vt && s.key).copied())
        else {
            return false;
        };
        self.pending = self.index.iter().filter(|s| s.offset >= k.offset).copied().collect();
        self.out.clear();
        self.want = Some(k.offset);
        true
    }

    fn abort(&mut self) {
        let pos = self.inp.pos();
        self.inp.reset_at(pos);
        self.skip = 0;
        self.state = State::Top;
        self.pending.clear();
        self.failed = None;
    }

    fn finish(&mut self) -> Result<()> {
        if self.skip > 0 && self.skip != u64::MAX {
            return Err(Error::Malformed("truncated at end"));
        }
        if !self.inp.is_empty() || !self.pending.is_empty() && self.index.is_empty() {
            return Err(Error::Malformed("truncated at end"));
        }
        Ok(())
    }
}

/// (type, body size or `None` for "to end", header length), or `None` if more bytes are needed.
fn box_header(b: &[u8]) -> Result<Option<(FourCc, Option<u64>, usize)>> {
    if b.len() < 8 {
        return Ok(None);
    }
    let size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
    let ty = [b[4], b[5], b[6], b[7]];
    let (size, hlen) = match size {
        0 => (None, 8),
        1 => {
            if b.len() < 16 {
                return Ok(None);
            }
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[8..16]);
            (Some(u64::from_be_bytes(a)), 16)
        }
        s => (Some(s), 8),
    };
    let body = match size {
        Some(s) if s < hlen as u64 => return Err(Error::Malformed("box size")),
        Some(s) => Some(s - hlen as u64),
        None => None,
    };
    Ok(Some((ty, body, hlen)))
}

/// Child boxes of a buffered box.
fn boxes(b: &[u8]) -> impl Iterator<Item = Result<(FourCc, &[u8])>> {
    let mut p = 0usize;
    let mut done = false;
    std::iter::from_fn(move || {
        if done || b.len() - p < 8 {
            return None;
        }
        let r = (|| {
            let (ty, size, hlen) = box_header(&b[p..])?.ok_or(Error::Malformed("truncated box"))?;
            let start = p + hlen;
            let end = match size {
                None => b.len(),
                Some(s) => (start as u64).checked_add(s).filter(|&e| e <= b.len() as u64).ok_or(Error::Malformed("box overruns"))? as usize,
            };
            p = end;
            Ok((ty, &b[start..end]))
        })();
        if r.is_err() {
            done = true;
        }
        Some(r)
    })
}

/// Skip a full box's version and flags.
fn full_box(b: &[u8]) -> Result<Rd<'_>> {
    let mut r = Rd::new(b);
    r.skip(4)?;
    Ok(r)
}

fn mvhd(b: &[u8]) -> Result<Option<f64>> {
    let mut r = Rd::new(b);
    let ver = r.u8()?;
    r.skip(3)?;
    let (ts, dur) = if ver == 1 {
        r.skip(16)?;
        (r.u32()? as u64, r.u64()?)
    } else {
        r.skip(8)?;
        let ts = r.u32()? as u64;
        let d = r.u32()?;
        (ts, if d == u32::MAX { u64::MAX } else { d as u64 })
    };
    if ts == 0 || dur == 0 || dur == u64::MAX {
        return Ok(None);
    }
    Ok(Some(dur as f64 / ts as f64))
}

fn sample_sizes(ty: &FourCc, b: &[u8], room: usize, max_frame: usize) -> Result<Vec<u32>> {
    let mut r = full_box(b)?;
    let mut out;
    if ty == b"stsz" {
        let fixed = r.u32()?;
        let n = r.u32()? as usize;
        if n > room {
            return Err(Error::TooBig("sample count"));
        }
        if fixed != 0 {
            out = vec![fixed; n];
        } else {
            if n.saturating_mul(4) > r.left() {
                return Err(Error::Malformed("stsz"));
            }
            out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(r.u32()?);
            }
        }
    } else {
        r.skip(3)?;
        let field = r.u8()?;
        let n = r.u32()? as usize;
        if n > room {
            return Err(Error::TooBig("sample count"));
        }
        out = Vec::with_capacity(n);
        match field {
            4 => {
                if n.div_ceil(2) > r.left() {
                    return Err(Error::Malformed("stz2"));
                }
                for i in 0..n {
                    let x = r.rest()[i / 2];
                    out.push(if i % 2 == 0 { (x >> 4) as u32 } else { (x & 15) as u32 });
                }
            }
            8 => {
                for _ in 0..n {
                    out.push(r.u8()? as u32);
                }
            }
            16 => {
                for _ in 0..n {
                    out.push(r.u16()? as u32);
                }
            }
            _ => return Err(Error::Malformed("stz2 field size")),
        }
    }
    if out.iter().any(|&s| s as usize > max_frame) {
        return Err(Error::TooBig("frame"));
    }
    Ok(out)
}

/// The MP4 Opus config (`dOps`, big-endian) as an Ogg `OpusHead` (little-endian),
/// which is what the decoder and WebM use.
fn dops_to_opushead(b: &[u8]) -> Result<Vec<u8>> {
    let mut r = Rd::new(b);
    if r.u8()? != 0 {
        return Err(Error::Unsupported("dOps version"));
    }
    let ch = r.u8()?;
    let pre = r.u16()?;
    let rate = r.u32()?;
    let gain = r.u16()?;
    let family = r.u8()?;
    let mut h = Vec::with_capacity(19 + 2 + ch as usize);
    h.extend_from_slice(b"OpusHead");
    h.push(1);
    h.push(ch);
    h.extend_from_slice(&pre.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&gain.to_le_bytes());
    h.push(family);
    if family != 0 {
        h.extend_from_slice(r.bytes(2 + ch as usize)?);
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_sizes() {
        assert_eq!(box_header(b"\0\0\0\x08free").unwrap(), Some((*b"free", Some(0), 8)));
        assert!(box_header(b"\0\0\0\x04free").is_err());
        assert_eq!(box_header(b"\0\0\0\0mdat").unwrap(), Some((*b"mdat", None, 8)));
        assert_eq!(box_header(b"\0\0\0\x01mdat\0\0\0\0").unwrap(), None);
    }

    #[test]
    fn opushead() {
        let d = [0u8, 2, 0x01, 0x38, 0, 0, 0xBB, 0x80, 0, 0, 0];
        let h = dops_to_opushead(&d).unwrap();
        assert_eq!(&h[..8], b"OpusHead");
        assert_eq!(h[9], 2);
        assert_eq!(u16::from_le_bytes([h[10], h[11]]), 312);
        assert_eq!(u32::from_le_bytes([h[12], h[13], h[14], h[15]]), 48000);
    }
}
