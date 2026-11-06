//! The worker's lifecycle and the player's state, independent of where
//! frames go ([`Sink`]): Fresco's video layer in the real player, a hash in
//! the headless one.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use crate::ring::PcmRing;
use crate::sys;
use crate::wire::{encode_frame, err, Format, Limits, Mode, Record, ToWorker, BUFFERS_MAX, RECORD_LEN};

/// How long the worker may go silent while it owes us something.
pub const WATCHDOG: Duration = Duration::from_secs(5);

/// Where decoded frames go.
pub trait Sink {
    /// Buffers for a video layer of this format and (even) size.
    fn layer(&mut self, format: Format, width: u32, height: u32, count: u32) -> io::Result<Vec<OwnedFd>>;
    /// Buffer `index` holds the frame for media time `pts_ns`. Return true if
    /// the sink is done with it at once; otherwise it hands the buffer back
    /// later through [`Player::release`].
    fn frame(&mut self, index: u32, pts_ns: i64) -> bool;
    /// Media time jumps or stood still (a seek, or play after pause): the
    /// next frame starts the clock again.
    fn discontinuity(&mut self) {}
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    VideoTrack { width: u32, height: u32, bit_depth: u8 },
    AudioTrack { rate: u32, channels: u16 },
    Duration(f64),
    Frame { pts_ns: i64 },
    Appended { track: u8 },
    Buffered { track: u8, ranges: Vec<(i64, i64)> },
    Ended,
    /// A `wire::err` code. Fatal for this feed.
    Error(u32),
    /// The worker died and a fresh one took over.
    Restarted,
    Probe { refused: u32 },
    Sandboxed(bool),
}

#[derive(Clone)]
pub struct Options {
    pub worker: PathBuf,
    pub limits: Limits,
    pub buffers: u32,
    pub worker_mem_mib: u64,
}

impl Options {
    /// `atrium-player-decode` next to this executable, unless
    /// `ATRIUM_PLAYER_DECODE` says otherwise.
    pub fn default_worker() -> PathBuf {
        if let Some(p) = std::env::var_os("ATRIUM_PLAYER_DECODE") {
            return p.into();
        }
        let exe = std::env::current_exe().unwrap_or_default();
        exe.with_file_name("atrium-player-decode")
    }
}

impl Default for Options {
    fn default() -> Self {
        Options { worker: Self::default_worker(), limits: Limits::default(), buffers: 4, worker_mem_mib: 768 }
    }
}

struct Proc {
    child: Child,
    sock: UnixStream,
    /// Bytes of a record read so far.
    partial: Vec<u8>,
}

impl Proc {
    fn spawn(o: &Options) -> io::Result<Proc> {
        let (mine, theirs) = UnixStream::pair()?;
        let mut cmd = Command::new(&o.worker);
        cmd.arg("--fd").arg(theirs.as_raw_fd().to_string()).arg("--mem-mib").arg(o.worker_mem_mib.to_string()).stdin(std::process::Stdio::null());
        sys::pass_fd(&mut cmd, theirs.as_raw_fd());
        // ETXTBSY: a worker binary just written can be briefly busy while
        // another thread's fork still holds it open for writing.
        let mut tries = 0;
        let child = loop {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                r => break r?,
            }
        };
        drop(theirs);
        Ok(Proc { child, sock: mine, partial: Vec::new() })
    }

    fn send(&self, m: &ToWorker, fds: &[&OwnedFd]) -> io::Result<()> {
        let raw: Vec<_> = fds.iter().map(|f| f.as_raw_fd()).collect();
        sys::send_frame(&self.sock, &encode_frame(m), &raw)
    }

    /// The next record, waiting at most `timeout`. `Ok(None)` on timeout.
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<[u8; RECORD_LEN]>> {
        self.sock.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        while self.partial.len() < RECORD_LEN {
            let mut b = [0u8; RECORD_LEN];
            match (&self.sock).read(&mut b[..RECORD_LEN - self.partial.len()]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "worker gone")),
                Ok(n) => self.partial.extend_from_slice(&b[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        let mut r = [0u8; RECORD_LEN];
        r.copy_from_slice(&self.partial);
        self.partial.clear();
        Ok(Some(r))
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = (&self.sock).write_all(&[]);
        self.kill();
    }
}

struct VideoState {
    format: Format,
    width: u32,
    height: u32,
    count: u32,
    held: Vec<bool>,
}

struct AudioState {
    ring: Option<PcmRing>,
    rate: u32,
    channels: u16,
    /// (ring position, media time) of each PCM run, oldest first.
    runs: VecDeque<(u64, i64)>,
    /// Ring position up to which PCM records have arrived.
    known_until: u64,
}

pub enum Source {
    /// A whole resource; the supervisor keeps it to restart a worker.
    File(File),
    /// A segment feed with these tracks (MSE source buffers).
    Feed(Vec<(u8, String)>),
}

pub struct Player<S: Sink> {
    opts: Options,
    pub sink: S,
    source: Source,
    w: Option<Proc>,
    restarts: u32,
    video: Option<VideoState>,
    audio: Option<AudioState>,
    epoch: u32,
    /// Media time of the last frame handed to the sink.
    last_pts: Option<i64>,
    /// Frames before this media time are dropped (after a seek).
    show_from: Option<i64>,
    events: VecDeque<PlayerEvent>,
    /// The worker owes us records (we're waiting for frames or appends).
    waiting: bool,
    /// When the worker last sent anything.
    heard: std::time::Instant,
    ended: bool,
    dead: bool,
    buffered: Vec<(u8, Vec<(i64, i64)>)>,
    /// Media time just after the last PCM read.
    audio_read_until: Option<i64>,
    /// After a restart: PCM before this media time was already played.
    audio_skip_until: Option<i64>,
    paused: bool,
    /// Frames decoded while paused, held until play.
    queued: VecDeque<(u32, i64)>,
    /// Paused and seeked: show the first frame there anyway.
    show_one: bool,
}

impl<S: Sink> Player<S> {
    pub fn new(opts: Options, sink: S, source: Source) -> io::Result<Self> {
        let mut p = Player {
            opts,
            sink,
            source,
            w: None,
            restarts: 0,
            video: None,
            audio: None,
            epoch: 0,
            last_pts: None,
            show_from: None,
            events: VecDeque::new(),
            waiting: true,
            heard: std::time::Instant::now(),
            ended: false,
            dead: false,
            buffered: Vec::new(),
            audio_read_until: None,
            audio_skip_until: None,
            paused: false,
            queued: VecDeque::new(),
            show_one: false,
        };
        p.start_worker()?;
        Ok(p)
    }

    fn start_worker(&mut self) -> io::Result<()> {
        let w = Proc::spawn(&self.opts)?;
        match &self.source {
            Source::File(f) => {
                let fd: OwnedFd = f.try_clone()?.into();
                let fd = sys::restricted(&fd, sys::Rights::Input)?;
                w.send(&ToWorker::Init { mode: Mode::Load, limits: self.opts.limits.clone() }, &[&fd])?;
            }
            Source::Feed(tracks) => {
                w.send(&ToWorker::Init { mode: Mode::Feed, limits: self.opts.limits.clone() }, &[])?;
                for (t, mime) in tracks {
                    w.send(&ToWorker::AddTrack { track: *t, mime: mime.clone() }, &[])?;
                }
            }
        }
        self.w = Some(w);
        self.video = None;
        self.audio = None;
        self.heard = std::time::Instant::now();
        self.waiting = true;
        Ok(())
    }

    fn send(&mut self, m: ToWorker) {
        let ok = self.w.as_ref().map(|w| w.send(&m, &[]).is_ok()).unwrap_or(false);
        if !ok {
            self.worker_lost();
        }
    }

    pub fn append(&mut self, track: u8, bytes: Vec<u8>, timestamp_offset_ns: Option<i64>) {
        self.waiting = true;
        self.send(ToWorker::Append { track, bytes, timestamp_offset_ns });
    }

    pub fn remove(&mut self, track: u8, start_ns: i64, end_ns: i64) {
        self.send(ToWorker::Remove { track, start_ns, end_ns });
    }

    pub fn abort(&mut self, track: u8) {
        self.send(ToWorker::Abort { track });
    }

    pub fn end_of_stream(&mut self) {
        self.send(ToWorker::EndOfStream);
    }

    pub fn probe_sandbox(&mut self) {
        self.waiting = true;
        self.send(ToWorker::ProbeSandbox);
    }

    /// The sink is done with buffer `index`.
    pub fn release(&mut self, index: u32) {
        if let Some(v) = self.video.as_mut() {
            if let Some(h) = v.held.get_mut(index as usize) {
                if *h {
                    *h = false;
                    // A whole resource always has a next frame until it ends.
                    if matches!(self.source, Source::File(_)) && !self.ended {
                        if !self.waiting {
                            self.heard = std::time::Instant::now();
                        }
                        self.waiting = true;
                    }
                    self.send(ToWorker::Release { index });
                }
            }
        }
    }

    /// A new track in a segment feed (MSE `addSourceBuffer`).
    pub fn add_track(&mut self, track: u8, mime: String) {
        if let Source::Feed(tracks) = &mut self.source {
            tracks.push((track, mime.clone()));
        }
        self.send(ToWorker::AddTrack { track, mime });
    }

    /// Show the next frame even if paused (a poster, or a seek's target).
    pub fn show_next(&mut self) {
        self.show_one = true;
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Pause holds decoded frames back (the worker stops once its buffers
    /// are all queued here); play hands them on with the clock restarted.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        self.show_one = false;
        if !paused {
            self.sink.discontinuity();
            while let Some((i, pts)) = self.queued.pop_front() {
                self.present(i, pts);
            }
        }
    }

    /// Media time of the frame last handed to the sink (or of the last PCM
    /// run, for audio only).
    pub fn position_ns(&self) -> Option<i64> {
        self.last_pts
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    fn present(&mut self, index: u32, pts_ns: i64) {
        self.last_pts = Some(pts_ns);
        if self.sink.frame(index, pts_ns) {
            self.release(index);
        }
        self.events.push_back(PlayerEvent::Frame { pts_ns });
    }

    fn drop_queued(&mut self) {
        while let Some((i, _)) = self.queued.pop_front() {
            self.release(i);
        }
    }

    pub fn seek(&mut self, t_ns: i64) {
        self.drop_queued();
        // PCM still in the ring is from before the seek: none of it is
        // readable until the worker's new PCM records say what is.
        if let Some(a) = self.audio.as_mut() {
            if let Some(r) = a.ring.as_ref() {
                a.known_until = r.read_pos();
            }
            a.runs.clear();
        }
        self.sink.discontinuity();
        self.show_one = self.paused;
        self.epoch += 1;
        self.ended = false;
        self.show_from = Some(t_ns);
        // The worker owes `Seeked` at once; frames too for a whole resource.
        // A feed's frames wait on the parent's appends, however slow the
        // network: that is not a hung worker.
        self.heard = std::time::Instant::now();
        self.waiting = true;
        let epoch = self.epoch;
        self.send(ToWorker::Seek { t_ns, epoch });
        // Everything the sink holds is stale; hand it back.
        let held: Vec<u32> = self.video.as_ref().map(|v| v.held.iter().enumerate().filter(|(_, h)| **h).map(|(i, _)| i as u32).collect()).unwrap_or_default();
        for i in held {
            self.release(i);
        }
    }

    pub fn video_format(&self) -> Option<(Format, u32, u32, u32)> {
        self.video.as_ref().map(|v| (v.format, v.width, v.height, v.count))
    }

    /// Read decoded audio (interleaved), with the media time of its first frame.
    pub fn read_pcm(&mut self, out: &mut [f32]) -> (usize, Option<i64>) {
        let Some(a) = self.audio.as_mut() else { return (0, None) };
        let Some(ring) = a.ring.as_mut() else { return (0, None) };
        let pos = ring.read_pos();
        // Media time at `pos`, from the run it falls in.
        while a.runs.len() > 1 && a.runs[1].0 <= pos {
            a.runs.pop_front();
        }
        let mut t = a.runs.front().filter(|r| r.0 <= pos).map(|&(p, t)| t + ((pos - p) as i64 * 1_000_000_000 / a.rate as i64));
        // Read only what a PCM record has told us the media time of.
        let mut known = a.known_until.saturating_sub(pos) as usize;
        // After a restart, skip what was already played.
        if let (Some(until), Some(now)) = (self.audio_skip_until, t) {
            if now < until {
                let skip = (((until - now) as i128 * a.rate as i128 / 1_000_000_000) as usize).min(known);
                if ring.skip_to(pos + skip as u64).is_err() {
                    self.worker_lost();
                    return (0, None);
                }
                known -= skip;
                t = Some(now + skip as i64 * 1_000_000_000 / a.rate as i64);
                if known == 0 {
                    return (0, None);
                }
            } else {
                self.audio_skip_until = None;
            }
        }
        let ch = a.channels as usize;
        let take = (known * ch).min(out.len() / ch * ch);
        let out = &mut out[..take];
        match ring.read(out) {
            Ok(n) => {
                if let Some(t) = t {
                    self.audio_read_until = Some(t + n as i64 * 1_000_000_000 / a.rate as i64);
                }
                (n, t)
            }
            Err(_) => {
                self.worker_lost();
                (0, None)
            }
        }
    }

    pub fn audio_format(&self) -> Option<(u32, u16)> {
        self.audio.as_ref().map(|a| (a.rate, a.channels))
    }

    fn worker_lost(&mut self) {
        if self.dead {
            return;
        }
        if let Some(mut w) = self.w.take() {
            w.kill();
        }
        // Its buffers die with it.
        self.queued.clear();
        self.sink.discontinuity();
        // A whole resource can start over where it was; a feed's bytes are
        // gone with the worker.
        // (A pipe can't: what the last worker read is gone.)
        let restartable = matches!(&self.source, Source::File(f) if f.metadata().is_ok_and(|m| m.is_file())) && self.restarts == 0;
        if restartable && self.start_worker().is_ok() {
            self.restarts += 1;
            self.audio_skip_until = self.audio_read_until;
            if let Some(t) = self.last_pts {
                self.epoch += 1;
                self.show_from = Some(t + 1);
                let epoch = self.epoch;
                self.send(ToWorker::Seek { t_ns: t, epoch });
            }
            self.events.push_back(PlayerEvent::Restarted);
            return;
        }
        self.dead = true;
        self.events.push_back(PlayerEvent::Error(err::DECODE));
    }

    /// Kill the worker (tests: as if it crashed).
    pub fn kill_worker(&mut self) {
        if let Some(w) = self.w.as_mut() {
            w.kill();
        }
    }

    pub fn worker_pid(&self) -> Option<u32> {
        self.w.as_ref().map(|w| w.child.id())
    }

    /// The next event, waiting at most `timeout` for the worker. Returns
    /// `Ok(None)` on timeout (and kills a worker that owes us something and
    /// has been silent past the watchdog).
    pub fn poll(&mut self, timeout: Duration) -> Option<PlayerEvent> {
        loop {
            if let Some(e) = self.events.pop_front() {
                return Some(e);
            }
            if self.dead {
                return None;
            }
            let Some(w) = self.w.as_mut() else { return None };
            match w.recv(timeout) {
                Ok(Some(b)) => {
                    self.heard = std::time::Instant::now();
                    match Record::decode(&b) {
                        Some(r) => self.on_record(r),
                        None => self.worker_lost(),
                    }
                }
                Ok(None) => {
                    if self.waiting && self.heard.elapsed() > WATCHDOG {
                        self.worker_lost();
                        continue;
                    }
                    return None;
                }
                Err(_) => self.worker_lost(),
            }
        }
    }

    fn on_record(&mut self, r: Record) {
        match r {
            Record::Ready { sandboxed } => self.events.push_back(PlayerEvent::Sandboxed(sandboxed)),
            Record::VideoTrack { width, height, bit_depth, .. } => {
                if self.video.is_some() {
                    return self.worker_lost();
                }
                let format = if bit_depth > 8 { Format::P010 } else { Format::Nv12 };
                let (lw, lh) = Format::layer_dims(width, height);
                let count = self.opts.buffers.clamp(3, BUFFERS_MAX);
                match self.sink.layer(format, lw, lh, count) {
                    Ok(fds) if fds.len() == count as usize => {
                        let Ok(fds) = fds.iter().map(|f| sys::restricted(f, sys::Rights::Shm)).collect::<io::Result<Vec<_>>>() else {
                            return self.worker_lost();
                        };
                        let refs: Vec<&OwnedFd> = fds.iter().collect();
                        let ok = self.w.as_ref().map(|w| w.send(&ToWorker::Buffers { format, width: lw, height: lh, count }, &refs).is_ok()).unwrap_or(false);
                        if !ok {
                            return self.worker_lost();
                        }
                        self.video = Some(VideoState { format, width: lw, height: lh, count, held: vec![false; count as usize] });
                        if self.restarts == 0 || self.last_pts.is_none() {
                            self.events.push_back(PlayerEvent::VideoTrack { width, height, bit_depth });
                        }
                    }
                    _ => {
                        self.dead = true;
                        self.events.push_back(PlayerEvent::Error(err::TOO_BIG));
                    }
                }
            }
            Record::AudioTrack { rate, channels, .. } => {
                if self.audio.is_some() {
                    return self.worker_lost();
                }
                // One second of PCM between the worker and us.
                match PcmRing::create(rate, channels) {
                    Ok((ring, fd)) => {
                        let Ok(fd) = sys::restricted(&fd, sys::Rights::Shm) else { return self.worker_lost() };
                        let ok = self.w.as_ref().map(|w| w.send(&ToWorker::PcmRing, &[&fd]).is_ok()).unwrap_or(false);
                        if !ok {
                            return self.worker_lost();
                        }
                        self.audio = Some(AudioState { ring: Some(ring), rate, channels, runs: VecDeque::new(), known_until: 0 });
                        if self.restarts == 0 || self.last_pts.is_none() {
                            self.events.push_back(PlayerEvent::AudioTrack { rate, channels });
                        }
                    }
                    Err(_) => self.events.push_back(PlayerEvent::Error(err::STATE)),
                }
            }
            Record::Duration { secs } => self.events.push_back(PlayerEvent::Duration(secs)),
            Record::Frame { index, epoch, pts_ns } => {
                let Some(v) = self.video.as_mut() else { return self.worker_lost() };
                if index >= v.count || v.held[index as usize] {
                    return self.worker_lost();
                }
                v.held[index as usize] = true;
                let stale = epoch != self.epoch || self.show_from.is_some_and(|t| pts_ns < t);
                if stale {
                    self.release(index);
                    return;
                }
                self.show_from = None;
                self.waiting = false;
                if self.paused && !std::mem::take(&mut self.show_one) {
                    self.queued.push_back((index, pts_ns));
                } else {
                    self.present(index, pts_ns);
                }
            }
            Record::Pcm { epoch, pts_ns, frames, pos } => {
                let Some(a) = self.audio.as_mut() else { return self.worker_lost() };
                if epoch == self.epoch && frames > 0 {
                    if a.runs.back().is_some_and(|&(p, _)| pos < p) {
                        return self.worker_lost();
                    }
                    a.runs.push_back((pos, pts_ns));
                    a.known_until = pos + frames as u64;
                    if a.runs.len() > 4096 {
                        a.runs.pop_front();
                    }
                    if self.video.is_none() {
                        self.last_pts = Some(pts_ns);
                        self.waiting = false;
                    }
                }
            }
            Record::Appended { track } => {
                self.waiting = false;
                self.events.push_back(PlayerEvent::Appended { track });
            }
            Record::Buffered { track, i, n, start_ns, end_ns } => {
                let e = match self.buffered.iter_mut().find(|(t, _)| *t == track) {
                    Some(e) => e,
                    None => {
                        self.buffered.push((track, Vec::new()));
                        self.buffered.last_mut().unwrap()
                    }
                };
                if i == 0 {
                    e.1.clear();
                }
                if n > 0 {
                    e.1.push((start_ns, end_ns));
                }
                if n == 0 || i + 1 == n {
                    let ranges = e.1.clone();
                    self.events.push_back(PlayerEvent::Buffered { track, ranges });
                }
            }
            Record::Seeked { epoch, pos } => {
                if epoch == self.epoch && matches!(self.source, Source::Feed(_)) {
                    self.waiting = false;
                }
                if epoch == self.epoch {
                    if let Some(a) = self.audio.as_mut() {
                        a.runs.clear();
                        a.known_until = pos;
                        if let Some(r) = a.ring.as_mut() {
                            if r.skip_to(pos).is_err() {
                                return self.worker_lost();
                            }
                        }
                    }
                }
            }
            Record::Ended => {
                self.waiting = false;
                self.ended = true;
                self.events.push_back(PlayerEvent::Ended);
            }
            Record::Error { code } => {
                self.waiting = false;
                self.events.push_back(PlayerEvent::Error(code));
            }
            Record::Probe { refused } => {
                self.waiting = false;
                self.events.push_back(PlayerEvent::Probe { refused });
            }
        }
    }
}

impl<S: Sink> Drop for Player<S> {
    fn drop(&mut self) {
        if let Some(w) = self.w.as_ref() {
            let _ = w.send(&ToWorker::Shutdown, &[]);
        }
    }
}

/// A sink for tests and `--headless`: frames go to local shared memory and
/// are hashed (or counted) at once.
pub struct LocalSink {
    pub maps: Vec<sys::Map>,
    pub format: Option<(Format, u32, u32)>,
    pub on_frame: Box<dyn FnMut(&[u8], i64)>,
}

impl LocalSink {
    pub fn new(on_frame: Box<dyn FnMut(&[u8], i64)>) -> Self {
        LocalSink { maps: Vec::new(), format: None, on_frame }
    }
}

impl Sink for LocalSink {
    fn layer(&mut self, format: Format, width: u32, height: u32, count: u32) -> io::Result<Vec<OwnedFd>> {
        let len = format.buffer_size(width, height);
        let mut fds = Vec::new();
        self.maps.clear();
        for _ in 0..count {
            let fd = sys::shm_anon(len)?;
            self.maps.push(sys::Map::new(&fd, len)?);
            fds.push(fd);
        }
        self.format = Some((format, width, height));
        Ok(fds)
    }

    fn frame(&mut self, index: u32, pts_ns: i64) -> bool {
        if let Some(m) = self.maps.get(index as usize) {
            (self.on_frame)(m.bytes(), pts_ns);
        }
        true
    }
}
