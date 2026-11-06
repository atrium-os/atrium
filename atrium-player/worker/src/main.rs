//! atrium-player-decode: the decode worker (docs/spec/atrium-player.md §2).
//!
//! Started by the supervisor with one socket (`--fd N`). Before it reads a
//! single byte from it, the worker caps its memory and enters Capsicum
//! capability mode: from then on it can open no file, socket or process,
//! and holds only what the supervisor passes it (the socket, a read-only
//! input for `Load`, the layer buffers and the PCM ring). Everything it
//! parses or decodes is hostile.
#![deny(unsafe_code)]

mod store;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use atrium_media_decode as decode;
use atrium_media_demux::{self as demux, Codec, Demuxer, Event, Kind, TrackInfo};
use atrium_player::ring::PcmRing;
use atrium_player::sys::{self, Map};
use atrium_player::wire::{decode_frame, err, Format, Limits, Mode, Record, ToWorker};
use store::Store;

/// Read ahead of the decode cursor in `Load` mode.
const AHEAD_NS: i64 = 3_000_000_000;
const READ_CHUNK: usize = 256 << 10;
/// Decode audio only while the ring has room for a whole packet.
const PCM_PACKET_MAX: u64 = 8192;

fn codec_id(c: Codec) -> u32 {
    match c {
        Codec::Unknown => 0,
        Codec::Av1 => 1,
        Codec::Vp9 => 2,
        Codec::Vp8 => 3,
        Codec::H264 => 4,
        Codec::Opus => 5,
        Codec::Vorbis => 6,
        Codec::Aac => 7,
    }
}

/// One byte stream: the whole file, or one MSE track's appends.
struct Feed {
    parser: Option<Box<dyn Demuxer + Send>>,
    mime: String,
    tracks: Vec<TrackInfo>,
    offset_ns: i64,
    /// Which of this feed's demuxed tracks are the chosen video / audio.
    video: Option<u8>,
    audio: Option<u8>,
}

struct Video {
    track: TrackInfo,
    dec: Box<dyn decode::VideoDecoder>,
    store: Store,
    feed: usize,
    buffers: Vec<Map>,
    free: VecDeque<u32>,
    format: Format,
    dims: (u32, u32),
    drained: bool,
    /// The first picture has been decoded to learn the bit depth.
    probed: bool,
    drain_tries: u8,
}

struct Audio {
    dec: Box<dyn decode::AudioDecoder>,
    store: Store,
    feed: usize,
    ring: Option<PcmRing>,
    pcm: Vec<f32>,
    /// Media time of `pcm[0]`.
    pcm_pts: i64,
    drop_before: Option<i64>,
}

struct Worker {
    sock: UnixStream,
    limits: Limits,
    mode: Option<Mode>,
    input: Option<File>,
    input_eof: bool,
    feeds: Vec<(u8, Feed)>,
    video: Option<Video>,
    audio: Option<Audio>,
    epoch: u32,
    ended_sent: bool,
    end_of_stream: bool,
    failed: bool,
    /// A seek that came before the audio track did (a restarted worker).
    pending_drop: Option<i64>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut fd = None;
    let mut mem: u64 = 768 << 20;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fd" => fd = args.next().and_then(|v| v.parse::<i32>().ok()),
            "--mem-mib" => mem = args.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(768) << 20,
            _ => {}
        }
    }
    let Some(fd) = fd else {
        eprintln!("atrium-player-decode: started without --fd; it is run by atrium-player");
        std::process::exit(2);
    };
    #[allow(unsafe_code)]
    // SAFETY: the supervisor gave us this descriptor for exactly this.
    let sock = UnixStream::from(unsafe { sys::adopt(fd) });
    // The socket can only carry messages: no ioctl, fcntl or shutdown games.
    if let Err(e) = sys::limit(&std::os::fd::OwnedFd::from(sock.try_clone().unwrap()), sys::Rights::Socket) {
        eprintln!("atrium-player-decode: cap_rights_limit: {e}");
        std::process::exit(1);
    }
    if let Err(e) = sys::limit_resources(mem) {
        eprintln!("atrium-player-decode: rlimit: {e}");
        std::process::exit(1);
    }
    let sandboxed = match sys::enter_sandbox() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("atrium-player-decode: cap_enter: {e}");
            std::process::exit(1);
        }
    };
    let mut w = Worker {
        sock,
        limits: Limits::default(),
        mode: None,
        input: None,
        input_eof: false,
        feeds: Vec::new(),
        video: None,
        audio: None,
        epoch: 0,
        ended_sent: false,
        end_of_stream: false,
        failed: false,
        pending_drop: None,
    };
    if w.send(Record::Ready { sandboxed }).is_err() {
        return;
    }
    w.run();
}

impl Worker {
    fn send(&mut self, r: Record) -> std::io::Result<()> {
        self.sock.write_all(&r.encode())
    }

    fn fail(&mut self, code: u32) {
        if !self.failed {
            self.failed = true;
            let _ = self.send(Record::Error { code });
        }
    }

    fn run(&mut self) {
        loop {
            let busy = !self.failed && self.work();
            // Block for the next message only when there's nothing to do.
            if busy && !sys::readable(&self.sock, 0) {
                continue;
            }
            match sys::recv_frame(&self.sock) {
                Ok(Some((payload, fds))) => {
                    let Some(msg) = decode_frame(&payload) else { return };
                    if !self.handle(msg, fds) {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
    }

    /// Returns false to exit.
    fn handle(&mut self, msg: ToWorker, mut fds: Vec<OwnedFd>) -> bool {
        match msg {
            ToWorker::Init { mode, limits } => {
                self.limits = limits;
                if mode == Mode::Load {
                    let Some(fd) = fds.pop() else { self.fail(err::STATE); return true };
                    let mut f = File::from(fd);
                    // A restarted worker shares the file offset with the last one.
                    if f.seek(SeekFrom::Start(0)).is_err() {
                        self.fail(err::STATE);
                        return true;
                    }
                    self.input = Some(f);
                    self.feeds.push((0, Feed { parser: None, mime: String::new(), tracks: Vec::new(), offset_ns: 0, video: None, audio: None }));
                }
                self.mode = Some(mode);
            }
            ToWorker::AddTrack { track, mime } => {
                if self.mode != Some(Mode::Feed) || self.feeds.iter().any(|(t, _)| *t == track) {
                    self.fail(err::STATE);
                } else {
                    self.feeds.push((track, Feed { parser: None, mime, tracks: Vec::new(), offset_ns: 0, video: None, audio: None }));
                }
            }
            ToWorker::Append { track, bytes, timestamp_offset_ns } => {
                self.append(track, &bytes, timestamp_offset_ns);
                let _ = self.send(Record::Appended { track });
            }
            ToWorker::Remove { track, start_ns, end_ns } => {
                if let Some(fi) = self.feed_index(track) {
                    if let Some(v) = self.video.as_mut().filter(|v| v.feed == fi) {
                        v.store.remove(start_ns, end_ns);
                    }
                    if let Some(a) = self.audio.as_mut().filter(|a| a.feed == fi) {
                        a.store.remove(start_ns, end_ns);
                    }
                    self.report_buffered(track);
                }
            }
            ToWorker::Abort { track } => {
                if let Some(fi) = self.feed_index(track) {
                    if let Some(p) = self.feeds[fi].1.parser.as_mut() {
                        p.abort();
                    }
                }
            }
            ToWorker::EndOfStream => self.end_of_stream = true,
            ToWorker::Buffers { format, width, height, count } => self.buffers(format, width, height, count, fds),
            ToWorker::PcmRing => {
                if let (Some(a), Some(fd)) = (self.audio.as_mut(), fds.pop()) {
                    match PcmRing::open(&fd, a.dec.rate(), a.dec.channels()) {
                        Ok(r) => a.ring = Some(r),
                        Err(_) => self.fail(err::STATE),
                    }
                }
            }
            ToWorker::Release { index } => {
                if let Some(v) = self.video.as_mut() {
                    if (index as usize) < v.buffers.len() && !v.free.contains(&index) {
                        v.free.push_back(index);
                    }
                }
            }
            ToWorker::Seek { t_ns, epoch } => self.seek(t_ns, epoch),
            ToWorker::ProbeSandbox => {
                let refused = sys::probe_sandbox();
                let _ = self.send(Record::Probe { refused });
            }
            ToWorker::Shutdown => return false,
        }
        true
    }

    fn feed_index(&self, track: u8) -> Option<usize> {
        self.feeds.iter().position(|(t, _)| *t == track)
    }

    fn append(&mut self, track: u8, bytes: &[u8], offset: Option<i64>) {
        let Some(fi) = self.feed_index(track) else { return self.fail(err::STATE) };
        let held: u64 = self.video.as_ref().map_or(0, |v| v.store.bytes()) + self.audio.as_ref().map_or(0, |a| a.store.bytes());
        if held + bytes.len() as u64 > self.limits.feed_bytes {
            let _ = self.send(Record::Error { code: err::TOO_BIG });
            return;
        }
        if let Some(o) = offset {
            self.feeds[fi].1.offset_ns = o;
        }
        self.push_bytes(fi, bytes);
        self.report_buffered(track);
    }

    /// Push bytes into a feed's parser and route what comes out.
    fn push_bytes(&mut self, fi: usize, bytes: &[u8]) {
        let feed = &mut self.feeds[fi].1;
        if feed.parser.is_none() {
            let fmt = demux::probe(bytes).or(if feed.mime.contains("mp4") { Some(demux::Format::Mp4) } else if feed.mime.contains("webm") { Some(demux::Format::Webm) } else { None });
            let Some(fmt) = fmt else { return self.fail(err::UNSUPPORTED) };
            let lim = demux::Limits { max_width: self.limits.max_width, max_height: self.limits.max_height, ..demux::Limits::default() };
            feed.parser = Some(demux::open(fmt, lim));
        }
        feed.parser.as_mut().unwrap().push(bytes);
        self.drain_parser(fi);
    }

    fn drain_parser(&mut self, fi: usize) {
        loop {
            let ev = match self.feeds[fi].1.parser.as_mut().unwrap().poll() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(demux::Error::Unsupported(_)) => return self.fail(err::UNSUPPORTED),
                Err(demux::Error::TooBig(_)) => return self.fail(err::TOO_BIG),
                Err(_) => return self.fail(err::DECODE),
            };
            match ev {
                Event::Tracks(t) => self.tracks(fi, t),
                Event::Duration(secs) => {
                    let _ = self.send(Record::Duration { secs });
                }
                Event::Sample(mut s) => {
                    let feed = &self.feeds[fi].1;
                    s.pts_ns = s.pts_ns.saturating_add(feed.offset_ns);
                    s.dts_ns = s.dts_ns.saturating_add(feed.offset_ns);
                    if feed.video == Some(s.track) {
                        if let Some(v) = self.video.as_mut() {
                            v.store.push(s);
                        }
                    } else if feed.audio == Some(s.track) {
                        if let Some(a) = self.audio.as_mut() {
                            a.store.push(s);
                        }
                    }
                }
            }
        }
        // A plain MP4 wants its moov, or a seek wants an offset.
        let want = self.feeds[fi].1.parser.as_ref().unwrap().wants_offset();
        if let Some(off) = want {
            if let Some(f) = self.input.as_mut() {
                if f.seek(SeekFrom::Start(off)).is_ok() {
                    self.feeds[fi].1.parser.as_mut().unwrap().input_at(off);
                    self.input_eof = false;
                    return;
                }
            }
            self.fail(err::UNSUPPORTED);
        }
    }

    fn tracks(&mut self, fi: usize, tracks: Vec<TrackInfo>) {
        let feed = &mut self.feeds[fi].1;
        feed.tracks = tracks.clone();
        if self.video.is_none() {
            if let Some(t) = tracks.iter().find(|t| t.codec.is_video()) {
                let cfg = decode::Config { threads: self.limits.threads, max_width: self.limits.max_width, max_height: self.limits.max_height };
                match decode::video(t, &cfg) {
                    Ok(dec) => {
                        let Kind::Video { width, height } = t.kind else { return };
                        feed.video = Some(t.index);
                        // The track is announced once its first picture says
                        // its bit depth (see `probe_video`).
                        self.video = Some(Video { track: t.clone(), dec, store: Store::default(), feed: fi, buffers: Vec::new(), free: VecDeque::new(), format: Format::Nv12, dims: (width, height), drained: false, probed: false, drain_tries: 0 });
                    }
                    Err(decode::Error::TooBig) => return self.fail(err::TOO_BIG),
                    Err(_) => return self.fail(err::UNSUPPORTED),
                }
            }
        }
        let feed = &mut self.feeds[fi].1;
        if self.audio.is_none() {
            if let Some(t) = tracks.iter().find(|t| matches!(t.kind, Kind::Audio { .. })) {
                if let Ok(dec) = decode::audio(t) {
                    feed.audio = Some(t.index);
                    let (rate, channels) = (dec.rate(), dec.channels());
                    self.audio = Some(Audio { dec, store: Store::default(), feed: fi, ring: None, pcm: Vec::new(), pcm_pts: 0, drop_before: self.pending_drop.take() });
                    let _ = self.send(Record::AudioTrack { track: self.feeds[fi].0, codec: codec_id(t.codec), rate, channels });
                } else if self.video.is_none() {
                    return self.fail(err::UNSUPPORTED);
                }
            }
        }
        if self.video.is_none() && self.audio.is_none() {
            self.fail(err::UNSUPPORTED);
        }
    }

    fn buffers(&mut self, format: Format, width: u32, height: u32, count: u32, fds: Vec<OwnedFd>) {
        let Some(v) = self.video.as_mut() else { return self.fail(err::STATE) };
        if fds.len() != count as usize || count == 0 || count > atrium_player::wire::BUFFERS_MAX {
            return self.fail(err::STATE);
        }
        let len = format.buffer_size(width, height);
        let mut maps = Vec::new();
        for fd in &fds {
            match Map::new(fd, len) {
                Ok(m) => maps.push(m),
                Err(_) => return self.fail(err::STATE),
            }
        }
        v.buffers = maps;
        v.free = (0..count).collect();
        v.format = format;
        v.dims = (width, height);
    }

    fn report_buffered(&mut self, track: u8) {
        let Some(fi) = self.feed_index(track) else { return };
        let mut ranges = Vec::new();
        if let Some(v) = self.video.as_ref().filter(|v| v.feed == fi) {
            ranges = v.store.ranges();
        }
        if let Some(a) = self.audio.as_ref().filter(|a| a.feed == fi) {
            let ar = a.store.ranges();
            ranges = if ranges.is_empty() { ar } else { store::intersect(&ranges, &ar) };
        }
        ranges.truncate(64);
        let n = ranges.len() as u8;
        if n == 0 {
            let _ = self.send(Record::Buffered { track, i: 0, n: 0, start_ns: 0, end_ns: 0 });
        }
        for (i, (s, e)) in ranges.into_iter().enumerate() {
            let _ = self.send(Record::Buffered { track, i: i as u8, n, start_ns: s, end_ns: e });
        }
    }

    fn seek(&mut self, t_ns: i64, epoch: u32) {
        self.epoch = epoch;
        self.ended_sent = false;
        if self.audio.is_none() {
            self.pending_drop = Some(t_ns);
        }
        let mut in_store = true;
        if let Some(v) = self.video.as_mut() {
            v.dec.flush();
            v.drained = false;
            v.drain_tries = 0;
            in_store &= v.store.seek(t_ns);
        }
        if let Some(a) = self.audio.as_mut() {
            a.dec.reset();
            a.pcm.clear();
            // Decode from 80 ms before the target and drop that (RFC 7845 §4.6).
            in_store &= a.store.seek(t_ns - 80_000_000);
            a.drop_before = Some(t_ns);
        }
        if !in_store && self.mode == Some(Mode::Load) {
            if let Some(v) = self.video.as_mut() {
                v.store.clear();
            }
            if let Some(a) = self.audio.as_mut() {
                a.store.clear();
            }
            self.input_eof = false;
            // Ask the demuxer where to read from (cues, the sample table).
            // Without an index it reads again from the start, and frames
            // before the target are decoded and dropped.
            let indexed = self.feeds[0].1.parser.as_mut().is_some_and(|p| p.seek(t_ns));
            if indexed {
                self.drain_parser(0);
            } else if let Some(f) = self.input.as_mut() {
                if f.seek(SeekFrom::Start(0)).is_ok() {
                    // A fresh parser; its Tracks event finds the decoders in place.
                    self.feeds[0].1.parser = None;
                } else {
                    self.fail(err::DECODE);
                }
            }
        }
        let pos = self.audio.as_ref().and_then(|a| a.ring.as_ref()).map_or(0, |r| r.write_pos());
        let _ = self.send(Record::Seeked { epoch, pos });
    }

    /// One round of work. Returns whether there is more to do right away.
    fn work(&mut self) -> bool {
        let mut busy = false;
        if self.mode == Some(Mode::Load) && !self.input_eof && self.needs_input() {
            let mut buf = vec![0u8; READ_CHUNK];
            match self.input.as_mut().map(|f| f.read(&mut buf)) {
                Some(Ok(0)) => {
                    self.input_eof = true;
                    if let Some(p) = self.feeds[0].1.parser.as_mut() {
                        if p.finish().is_err() {
                            // A truncated file plays what it has.
                        }
                    }
                }
                Some(Ok(n)) => {
                    self.push_bytes(0, &buf[..n]);
                    busy = true;
                }
                _ => self.fail(err::DECODE),
            }
        }
        busy |= self.work_video();
        busy |= self.work_audio();
        if !busy && !self.ended_sent && self.all_done() {
            self.ended_sent = true;
            let _ = self.send(Record::Ended);
        }
        busy && !self.failed
    }

    fn needs_input(&self) -> bool {
        let ahead = |s: &Store| s.ahead_ns() < AHEAD_NS;
        self.video.as_ref().map_or(false, |v| ahead(&v.store)) || self.audio.as_ref().map_or(false, |a| ahead(&a.store)) || (self.video.is_none() && self.audio.is_none())
    }

    fn input_done(&self) -> bool {
        match self.mode {
            Some(Mode::Load) => self.input_eof,
            Some(Mode::Feed) => self.end_of_stream,
            None => false,
        }
    }

    fn all_done(&self) -> bool {
        self.input_done()
            && self.video.as_ref().map_or(true, |v| v.drained)
            && self.audio.as_ref().map_or(true, |a| a.store.ahead() == 0 && a.pcm.is_empty())
    }

    fn work_video(&mut self) -> bool {
        let input_done = self.input_done();
        let epoch = self.epoch;
        if self.video.as_ref().is_some_and(|v| !v.probed) {
            return self.probe_video();
        }
        let Some(v) = self.video.as_mut() else { return false };
        if v.buffers.is_empty() || v.drained {
            return false;
        }
        let mut did = false;
        let mut out = Vec::new();
        let mut fail = None;
        while let Some(&idx) = v.free.front() {
            match v.dec.receive() {
                Ok(Some(pic)) => {
                    let (w, h) = Format::layer_dims(v.dims.0, v.dims.1);
                    if pic.width > w || pic.height > h {
                        // A stream that changed size: the supervisor needs new buffers.
                        fail = Some(err::UNSUPPORTED);
                        break;
                    }
                    let bps = v.format.bytes_per_sample();
                    let buf = v.buffers[idx as usize].bytes_mut();
                    let ylen = (w * h) as usize * bps;
                    let (y, uv) = buf.split_at_mut(ylen);
                    let stride = w as usize * bps;
                    let mut yp = decode::convert::PlaneMut { data: y, stride };
                    let mut uvp = decode::convert::PlaneMut { data: uv, stride };
                    let r = match v.format {
                        Format::Nv12 => decode::convert::to_nv12(&pic, &mut yp, &mut uvp),
                        Format::P010 => decode::convert::to_p010(&pic, &mut yp, &mut uvp),
                    };
                    let pts = pic.pts_ns;
                    if r.is_err() {
                        fail = Some(err::DECODE);
                        break;
                    }
                    v.free.pop_front();
                    v.drain_tries = 0;
                    out.push(Record::Frame { index: idx, epoch, pts_ns: pts });
                    did = true;
                }
                Ok(None) => {
                    if let Some(s) = v.store.next() {
                        match v.dec.send(&s.data, s.pts_ns) {
                            Ok(true) => v.store.advance(),
                            Ok(false) => {}
                            Err(decode::Error::TooBig) => {
                                fail = Some(err::TOO_BIG);
                                break;
                            }
                            Err(_) => {
                                fail = Some(err::DECODE);
                                break;
                            }
                        }
                        did = true;
                    } else {
                        // dav1d hands out its last pictures only to a get
                        // that follows a get with no send between them.
                        if input_done {
                            v.drain_tries += 1;
                            if v.drain_tries < 3 {
                                continue;
                            }
                            v.drained = true;
                        }
                        break;
                    }
                }
                Err(decode::Error::TooBig) => {
                    fail = Some(err::TOO_BIG);
                    break;
                }
                Err(_) => {
                    fail = Some(err::DECODE);
                    break;
                }
            }
        }
        if self.mode == Some(Mode::Load) {
            v.store.evict_played();
        }
        for r in out {
            let _ = self.send(r);
        }
        if let Some(code) = fail {
            self.fail(code);
        }
        did
    }

    /// Decode the first picture to learn its real size and bit depth,
    /// announce the track, then start over from the first sample.
    fn probe_video(&mut self) -> bool {
        let fi_track = self.video.as_ref().map(|v| self.feeds[v.feed].0).unwrap_or(0);
        let input_done = self.input_done();
        let v = self.video.as_mut().unwrap();
        let mut found = None;
        let mut fail = None;
        loop {
            match v.dec.receive() {
                Ok(Some(p)) => {
                    found = Some((p.width, p.height, p.bit_depth));
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    fail = Some(err::DECODE);
                    break;
                }
            }
            let Some(s) = v.store.next() else { break };
            match v.dec.send(&s.data, s.pts_ns) {
                Ok(true) => v.store.advance(),
                Ok(false) => {}
                Err(decode::Error::TooBig) => {
                    fail = Some(err::TOO_BIG);
                    break;
                }
                Err(_) => {
                    fail = Some(err::DECODE);
                    break;
                }
            }
        }
        if let Some(c) = fail {
            self.fail(c);
            return false;
        }
        let Some((w, h, depth)) = found else {
            if input_done && v.store.next().is_none() {
                self.fail(err::DECODE);
            }
            return false;
        };
        v.dec.flush();
        v.store.rewind();
        v.probed = true;
        v.dims = (w, h);
        let codec = codec_id(v.track.codec);
        let _ = self.send(Record::VideoTrack { track: fi_track, codec, width: w, height: h, bit_depth: depth });
        true
    }

    fn work_audio(&mut self) -> bool {
        let epoch = self.epoch;
        let Some(a) = self.audio.as_mut() else { return false };
        let Some(ring) = a.ring.as_mut() else { return false };
        let mut did = false;
        let mut out = Vec::new();
        loop {
            if !a.pcm.is_empty() {
                let ch = ring.channels();
                let pos = ring.write_pos();
                let n = ring.write(&a.pcm);
                if n == 0 {
                    break;
                }
                out.push(Record::Pcm { epoch, pts_ns: a.pcm_pts, frames: n as u32, pos });
                a.pcm.drain(..n * ch);
                a.pcm_pts += n as i64 * 1_000_000_000 / a.dec.rate() as i64;
                did = true;
                continue;
            }
            if ring.free() < PCM_PACKET_MAX {
                break;
            }
            let Some(s) = a.store.next() else { break };
            let (pts, data) = (s.pts_ns, s.data.clone());
            a.store.advance();
            if a.dec.decode(&data, &mut a.pcm).is_err() {
                // One bad packet is a gap, not the end of playback.
                a.pcm.clear();
                continue;
            }
            a.pcm_pts = pts;
            if let Some(t) = a.drop_before {
                let rate = a.dec.rate() as i64;
                let ch = a.dec.channels() as usize;
                let frames = (a.pcm.len() / ch) as i64;
                let skip = ((t - pts) * rate / 1_000_000_000).clamp(0, frames);
                a.pcm.drain(..skip as usize * ch);
                a.pcm_pts = pts + skip * 1_000_000_000 / rate;
                if skip < frames {
                    a.drop_before = None;
                }
            }
            did = true;
        }
        if self.mode == Some(Mode::Load) {
            a.store.evict_played();
        }
        for r in out {
            let _ = self.send(r);
        }
        did
    }
}
