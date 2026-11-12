//! MP4 (ISO BMFF): plain files with `moov` sample tables, and fragmented
//! files and MSE segments (`moof` + `mdat`). Boxes other than `moov` and
//! `moof` are skipped by size, and `mdat` is streamed: only the samples the
//! tables point at are read out of it, one at a time.
use std::collections::{HashMap, VecDeque};

use crate::rd::Rd;
use crate::{to_ns, Codec, Crypto, Demuxer, Error, Event, InitDataKind, Input, Kind, Limits, Protection, Result, Sample, Scheme, TrackInfo};

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
    /// The edit list's start (media timescale): presentation times begin
    /// there (Opus pre-skip, B-frame delay).
    shift: i64,
    trex: Trex,
    /// Next decode time for fragments without `tfdt`, in timescale units.
    next_dts: u64,
    /// Sample descriptions (bit `i` for index `i + 1`) that are not encrypted: a fragment
    /// that uses one is clear (a "clear lead", as Shaka Packager writes it).
    clear_sdi: u64,
}

/// [`Trk::clear_sdi`] from a `trak` body: the `stsd` entries that are not `encv`/`enca`, if
/// any entry is.
fn clear_entries(trak: &[u8]) -> u64 {
    fn child<'a>(b: &'a [u8], want: &FourCc) -> Option<&'a [u8]> {
        boxes(b).flatten().find(|(t, _)| t == want).map(|(_, v)| v)
    }
    let Some(stsd) = child(trak, b"mdia").and_then(|b| child(b, b"minf")).and_then(|b| child(b, b"stbl")).and_then(|b| child(b, b"stsd")) else { return 0 };
    let Some(entries) = stsd.get(8..) else { return 0 };
    let (mut clear, mut any_enc) = (0u64, false);
    for (i, (t, _)) in boxes(entries).flatten().enumerate().take(64) {
        if matches!(&t, b"encv" | b"enca") {
            any_enc = true;
        } else {
            clear |= 1 << i;
        }
    }
    if any_enc { clear } else { 0 }
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
    /// CENC parameters of encrypted samples, by sample offset.
    crypto: HashMap<u64, Box<Crypto>>,
    /// `pssh` sets already announced.
    init_data: Vec<Vec<u8>>,
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
            crypto: HashMap::new(),
            init_data: Vec::new(),
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
            // A plain file keeps its table (and the CENC data) for seeking.
            let crypto = if self.index.is_empty() { self.crypto.remove(&s.offset) } else { self.crypto.get(&s.offset).cloned() };
            self.out.push_back(Event::Sample(Sample {
                track: s.track,
                pts_ns: s.pts,
                dts_ns: s.dts,
                duration_ns: s.dur,
                key: s.key,
                data,
                crypto,
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
                        self.moov(&body, body_start)?;
                    }
                } else {
                    self.moof(&body, pos, body_start)?;
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

    fn moov(&mut self, b: &[u8], b_abs: u64) -> Result<()> {
        let mut tracks = Vec::new();
        let mut trks = Vec::new();
        let mut by_id = HashMap::new();
        let mut index = Vec::new();
        let mut trex: HashMap<u32, Trex> = HashMap::new();
        let mut duration = None;
        let mut pssh = Vec::new();
        let mut crypto = HashMap::new();
        for bx in boxes(b) {
            let (ty, v) = bx?;
            match &ty {
                b"mvhd" => duration = mvhd(v)?,
                b"pssh" => push_pssh(&mut pssh, v),
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
                    let (info, ts, shift, samples) = self.trak(v, idx, self.lim.max_index.saturating_sub(index.len()), (b, b_abs), &mut crypto)?;
                    if by_id.insert(info.id as u32, idx).is_some() {
                        return Err(Error::Malformed("duplicate track id"));
                    }
                    tracks.push(info);
                    trks.push(Trk { timescale: ts, shift, trex: Trex::default(), next_dts: 0, clear_sdi: clear_entries(v) });
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
        self.crypto = crypto;
        self.out.push_back(Event::Tracks(tracks));
        self.announce(pssh);
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

    /// (track, timescale, edit shift, plain sample table).
    fn trak(&self, b: &[u8], idx: u8, room: usize, outer: (&[u8], u64), crypto: &mut HashMap<u64, Box<Crypto>>) -> Result<(TrackInfo, u64, i64, Vec<Pending>)> {
        let mut id = 0u32;
        let mut senc = None;
        let mut shift = 0i64;
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
                // Where GPAC puts a plain file's sample encryption.
                b"senc" => senc = Some(v),
                // The first edit that shows media says where it starts.
                b"edts" => {
                    for bx in boxes(v) {
                        let (ty, v) = bx?;
                        if &ty != b"elst" {
                            continue;
                        }
                        let mut r = Rd::new(v);
                        let ver = r.u8()?;
                        r.skip(3)?;
                        let n = r.u32()?.min(64);
                        for _ in 0..n {
                            let t = if ver == 1 {
                                r.skip(8)?;
                                r.u64()? as i64
                            } else {
                                r.skip(4)?;
                                r.u32()? as i32 as i64
                            };
                            r.skip(4)?;
                            if t >= 0 {
                                shift = t;
                                break;
                            }
                        }
                    }
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
        let mut info = TrackInfo { index: idx, id: id as u64, codec: Codec::Unknown, kind: Kind::Other, config: Vec::new(), default_duration_ns: None, encrypted: false, protection: None, color: None };
        let samples = self.stbl(stbl, &mut info, &handler, ts, shift, room)?;
        // A fragmented file's moov has no samples; its fragments carry the aux information.
        if let Some(p) = info.protection.as_ref().filter(|_| !samples.is_empty()) {
            let (mut saiz, mut saio) = (None, None);
            for bx in boxes(stbl) {
                let (ty, v) = bx?;
                match &ty {
                    b"senc" => senc = Some(v),
                    b"saiz" => saiz = Some(v),
                    b"saio" => saio = Some(v),
                    b"sbgp" | b"sgpd" if v.len() >= 8 && &v[4..8] == b"seig" => return Err(Error::Unsupported("CENC key rotation (seig)")),
                    _ => {}
                }
            }
            // A plain file's saio offsets are file offsets.
            let aux = aux_info(Aux { senc, saiz, saio, outer, saio_base: 0 }, p, &samples)?;
            for (s, c) in samples.iter().zip(aux) {
                if let Some(c) = c {
                    crypto.insert(s.offset, c);
                }
            }
        }
        Ok((info, ts, shift, samples))
    }

    fn stbl(&self, b: &[u8], info: &mut TrackInfo, handler: &FourCc, ts: u64, shift: i64, room: usize) -> Result<Vec<Pending>> {
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
                        pts: to_ns(d + cto[s] as i128 - shift as i128, ts),
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
        if matches!(handler, b"text" | b"sbtl" | b"subt") {
            info.codec = match &ty {
                b"tx3g" => Codec::Tx3g,
                b"wvtt" => Codec::WebVtt,
                _ => Codec::Unknown,
            };
            info.kind = if info.codec == Codec::Unknown { Kind::Other } else { Kind::Text };
            return Ok(());
        }
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
        let (mut color, mut nclx, mut hdr) = (None::<crate::Color>, false, None::<crate::Hdr>);
        for bx in boxes(r.rest()) {
            let (cty, cv) = bx?;
            if cv.len() > self.lim.max_config {
                return Err(Error::TooBig("codec config"));
            }
            match &cty {
                b"av1C" | b"vpcC" | b"avcC" | b"esds" => {
                    config = cv.to_vec();
                    // vpcC 1: profile, level, depth|subsampling|full range,
                    // then primaries, transfer, matrix. A colr box wins.
                    if &cty == b"vpcC" && cv.len() >= 10 && cv[0] == 1 && !nclx {
                        color = Some(crate::Color { primaries: cv[7], transfer: cv[8], matrix: cv[9], range: 1 + (cv[6] & 1), hdr: None });
                    }
                }
                b"colr" if cv.len() >= 11 && &cv[..4] == b"nclx" => {
                    let u = |i: usize| u16::from_be_bytes([cv[i], cv[i + 1]]).min(255) as u8;
                    color = Some(crate::Color { primaries: u(4), transfer: u(6), matrix: u(8), range: 1 + (cv[10] >> 7), hdr: None });
                    nclx = true;
                }
                // Mastering display (G, B, R order, as in HEVC's SEI), then
                // content light level.
                b"mdcv" if cv.len() >= 24 => {
                    let u = |i: usize| u16::from_be_bytes([cv[i], cv[i + 1]]);
                    let l = |i: usize| u32::from_be_bytes([cv[i], cv[i + 1], cv[i + 2], cv[i + 3]]);
                    let h = hdr.get_or_insert_with(Default::default);
                    for (k, rgb) in [1usize, 2, 0].into_iter().enumerate() {
                        h.primaries_x[rgb] = u(4 * k);
                        h.primaries_y[rgb] = u(4 * k + 2);
                    }
                    h.white_x = u(12);
                    h.white_y = u(14);
                    h.max_luminance = (l(16) / 10_000).min(65535) as u16;
                    h.min_luminance = l(20).min(65535) as u16;
                }
                b"clli" if cv.len() >= 4 => {
                    let h = hdr.get_or_insert_with(Default::default);
                    h.max_cll = u16::from_be_bytes([cv[0], cv[1]]);
                    h.max_fall = u16::from_be_bytes([cv[2], cv[3]]);
                }
                b"dOps" => opus_head = Some(dops_to_opushead(cv)?),
                b"sinf" => {
                    info.encrypted = true;
                    let mut scheme = None;
                    for bx in boxes(cv) {
                        let (sty, sv) = bx?;
                        match &sty {
                            b"frma" if sv.len() >= 4 => ty.copy_from_slice(&sv[..4]),
                            b"schm" if sv.len() >= 8 => {
                                scheme = match &sv[4..8] {
                                    b"cenc" => Some(Scheme::Cenc),
                                    b"cbcs" => Some(Scheme::Cbcs),
                                    _ => None,
                                }
                            }
                            b"schi" => {
                                for bx in boxes(sv) {
                                    let (t, tv) = bx?;
                                    if &t == b"tenc" {
                                        if let Some(sc) = scheme {
                                            info.protection = Some(tenc(tv, sc)?);
                                        }
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
        if video {
            info.color = match (color, hdr) {
                (Some(c), h) => Some(crate::Color { hdr: h, ..c }),
                (None, Some(h)) => Some(crate::Color { hdr: Some(h), ..Default::default() }),
                (None, None) => None,
            };
        }
        Ok(())
    }

    fn moof(&mut self, b: &[u8], moof_start: u64, b_abs: u64) -> Result<()> {
        if !self.moov_seen {
            return Err(Error::Malformed("moof before moov"));
        }
        let mut new = Vec::new();
        let mut pssh = Vec::new();
        for bx in boxes(b) {
            let (ty, v) = bx?;
            if &ty == b"pssh" {
                push_pssh(&mut pssh, v);
            }
            if &ty != b"traf" {
                continue;
            }
            let first = new.len();
            let (mut senc, mut saiz, mut saio) = (None, None, None);
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
                                pts: to_ns(dt + cto as i128 - trk.shift as i128, trk.timescale),
                                dur: Some(to_ns(dur as i128, trk.timescale).max(0) as u64),
                                key: sf & 0x0001_0000 == 0,
                            });
                            trk.next_dts = trk.next_dts.saturating_add(dur as u64);
                            ptr = ptr.checked_add(size as u64).ok_or(Error::Malformed("sample offset"))?;
                        }
                        data_ptr = Some(ptr);
                    }
                    b"senc" => senc = Some(v),
                    b"saiz" => saiz = Some(v),
                    b"saio" => saio = Some(v),
                    b"sbgp" | b"sgpd" if v.len() >= 8 && &v[4..8] == b"seig" => return Err(Error::Unsupported("CENC key rotation (seig)")),
                    _ => {}
                }
            }
            if let Some((ti, d, base)) = tf {
                let clear = d.sdi >= 1 && d.sdi <= 64 && self.trks[ti as usize].clear_sdi & (1 << (d.sdi - 1)) != 0;
                if let (Some(p), false) = (self.tracks[ti as usize].protection.clone(), clear) {
                    // In a fragment, saio offsets count from the track fragment's base.
                    let aux = aux_info(Aux { senc, saiz, saio, outer: (b, b_abs), saio_base: base }, &p, &new[first..])?;
                    for (s, c) in new[first..].iter().zip(aux) {
                        if let Some(c) = c {
                            self.crypto.insert(s.offset, c);
                        }
                    }
                }
            }
        }
        new.sort_by_key(|s| s.offset);
        self.pending.extend(new);
        self.announce(pssh);
        Ok(())
    }
}

impl Parser {
    /// Announce a new set of `pssh` boxes as init data (once each).
    fn announce(&mut self, pssh: Vec<u8>) {
        if !pssh.is_empty() && !self.init_data.contains(&pssh) {
            self.init_data.push(pssh.clone());
            self.out.push_back(Event::InitData { kind: InitDataKind::Cenc, data: pssh });
        }
    }
}

/// Re-frame a `pssh` box body as the whole box (EME's `cenc` init data is whole boxes).
fn push_pssh(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"pssh");
    out.extend_from_slice(body);
}

/// `tenc`: the track's key id and IV defaults.
fn tenc(b: &[u8], scheme: Scheme) -> Result<Protection> {
    let mut r = Rd::new(b);
    let ver = r.u8()?;
    r.skip(3)?;
    r.skip(1)?;
    let pattern = r.u8()?;
    let (crypt, skip) = if ver >= 1 { (pattern >> 4, pattern & 0xf) } else { (0, 0) };
    let protected = r.u8()?;
    let iv_size = r.u8()?;
    let mut key_id = [0u8; 16];
    key_id.copy_from_slice(r.bytes(16)?);
    let mut constant_iv = Vec::new();
    if protected == 1 && iv_size == 0 {
        let n = r.u8()? as usize;
        if n != 8 && n != 16 {
            return Err(Error::Malformed("tenc constant IV size"));
        }
        constant_iv = r.bytes(n)?.to_vec();
    }
    if !matches!(iv_size, 0 | 8 | 16) {
        return Err(Error::Malformed("tenc IV size"));
    }
    if protected != 1 {
        return Err(Error::Unsupported("tenc: track marked unprotected"));
    }
    Ok(Protection { scheme, key_id, iv_size, constant_iv, crypt_byte_block: crypt, skip_byte_block: skip })
}

/// Where a track's (or track fragment's) sample auxiliary information is.
struct Aux<'a> {
    senc: Option<&'a [u8]>,
    saiz: Option<&'a [u8]>,
    saio: Option<&'a [u8]>,
    /// The box we hold in memory (moov or moof body) and its file offset: saio must point
    /// inside it.
    outer: (&'a [u8], u64),
    /// What saio offsets are relative to.
    saio_base: u64,
}

/// One `Crypto` per sample, in sample order: from `senc`, or from `saiz`/`saio`, or (no
/// per-sample data) the constant IV with the whole sample encrypted.
fn aux_info(a: Aux<'_>, p: &Protection, samples: &[Pending]) -> Result<Vec<Option<Box<Crypto>>>> {
    let n = samples.len();
    let make = |iv: Vec<u8>, subsamples: Vec<(u32, u32)>, size: u32| -> Result<Option<Box<Crypto>>> {
        if !subsamples.is_empty() {
            let total = subsamples.iter().map(|&(c, e)| c as u64 + e as u64).sum::<u64>();
            if total != size as u64 {
                return Err(Error::Malformed("CENC subsamples do not cover the sample"));
            }
        }
        Ok(Some(Box::new(Crypto {
            scheme: p.scheme,
            key_id: p.key_id,
            iv,
            subsamples,
            crypt_byte_block: p.crypt_byte_block,
            skip_byte_block: p.skip_byte_block,
        })))
    };
    let iv_of = |r: &mut Rd<'_>| -> Result<Vec<u8>> {
        Ok(if p.iv_size == 0 { p.constant_iv.clone() } else { r.bytes(p.iv_size as usize)?.to_vec() })
    };
    let subs_of = |r: &mut Rd<'_>| -> Result<Vec<(u32, u32)>> {
        let k = r.u16()? as usize;
        if k * 6 > r.left() {
            return Err(Error::Malformed("CENC subsample count"));
        }
        let mut v = Vec::with_capacity(k);
        for _ in 0..k {
            v.push((r.u16()? as u32, r.u32()?));
        }
        Ok(v)
    };
    let mut out = Vec::with_capacity(n);
    if let Some(senc) = a.senc {
        let mut r = Rd::new(senc);
        let flags = r.u32()? & 0xFF_FFFF;
        let count = r.u32()? as usize;
        if count != n {
            return Err(Error::Malformed("senc sample count"));
        }
        for s in samples {
            let iv = iv_of(&mut r)?;
            let subs = if flags & 0x2 != 0 { subs_of(&mut r)? } else { Vec::new() };
            out.push(make(iv, subs, s.size)?);
        }
        return Ok(out);
    }
    if let (Some(saiz), Some(saio)) = (a.saiz, a.saio) {
        let mut r = Rd::new(saiz);
        let flags = r.u32()? & 0xFF_FFFF;
        if flags & 1 != 0 {
            r.skip(8)?; // aux_info_type and parameter
        }
        let default = r.u8()? as usize;
        let count = r.u32()? as usize;
        if count != n {
            return Err(Error::Malformed("saiz sample count"));
        }
        let sizes: Vec<usize> = if default != 0 { vec![default; n] } else { r.bytes(n)?.iter().map(|&x| x as usize).collect() };
        let mut r = Rd::new(saio);
        let vf = r.u32()?;
        if vf & 1 != 0 {
            r.skip(8)?;
        }
        if r.u32()? != 1 {
            return Err(Error::Unsupported("saio with more than one offset"));
        }
        let off = if vf >> 24 == 1 { r.u64()? } else { r.u32()? as u64 };
        let abs = a.saio_base.checked_add(off).ok_or(Error::Malformed("saio offset"))?;
        let (outer, outer_abs) = a.outer;
        let start = abs.checked_sub(outer_abs).filter(|&o| o <= outer.len() as u64).ok_or(Error::Unsupported("CENC aux information outside moov/moof"))? as usize;
        let total: usize = sizes.iter().sum();
        let mut r = Rd::new(outer.get(start..start + total).ok_or(Error::Unsupported("CENC aux information outside moov/moof"))?);
        for (s, &z) in samples.iter().zip(&sizes) {
            let rec = r.bytes(z)?;
            let mut q = Rd::new(rec);
            let iv = iv_of(&mut q)?;
            let subs = if q.left() > 0 { subs_of(&mut q)? } else { Vec::new() };
            if q.left() != 0 {
                return Err(Error::Malformed("CENC aux record size"));
            }
            out.push(make(iv, subs, s.size)?);
        }
        return Ok(out);
    }
    if p.iv_size == 0 && !p.constant_iv.is_empty() {
        for s in samples {
            out.push(make(p.constant_iv.clone(), Vec::new(), s.size)?);
        }
        return Ok(out);
    }
    Err(Error::Malformed("encrypted track without CENC aux information"))
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
        // Other tracks (sound) from their sample at or before the pre-roll
        // point, which in the file can come before the video's key sample:
        // a decoder needs that much before the target to converge.
        let pre = t_ns.min(k.pts) - crate::AUDIO_PREROLL_NS;
        let mut from: Vec<(u8, u64)> = Vec::new();
        for t in self.tracks.iter().filter(|t| t.index != vt) {
            let first = self.index.iter().filter(|s| s.track == t.index && s.pts <= pre).max_by_key(|s| s.pts)
                .or_else(|| self.index.iter().filter(|s| s.track == t.index).min_by_key(|s| s.pts));
            if let Some(f) = first {
                from.push((t.index, f.offset));
            }
        }
        let start = |s: &Pending| if s.track == vt { k.offset } else { from.iter().find(|f| f.0 == s.track).map_or(k.offset, |f| f.1) };
        self.pending = self.index.iter().filter(|s| s.offset >= start(s)).copied().collect();
        self.out.clear();
        self.want = Some(from.iter().map(|f| f.1).fold(k.offset, u64::min));
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

    fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(t);
        v.extend_from_slice(body);
        v
    }

    /// A clear lead: `stsd` holds `encv` (1) then `avc1` (2); fragments using 2 are clear.
    #[test]
    fn clear_sample_entries() {
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 2];
        stsd.extend(bx(b"encv", &[0; 8]));
        stsd.extend(bx(b"avc1", &[0; 8]));
        let trak = bx(b"mdia", &bx(b"minf", &bx(b"stbl", &bx(b"stsd", &stsd))));
        assert_eq!(clear_entries(&trak), 0b10);
        // No encrypted entry: nothing to tell apart.
        let mut plain = vec![0, 0, 0, 0, 0, 0, 0, 1];
        plain.extend(bx(b"avc1", &[0; 8]));
        let trak = bx(b"mdia", &bx(b"minf", &bx(b"stbl", &bx(b"stsd", &plain))));
        assert_eq!(clear_entries(&trak), 0);
    }
}
