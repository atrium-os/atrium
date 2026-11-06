//! lyrad as a service (lyra-v1.md §3 and §5, work items A2 and A4).
//!
//! - **One socket, one peer.** `lyrad.sock` speaks [`wire::ToEngine`] /
//!   [`wire::FromEngine`], and lyrad accepts a connection only from the uids it
//!   was told to trust (choragusd's). A stream's ring is made only by the
//!   `Open` that creates the stream, on that connection.
//! - **Sessions.** The trusted peer's connection is a session. Every stream
//!   belongs to it: when it ends (choragusd exits or restarts) every stream is
//!   closed, and commands from a stale session are ignored.
//! - **Streams** have their own rate (8 to 192 kHz) and channel count (1, 2,
//!   6, 8). lyrad converts each once, to the device's rate and layout
//!   ([`Adapter`]), checks its samples ([`sanitize`]), applies its gain and sums.
//!   A stream starts paused; `Flush` drops what is queued; `Drain` reports when
//!   the last frame has been played; empty rings are reported as underruns, at
//!   most once a second.
//! - **The clock** (§6): every period lyrad writes each playing stream's clock
//!   page: which stream frame is heard at which `CLOCK_MONOTONIC` instant,
//!   from the adapter's position, the device's queue and the measured device
//!   speed.
//! - **Runs until stopped**, reopening the device if it goes away.
//!
//! The engine ([`Engine`]) is host-testable; [`run`] is the FreeBSD loop.

use crate::binaural::Virtualizer;
use crate::channels::{matrix, ChannelMap};
use crate::gain::Gain;
use crate::oss::OssSink;
use crate::ring::Ring;
use lyra_protocol::clock::{self, Clock, ClockWriter, STATE_PAUSED, STATE_PLAYING};
use lyra_protocol::wire::{self, FromEngine, ToEngine};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

/// The most streams lyrad mixes at once (lyra-v1.md §5).
pub const MAX_STREAMS: usize = 64;
/// Largest ring lyrad will make, in frames (2 s at 192 kHz, rounded up).
pub const MAX_RING_FRAMES: u64 = 1 << 19;
/// Largest magnitude a sample may have after [`sanitize`].
pub const SAMPLE_LIMIT: f32 = 4.0;

/// Replace NaN and infinity with 0 and clamp to ±[`SAMPLE_LIMIT`]. Run on each
/// stream's samples before they are summed, so a hostile or broken stream can't
/// poison the shared mix.
pub fn sanitize(buf: &mut [f32]) {
    for s in buf.iter_mut() {
        *s = if s.is_finite() { s.clamp(-SAMPLE_LIMIT, SAMPLE_LIMIT) } else { 0.0 };
    }
}

fn layout(channels: u8) -> Option<ChannelMap> {
    Some(match channels {
        1 => ChannelMap::mono(),
        2 => ChannelMap::stereo(),
        6 => ChannelMap::surround_5_1(),
        8 => ChannelMap::surround_7_1(),
        _ => return None,
    })
}

/// Converts one stream to the device: pulls frames from its ring at the
/// stream's rate, resamples (linear; resample once, architecture §4) and maps
/// channels, producing exactly the frames the period needs.
pub struct Adapter {
    ch: usize,
    dev_ch: usize,
    /// Stream frames per device frame.
    ratio: f64,
    frac: f64,
    /// Stream frames read but not yet used; always starts with the left
    /// interpolation point.
    carry: Vec<f32>,
    m: Vec<Vec<f32>>,
    scratch: Vec<f32>,
}

impl Adapter {
    /// Convert to another device layout from the next period on.
    pub fn set_device_channels(&mut self, dev: u8) -> bool {
        let Some(m) = layout(self.ch as u8).zip(layout(dev)).map(|(a, b)| matrix(&a, &b)) else { return false };
        self.m = m;
        self.dev_ch = dev as usize;
        true
    }

    pub fn device_channels(&self) -> usize {
        self.dev_ch
    }

    pub fn new(rate: u32, channels: u8, dev_rate: u32, dev_channels: u8) -> Option<Adapter> {
        let m = matrix(&layout(channels)?, &layout(dev_channels)?);
        let ch = channels as usize;
        Some(Adapter {
            ch,
            dev_ch: dev_channels as usize,
            ratio: rate as f64 / dev_rate as f64,
            frac: 0.0,
            carry: vec![0.0; ch],
            m,
            scratch: Vec::new(),
        })
    }

    /// The stream position (fractional, stream frames) the next output frame
    /// will sample, given the ring's consumed count. The first carried frame is
    /// the one before `consumed - carried + 1`.
    pub fn position(&self, consumed: u64) -> f64 {
        consumed as f64 - (self.carry.len() / self.ch) as f64 + self.frac
    }

    /// Stream frames per device frame.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Forget carried frames (a flush).
    pub fn reset(&mut self) {
        self.frac = 0.0;
        self.carry.clear();
        self.carry.resize(self.ch, 0.0);
    }

    /// Fill `out` (`n × dev_ch` interleaved) from `ring`. Returns the device
    /// frames that had real input; the rest are zero (an underrun).
    pub fn pull(&mut self, ring: &Ring, out: &mut [f32]) -> usize {
        let n = out.len() / self.dev_ch;
        let ch = self.ch;
        let have = self.carry.len() / ch;
        // Highest stream index the n outputs touch, plus its right neighbour.
        let need = (self.frac + (n.saturating_sub(1)) as f64 * self.ratio).floor() as usize + 2;
        if need > have {
            let start = self.carry.len();
            self.carry.resize(need * ch, 0.0);
            let got = ring.read(&mut self.carry[start..]) as usize;
            self.carry.truncate(start + got * ch);
        }
        let have = self.carry.len() / ch;
        self.scratch.resize(ch, 0.0);
        let mut produced = 0;
        for k in 0..n {
            let x = self.frac + k as f64 * self.ratio;
            let i = x.floor() as usize;
            if i + 1 >= have {
                break;
            }
            let t = (x - i as f64) as f32;
            for c in 0..ch {
                self.scratch[c] = self.carry[i * ch + c] * (1.0 - t) + self.carry[(i + 1) * ch + c] * t;
            }
            for o in 0..self.dev_ch {
                let mut acc = 0.0;
                for c in 0..ch {
                    acc += self.m[o][c] * self.scratch[c];
                }
                out[k * self.dev_ch + o] = acc;
            }
            produced += 1;
        }
        out[produced * self.dev_ch..].iter_mut().for_each(|v| *v = 0.0);
        // Drop the stream frames fully behind the next output's left point.
        let next = self.frac + produced as f64 * self.ratio;
        let drop = (next.floor() as usize).min(have.saturating_sub(1));
        self.frac = next - drop as f64;
        self.carry.drain(..drop * ch);
        if produced < n {
            // Underrun: keep only the last frame as the left point, and restart
            // the phase there so the next frames line up.
            let keep = self.carry.len().saturating_sub(ch);
            self.carry.drain(..keep);
            self.frac = 0.0;
        }
        produced
    }
}

/// A command for the engine, applied at a period boundary.
pub enum Cmd {
    /// A trusted peer connected: session `gen` begins with no streams.
    Begin(u64),
    /// Session `gen`'s connection ended: close its streams.
    End(u64),
    Msg(u64, ToEngine),
    /// A stream made by session `gen`'s `Open`: the consumer end of its ring
    /// and the writer of its clock page.
    Open(u64, u32, Box<Adapter>, Ring, ClockWriter),
}

struct Slot {
    id: u32,
    gain: Gain,
    ring: Ring,
    adapter: Box<Adapter>,
    clock: ClockWriter,
    stream_rate: u32,
    flush_epoch: u32,
    playing: bool,
    /// Started at least once: from then on level changes ramp.
    started: bool,
    /// Goes through the output's virtual surround when it is on.
    spatial: bool,
    /// Periods left before `Drained`, once the ring has run dry.
    drain: Option<Option<u32>>,
    underrun_frames: u64,
    periods_since_report: u32,
}

/// The mix state: the current session and its streams.
pub struct Engine {
    gen: u64,
    slots: Vec<Slot>,
    rate: u32,
    period_frames: usize,
    tmp: Vec<f32>,
    /// Events for the current session, taken by [`Engine::take_events`].
    events: Vec<FromEngine>,
    /// Measured device speed against nominal (ppm), for the clock pages.
    pub rate_ppm: i32,
    channels: usize,
    /// The output's left and right gains, from its balance.
    balance: [Gain; 2],
    /// A layout choragusd asked for, until the run loop reopens the device.
    pub want_channels: Option<u8>,
    /// Headphone virtual surround switched on for the output.
    spatial: bool,
    virt: Virtualizer,
    /// The virtual 7.1 bed the spatial streams mix into (period × 8).
    bed: Vec<f32>,
}

/// The virtual speaker bed: 7.1.
const BED_CHANNELS: usize = 8;

/// Left and right gains for a balance in −1..1: the far side fades, the near
/// side stays at full level. Not a number counts as centred.
pub fn balance_gains(b: f32) -> [f32; 2] {
    let b = if b.is_finite() { b.clamp(-1.0, 1.0) } else { 0.0 };
    [(1.0 - b).min(1.0), (1.0 + b).min(1.0)]
}

impl Engine {
    pub fn new(rate: u32, channels: usize, period_frames: usize) -> Self {
        Engine {
            gen: 0,
            slots: Vec::new(),
            rate,
            period_frames,
            tmp: vec![0.0; period_frames * channels.max(BED_CHANNELS)],
            events: Vec::new(),
            rate_ppm: 0,
            channels,
            balance: [Gain::new(1.0, rate as f32, 20.0), Gain::new(1.0, rate as f32, 20.0)],
            want_channels: None,
            spatial: false,
            virt: Virtualizer::for_period(rate as f32, period_frames),
            bed: vec![0.0; period_frames * BED_CHANNELS],
        }
    }

    /// Virtual surround is rendered: switched on, and the device is stereo
    /// (on real speakers the layout itself is the surround).
    fn virtual_on(&self) -> bool {
        self.spatial && self.channels == 2
    }

    /// The layout a stream's adapter mixes to: the bed for a spatial stream
    /// while virtual surround is on, the device's otherwise.
    fn target_channels(&self, stream_spatial: bool) -> usize {
        if stream_spatial && self.virtual_on() { BED_CHANNELS } else { self.channels }
    }

    /// Point every stream's adapter at its layout (after a change of layout,
    /// of the spatial switch, or of a stream's opt-out).
    fn retarget(&mut self) {
        let (virt, ch) = (self.virtual_on(), self.channels);
        for s in &mut self.slots {
            let t = if s.spatial && virt { BED_CHANNELS } else { ch };
            if s.adapter.device_channels() != t {
                s.adapter.set_device_channels(t as u8);
            }
        }
    }

    /// Mix for a device with `ch` channels from the next period on (the
    /// device was reopened with them). Every stream keeps its ring and clock.
    pub fn set_channels(&mut self, ch: usize) {
        self.channels = ch;
        self.tmp.resize(self.period_frames * ch.max(BED_CHANNELS), 0.0);
        self.retarget();
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn streams(&self) -> usize {
        self.slots.len()
    }

    pub fn is_open(&self, stream: u32) -> bool {
        self.slots.iter().any(|s| s.id == stream)
    }

    pub fn session(&self) -> u64 {
        self.gen
    }

    pub fn take_events(&mut self) -> Vec<FromEngine> {
        std::mem::take(&mut self.events)
    }

    fn slot(&mut self, stream: u32) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.id == stream)
    }

    /// Apply one command. Commands from a session that is not the current one
    /// are ignored, so a stale connection can't touch the new session's streams.
    pub fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Begin(g) => {
                if !self.slots.is_empty() {
                    eprintln!("lyrad: new session {g}; closing {} stream(s) of session {}", self.slots.len(), self.gen);
                }
                self.slots.clear();
                self.events.clear();
                self.gen = g;
            }
            Cmd::End(g) if g == self.gen => {
                if !self.slots.is_empty() {
                    eprintln!("lyrad: session {g} ended; closing {} stream(s)", self.slots.len());
                }
                self.slots.clear();
                self.events.clear();
            }
            Cmd::End(_) => {}
            Cmd::Msg(g, _) | Cmd::Open(g, ..) if g != self.gen => {}
            Cmd::Open(_, id, mut adapter, ring, clock) => {
                // Made for the layout the connection thread knew of. New
                // streams are spatial until choragusd says otherwise.
                let target = self.target_channels(true);
                if adapter.device_channels() != target && !adapter.set_device_channels(target as u8) {
                    return;
                }
                if self.is_open(id) || self.slots.len() >= MAX_STREAMS {
                    return;
                }
                let stream_rate = (adapter.ratio() * self.rate as f64).round() as u32;
                clock.publish(&Clock { state: STATE_PAUSED, frame: 0, heard_ns: 0, rate_ppm: self.rate_ppm, flush_epoch: 0, rate: stream_rate });
                self.slots.push(Slot {
                    id,
                    // Silent until choragusd sets its level, so a stream the
                    // user muted or turned down is never heard at full level.
                    gain: Gain::new(0.0, self.rate as f32, 20.0),
                    ring,
                    adapter,
                    clock,
                    stream_rate,
                    flush_epoch: 0,
                    playing: false,
                    started: false,
                    spatial: true,
                    drain: None,
                    underrun_frames: 0,
                    periods_since_report: 0,
                });
            }
            Cmd::Msg(_, m) => self.apply_msg(m),
        }
    }

    fn apply_msg(&mut self, m: ToEngine) {
        match m {
            ToEngine::Open { .. } => {} // made by the connection thread
            ToEngine::Close { stream } => self.slots.retain(|s| s.id != stream),
            ToEngine::Start { stream } => {
                if let Some(s) = self.slot(stream) {
                    s.playing = true;
                    s.started = true;
                }
            }
            ToEngine::Pause { stream } => {
                let ppm = self.rate_ppm;
                if let Some(s) = self.slot(stream) {
                    s.playing = false;
                    s.clock.publish(&Clock { state: STATE_PAUSED, frame: s.ring.consumed(), heard_ns: 0, rate_ppm: ppm, flush_epoch: s.flush_epoch, rate: s.stream_rate });
                }
            }
            ToEngine::Flush { stream } => {
                if let Some(s) = self.slot(stream) {
                    let frame = s.ring.discard();
                    s.adapter.reset();
                    s.drain = None;
                    s.flush_epoch = s.flush_epoch.wrapping_add(1);
                    self.events.push(FromEngine::Flushed { stream, frame });
                }
            }
            ToEngine::Drain { stream } => {
                if let Some(s) = self.slot(stream) {
                    s.drain = Some(None);
                }
            }
            ToEngine::SetGain { stream, db } => {
                if let Some(s) = self.slot(stream) {
                    // A non-finite or absurd level from the wire is clamped too.
                    let db = if db.is_finite() { db.clamp(-120.0, 12.0) } else { -120.0 };
                    let g = if db <= -120.0 { 0.0 } else { 10f32.powf(db / 20.0) };
                    // Nothing has heard it yet: no ramp to hear (a ramp from
                    // the old level would play a muted stream for 20 ms).
                    if s.started { s.gain.set_target(g) } else { s.gain.jump(g) }
                }
            }
            ToEngine::Reroute { .. } => {} // single sink in v1
            ToEngine::SetLayout { channels, .. } => {
                if layout(channels).is_some() && channels >= 2 {
                    self.want_channels = Some(channels);
                }
            }
            ToEngine::SetSpatial { on, .. } => {
                if self.spatial != on {
                    self.spatial = on;
                    self.virt.reset();
                    self.retarget();
                }
            }
            ToEngine::SetStreamSpatial { stream, on } => {
                if let Some(s) = self.slot(stream) {
                    s.spatial = on;
                }
                self.retarget();
            }
            ToEngine::SetBalance { balance, .. } => {
                let [l, r] = balance_gains(balance);
                self.balance[0].set_target(l);
                self.balance[1].set_target(r);
            }
        }
    }

    /// Mix one period into `mix` (interleaved, `period_frames × channels`).
    /// `device_delay_frames` is what the device still has queued (for `Drain`
    /// and the clock), and `now_ns` is `CLOCK_MONOTONIC` now: this period's
    /// first frame is heard `device_delay_frames` later.
    pub fn mix(&mut self, mix: &mut [f32], device_delay_frames: u64, now_ns: i64) {
        let start_ns = now_ns + (device_delay_frames as i128 * 1_000_000_000 / self.rate as i128) as i64;
        self.mix_at(mix, device_delay_frames, start_ns);
    }

    /// As [`Engine::mix`], with the time this period's first frame is heard
    /// given (from [`DeviceClock`], smoother than one reading of the delay).
    pub fn mix_at(&mut self, mix: &mut [f32], device_delay_frames: u64, start_ns: i64) {
        mix.iter_mut().for_each(|v| *v = 0.0);
        let virt = self.virtual_on();
        if virt {
            self.bed.iter_mut().for_each(|v| *v = 0.0);
        }
        let dev_rate = self.rate as f64;
        let ppm = self.rate_ppm;
        let drain_periods = (device_delay_frames as usize).div_ceil(self.period_frames) as u32 + 1;
        let report_every = (self.rate as usize / self.period_frames).max(1) as u32;
        for s in self.slots.iter_mut() {
            if !s.playing {
                continue;
            }
            // The clock: output frame k samples stream position x0 + k·ratio,
            // and is heard at start + k / device rate. Anchor on the first
            // whole stream frame at or after x0.
            let x0 = s.adapter.position(s.ring.consumed());
            // A spatial stream mixes into the bed, the rest straight to
            // the device (Pro and opted-out streams join after the
            // binaural stage, untouched by it).
            let into_bed = virt && s.adapter.device_channels() == BED_CHANNELS;
            let tmp = &mut self.tmp[..self.period_frames * s.adapter.device_channels()];
            let got = s.adapter.pull(&s.ring, tmp);
            if got > 0 {
                let frame = x0.max(0.0).ceil();
                let k = (frame - x0) / s.adapter.ratio();
                let heard_ns = start_ns + (k / dev_rate * 1e9).round() as i64;
                s.clock.publish(&Clock { state: STATE_PLAYING, frame: frame as u64, heard_ns, rate_ppm: ppm, flush_epoch: s.flush_epoch, rate: s.stream_rate });
            }
            let short = (self.period_frames - got) as u64;
            match s.drain {
                Some(None) if short > 0 || s.ring.readable() == 0 => s.drain = Some(Some(drain_periods)),
                Some(Some(0)) => {
                    self.events.push(FromEngine::Drained { stream: s.id });
                    s.drain = None;
                    s.playing = false;
                }
                Some(Some(ref mut left)) => *left -= 1,
                _ => s.underrun_frames += short,
            }
            s.periods_since_report += 1;
            if s.periods_since_report >= report_every {
                if s.underrun_frames > 0 {
                    self.events.push(FromEngine::Underrun { stream: s.id, frames: s.underrun_frames });
                    s.underrun_frames = 0;
                }
                s.periods_since_report = 0;
            }
            sanitize(tmp);
            s.gain.process_frames(tmp, s.adapter.device_channels());
            let dest: &mut [f32] = if into_bed { &mut self.bed } else { mix };
            for (m, t) in dest.iter_mut().zip(tmp.iter()) {
                *m += *t;
            }
        }
        if virt {
            self.virt.render_add(&self.bed, mix);
        }
        // The output's balance: left-side speakers (FL, SL, RL) take the
        // left gain, right-side ones (FR, SR, RR) the right; FC and LFE none.
        if self.channels >= 2 {
            let (lefts, rights): (&[usize], &[usize]) = match self.channels {
                6 => (&[0, 4], &[1, 5]),
                8 => (&[0, 4, 6], &[1, 5, 7]),
                _ => (&[0], &[1]),
            };
            for f in mix.chunks_exact_mut(self.channels) {
                let (l, r) = (self.balance[0].next(), self.balance[1].next());
                lefts.iter().for_each(|&i| f[i] *= l);
                rights.iter().for_each(|&i| f[i] *= r);
            }
        }
    }
}

/// Where lyrad listens, whom it trusts, and the device format.
#[derive(Clone)]
pub struct Config {
    pub socket: PathBuf,
    /// Peer uids allowed to connect. Empty = only lyrad's own uid (development).
    pub peers: Vec<u32>,
    pub rate: u32,
    pub channels: u32,
    pub period_frames: u32,
    pub nfrags: u32,
}

impl Config {
    /// From the environment (jaild passes only `ATRIUM_*` variables):
    /// `ATRIUM_LYRAD_SOCK` (default `/atrium/sockets/lyrad/lyrad.sock`) and
    /// `ATRIUM_LYRAD_PEERS`, a comma-separated list of user names or uids
    /// (default `_choragus`). A name that does not resolve is an error, so a
    /// missing system user fails the start instead of trusting nobody silently.
    pub fn from_env() -> io::Result<Config> {
        let socket = std::env::var("ATRIUM_LYRAD_SOCK").unwrap_or_else(|_| "/atrium/sockets/lyrad/lyrad.sock".into());
        let peers_s = std::env::var("ATRIUM_LYRAD_PEERS").unwrap_or_else(|_| "_choragus".into());
        Ok(Config { socket: socket.into(), peers: parse_peers(&peers_s)?, rate: 48_000, channels: 2, period_frames: 128, nfrags: 8 })
    }

    /// The output latency a new stream sees: the device queue plus one period.
    pub fn latency_ns(&self) -> u64 {
        (self.nfrags as u64 + 1) * self.period_frames as u64 * 1_000_000_000 / self.rate as u64
    }
}

/// Parse `name,uid,...` into uids.
pub fn parse_peers(s: &str) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for p in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let uid = match p.parse::<u32>() {
            Ok(u) => u,
            Err(_) => lyra_protocol::peer::uid_of(p)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no such user '{p}'")))?,
        };
        out.push(uid);
    }
    Ok(out)
}

/// Whether a peer uid may connect.
pub fn peer_allowed(uid: u32, peers: &[u32]) -> bool {
    if peers.is_empty() {
        uid == unsafe { libc::getuid() }
    } else {
        peers.contains(&uid)
    }
}

/// The current session's write side, shared by its connection thread (which
/// answers `Open` with a ring fd) and the event writer (engine events).
type Writer = Arc<Mutex<Option<(u64, UnixStream)>>>;

/// Make a ring for an `Open`, or `None` if its parameters are out of range.
#[allow(clippy::type_complexity)]
fn make_stream(cfg: &Config, rate: u32, channels: u8, ring_frames: u64) -> Option<(Box<Adapter>, Ring, std::os::fd::OwnedFd, ClockWriter, std::os::fd::OwnedFd)> {
    if !(wire::MIN_RATE..=wire::MAX_RATE).contains(&rate)
        || !ring_frames.is_power_of_two()
        || ring_frames > MAX_RING_FRAMES
    {
        return None;
    }
    let adapter = Adapter::new(rate, channels, cfg.rate, cfg.channels as u8)?;
    let (ring, fd) = Ring::create_anon(ring_frames, channels as u64, false).ok()?;
    let (clock, clock_fd) = clock::create().ok()?;
    Some((Box::new(adapter), ring, fd, clock, clock_fd))
}

/// Serve one trusted connection as session `gen`.
fn serve_session(mut conn: UnixStream, gen: u64, cfg: Config, tx: mpsc::Sender<Cmd>, writer: Writer) {
    let _ = tx.send(Cmd::Begin(gen));
    if let Ok(w) = conn.try_clone() {
        *writer.lock().unwrap() = Some((gen, w));
    }
    loop {
        let msg: ToEngine = match wire::recv(&mut conn) {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            ToEngine::Open { stream, rate, channels, ring_frames } => {
                let reply = match make_stream(&cfg, rate, channels, ring_frames) {
                    Some((adapter, ring, fd, clock, clock_fd)) => {
                        let _ = tx.send(Cmd::Open(gen, stream, adapter, ring, clock));
                        Some((fd, clock_fd))
                    }
                    None => None,
                };
                let mut w = writer.lock().unwrap();
                let Some((g, w)) = w.as_mut() else { break };
                if *g != gen {
                    break; // a newer session took over
                }
                let ok = match reply {
                    Some((fd, clock_fd)) => wire::send(w, &FromEngine::Opened { stream, ring_frames, latency_ns: cfg.latency_ns() }).is_ok()
                        && crate::fdpass::send_fd(w.as_raw_fd(), fd.as_raw_fd()).is_ok()
                        && crate::fdpass::send_fd(w.as_raw_fd(), clock_fd.as_raw_fd()).is_ok(),
                    None => wire::send(w, &FromEngine::OpenFailed { stream }).is_ok(),
                };
                if !ok {
                    break;
                }
            }
            other => {
                let _ = tx.send(Cmd::Msg(gen, other));
            }
        }
    }
    let _ = tx.send(Cmd::End(gen));
    let mut w = writer.lock().unwrap();
    if matches!(w.as_ref(), Some((g, _)) if *g == gen) {
        *w = None;
    }
}

/// Bind the socket and accept trusted peers forever (on its own thread).
pub fn listen(cfg: &Config, tx: mpsc::Sender<Cmd>, writer: Writer) -> io::Result<()> {
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = UnixListener::bind(&cfg.socket)?;
    {
        // Group-connectable; the directory (owned _lyra, group _choragus, 0750)
        // and the peer-uid check below are what keep everyone else out.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o660))?;
    }
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        let mut gen = 0u64;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let uid = match lyra_protocol::peer::peer_uid(conn.as_raw_fd()) {
                Ok(u) => u,
                Err(_) => continue,
            };
            if !peer_allowed(uid, &cfg.peers) {
                eprintln!("lyrad: refused a connection from uid {uid} (not a trusted peer)");
                continue; // dropping `conn` hangs up
            }
            gen += 1;
            eprintln!("lyrad: session {gen} from uid {uid}");
            let (tx, writer, cfg) = (tx.clone(), writer.clone(), cfg.clone());
            std::thread::spawn(move || serve_session(conn, gen, cfg, tx, writer));
        }
    });
    Ok(())
}

/// Send engine events to the current session, off the mix thread.
fn event_writer(rx: mpsc::Receiver<(u64, FromEngine)>, writer: Writer) {
    for (gen, ev) in rx {
        let mut w = writer.lock().unwrap();
        if let Some((g, s)) = w.as_mut() {
            if *g == gen {
                let _ = wire::send(s, &ev);
            }
        }
    }
}

/// `CLOCK_MONOTONIC` in ns.
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// The device's clock, measured (architecture §4: measured, not guessed).
///
/// Each period lyrad knows how many frames it has written and how many the
/// device still holds (GETODELAY), so `written − delay` is the frame the
/// device is sounding now. (GETOPTR is no use: on FreeBSD it counts frames
/// taken into the device buffer, which runs ahead in bursts.) One reading is
/// rough: a device (or QEMU's HDA) moves its position in DMA-sized steps, and
/// a timeshare thread reads it late. So the clock is a least-squares line
/// through the readings of the last [`DeviceClock::WINDOW_NS`], in 50 ms
/// buckets: its slope is the device's real rate, and it says when a given
/// frame will be heard. A reading far off the line (an underrun, a stall)
/// starts the line again.
pub struct DeviceClock {
    nominal: f64,
    /// Finished buckets: (mean time ns, mean position frames), relative to `base`.
    buckets: std::collections::VecDeque<(f64, f64)>,
    /// The bucket being filled: sums and count.
    cur: (f64, f64, u32),
    cur_start: i64,
    base: (i64, i64),
    /// The fitted line: position = q0 + slope × (t − t0) (relative to `base`).
    fit: Option<(f64, f64, f64)>,
    last_report: i64,
}

impl DeviceClock {
    pub const WINDOW_NS: i64 = 20_000_000_000;
    const BUCKET_NS: i64 = 50_000_000;
    /// Fit span before the line is used for the time a frame is heard.
    const MIN_FIT_NS: f64 = 1e9;
    /// Fit span before a rate is reported.
    const MIN_RATE_NS: f64 = 5e9;
    /// A reading this far off the line (seconds) restarts it.
    const RESET_S: f64 = 0.030;

    pub fn new(nominal: u32) -> Self {
        DeviceClock { nominal: nominal as f64, buckets: Default::default(), cur: (0.0, 0.0, 0), cur_start: 0,
            base: (0, 0), fit: None, last_report: 0 }
    }

    fn restart(&mut self, now: i64, pos: i64) {
        self.buckets.clear();
        self.cur = (0.0, 0.0, 0);
        self.cur_start = now;
        self.base = (now, pos);
        self.fit = None;
    }

    fn refit(&mut self) {
        let n = self.buckets.len();
        if n < 3 {
            self.fit = None;
            return;
        }
        let (mt, mq) = self.buckets.iter().fold((0.0, 0.0), |(a, b), (t, q)| (a + t, b + q));
        let (mt, mq) = (mt / n as f64, mq / n as f64);
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for (t, q) in &self.buckets {
            sxx += (t - mt) * (t - mt);
            sxy += (t - mt) * (q - mq);
        }
        if sxx <= 0.0 {
            return;
        }
        let slope = sxy / sxx;
        // A slope wildly off nominal is a bad window, not a device.
        if (slope * 1e9 / self.nominal - 1.0).abs() > 0.05 {
            self.fit = None;
            return;
        }
        self.fit = Some((mt, mq, slope));
    }

    fn span_ns(&self) -> f64 {
        match (self.buckets.front(), self.buckets.back()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0.0,
        }
    }

    /// Feed one reading: the device is sounding frame `pos` at `now`.
    pub fn sample(&mut self, now: i64, pos: u64) {
        let pos = pos as i64;
        if self.cur.2 == 0 && self.buckets.is_empty() {
            self.restart(now, pos);
        }
        let (t, q) = ((now - self.base.0) as f64, (pos - self.base.1) as f64);
        if let Some((t0, q0, slope)) = self.fit {
            let off = (q - (q0 + slope * (t - t0))) / self.nominal;
            if off.abs() > Self::RESET_S || q < 0.0 {
                self.restart(now, pos);
                return self.sample(now, pos as u64);
            }
        }
        self.cur.0 += t;
        self.cur.1 += q;
        self.cur.2 += 1;
        if now - self.cur_start >= Self::BUCKET_NS {
            let c = self.cur.2 as f64;
            self.buckets.push_back((self.cur.0 / c, self.cur.1 / c));
            self.cur = (0.0, 0.0, 0);
            self.cur_start = now;
            let oldest = t - Self::WINDOW_NS as f64;
            while self.buckets.front().is_some_and(|b| b.0 < oldest) {
                self.buckets.pop_front();
            }
            self.refit();
        }
    }

    /// When frame `pos` (counted as in [`DeviceClock::sample`]) is heard, on
    /// `CLOCK_MONOTONIC`, once the line is long enough to trust.
    pub fn heard_at(&self, pos: u64) -> Option<i64> {
        let (t0, q0, slope) = self.fit?;
        if self.span_ns() < Self::MIN_FIT_NS {
            return None;
        }
        let q = (pos as i64 - self.base.1) as f64;
        Some(self.base.0 + (t0 + (q - q0) / slope).round() as i64)
    }

    /// The device's rate off nominal in parts per million (clamped to
    /// ±1000), about once a second, once the line spans a few seconds.
    pub fn rate_ppm(&mut self, now: i64) -> Option<i32> {
        let (_, _, slope) = self.fit?;
        if self.span_ns() < Self::MIN_RATE_NS || now - self.last_report < 1_000_000_000 {
            return None;
        }
        self.last_report = now;
        let ppm = (slope * 1e9 / self.nominal - 1.0) * 1e6;
        Some(ppm.clamp(-1000.0, 1000.0).round() as i32)
    }
}

/// The service: listen, then mix to the device forever, reopening it if it goes.
pub fn run(cfg: Config) -> ! {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let writer: Writer = Arc::new(Mutex::new(None));
    if let Err(e) = listen(&cfg, tx, writer.clone()) {
        eprintln!("lyrad: bind {}: {e}", cfg.socket.display());
        std::process::exit(1);
    }
    let (ev_tx, ev_rx) = mpsc::channel::<(u64, FromEngine)>();
    {
        let writer = writer.clone();
        std::thread::spawn(move || event_writer(ev_rx, writer));
    }
    eprintln!("lyrad: serving on {} (trusted peers: {:?})", cfg.socket.display(), cfg.peers);
    let period_us = cfg.period_frames as u64 * 1_000_000 / cfg.rate as u64;
    let _lane = match crate::lane::self_sponsor(1000, period_us) {
        Ok(fd) => { eprintln!("lyrad: mix thread on the deadline lane"); Some(fd) }
        Err(e) => { eprintln!("lyrad: no deadline lane ({e}); mixing on timeshare"); None }
    };
    let mut engine = Engine::new(cfg.rate, cfg.channels as usize, cfg.period_frames as usize);
    let mut mix = Vec::new();
    let mut out = Vec::new();
    // The device's channel count: 2 until choragusd sets the output's layout.
    let mut channels = cfg.channels;
    // Development: LYRA_DUMP=<file> writes the exact i16 stereo sent to the
    // device, for offline spectral checks (jaild passes no such variable).
    // Appended, so a restarted lyrad keeps what the last one wrote.
    let mut dump = std::env::var("LYRA_DUMP").ok()
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
    // Development: LYRA_POSLOG=<file> logs, each period, CLOCK_MONOTONIC, the
    // device's consumed frames (GETOPTR), the frames written so far (the
    // dump's frame index) and GETODELAY: the clock test's truth (§8 check 6).
    let mut poslog = std::env::var("LYRA_POSLOG").ok()
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok())
        .map(std::io::BufWriter::new);
    let mut written: u64 = 0;
    loop {
        let sink = match OssSink::open(cfg.rate, channels, cfg.period_frames, cfg.nfrags) {
            Ok(s) => s,
            Err(e) if channels != engine.channels() as u32 => {
                // The device refused the new layout: back to the one it had.
                eprintln!("lyrad: the device refused {channels} channels ({e}); staying at {}", engine.channels());
                channels = engine.channels() as u32;
                let _ = ev_tx.send((engine.session(), FromEngine::Layout { channels: channels as u8 }));
                continue;
            }
            Err(e) => {
                eprintln!("lyrad: no OSS device ({e}); retrying in 2 s");
                // Keep the session state current while there is no device.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
                    if let Ok(c) = rx.recv_timeout(left) { engine.apply(c); }
                }
                continue;
            }
        };
        eprintln!("lyrad: OSS device open ({} Hz, {} ch)", sink.rate_hz(), sink.channels());
        engine.set_channels(channels as usize);
        mix.resize(cfg.period_frames as usize * channels as usize, 0.0);
        out.resize(mix.len(), 0);
        let _ = ev_tx.send((engine.session(), FromEngine::Layout { channels: channels as u8 }));
        let mut clock = DeviceClock::new(cfg.rate);
        loop {
            while let Ok(c) = rx.try_recv() {
                engine.apply(c);
            }
            if let Some(w) = engine.want_channels.take() {
                if w as u32 != channels {
                    eprintln!("lyrad: output layout {channels} -> {w} channels; reopening the device");
                    channels = w as u32;
                    break;
                }
                // Already so: still answer, so choragusd knows.
                let _ = ev_tx.send((engine.session(), FromEngine::Layout { channels: w }));
            }
            let delay = sink.delay_frames().unwrap_or(0);
            let now = monotonic_ns();
            // The frame the device is sounding now (see DeviceClock).
            clock.sample(now, written.saturating_sub(delay));
            if let Some(ppm) = clock.rate_ppm(now) {
                engine.rate_ppm = ppm;
            }
            if let Ok(played) = sink.played_frames() {
                if let Some(w) = poslog.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(w, "{now} {played} {written} {delay}");
                    if written % (cfg.rate as u64) < cfg.period_frames as u64 {
                        let _ = w.flush();
                    }
                }
            }
            // This period's first frame is frame `written`.
            let start = clock.heard_at(written)
                .unwrap_or(now + (delay as i128 * 1_000_000_000 / cfg.rate as i128) as i64);
            engine.mix_at(&mut mix, delay, start);
            let gen = engine.session();
            for ev in engine.take_events() {
                let _ = ev_tx.send((gen, ev));
            }
            for (o, m) in out.iter_mut().zip(&mix) {
                *o = (m.clamp(-1.0, 1.0) * 32767.0) as i16;
            }
            if let Some(f) = dump.as_mut() {
                use std::io::Write;
                let bytes = unsafe { std::slice::from_raw_parts(out.as_ptr() as *const u8, out.len() * 2) };
                let _ = f.write_all(bytes);
            }
            written += cfg.period_frames as u64;
            if let Err(e) = sink.write_i16(&out) {
                eprintln!("lyrad: device write failed ({e}); reopening");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// A device at `ppm` off 48 kHz whose position moves in 10 ms DMA steps,
    /// read each 128-frame period by a thread up to 2 ms late.
    fn bursty(ppm: f64, secs: f64, mut each: impl FnMut(i64, u64, u64)) {
        let rate = 48_000.0 * (1.0 + ppm / 1e6);
        let mut seed = 12345u64;
        let mut written = 0u64;
        let mut t = 1_000_000_000.0f64;
        while t < 1e9 + secs * 1e9 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let late = (seed >> 40) as f64 / (1u64 << 24) as f64 * 2e6;
            let now = t + late;
            let true_pos = (now - 1e9) / 1e9 * rate;
            let stepped = (true_pos / 480.0).floor() * 480.0;
            each(now as i64, stepped as u64, written);
            written += 128;
            t += 128.0 / rate * 1e9;
        }
    }

    #[test]
    fn the_device_clock_smooths_a_bursty_device_and_finds_its_rate() {
        for ppm in [-950.0, 0.0, 300.0] {
            let mut c = DeviceClock::new(48_000);
            let mut worst = 0.0f64;
            let mut last_ppm = None;
            let rate = 48_000.0 * (1.0 + ppm / 1e6);
            bursty(ppm, 30.0, |now, pos, _| {
                c.sample(now, pos);
                if let Some(p) = c.rate_ppm(now) { last_ppm = Some(p); }
                // After the first seconds: when is the frame 100 ms ahead heard?
                if now > 4_000_000_000 {
                    let f = pos + 4800;
                    // The true time (the position is floored to 480-frame steps,
                    // so the true line runs half a step above the steps).
                    let truth = 1e9 + (f as f64 + 240.0) / rate * 1e9;
                    let got = c.heard_at(f).expect("fitted") as f64;
                    worst = worst.max((got - truth).abs());
                }
            });
            assert!(worst < 0.5e6, "{ppm} ppm: worst error {:.2} ms", worst / 1e6);
            let p = last_ppm.expect("a rate") as f64;
            assert!((p - ppm).abs() <= 20.0, "{ppm} ppm measured as {p}");
        }
    }

    #[test]
    fn the_device_clock_starts_again_after_a_stall() {
        let mut c = DeviceClock::new(48_000);
        let mut t = 0i64;
        let mut pos = 0u64;
        for _ in 0..2000 {
            c.sample(t, pos);
            t += 2_666_667;
            pos += 128;
        }
        let before = c.heard_at(pos).unwrap();
        assert!((before - t).abs() < 100_000);
        // The device stalls for 200 ms (an underrun), then plays on.
        t += 200_000_000;
        for _ in 0..2000 {
            c.sample(t, pos);
            t += 2_666_667;
            pos += 128;
        }
        assert!((c.heard_at(pos).unwrap() - t).abs() < 100_000, "the line follows the stall");
    }

    use super::*;

    /// A named ring pair (named shm works on the Linux host; anonymous is
    /// FreeBSD-only).
    fn ring_pair(tag: &str, frames: u64, ch: u64) -> (Ring, Ring) {
        let name = format!("/lyra-test-{tag}-{}", std::process::id());
        let p = Ring::create(&name, frames, ch).unwrap();
        let c = Ring::open(&name, false).unwrap();
        (p, c)
    }

    fn writer() -> ClockWriter {
        clock::create().unwrap().0
    }

    fn open(e: &mut Engine, gen: u64, id: u32, rate: u32, ch: u8, ring: Ring) {
        e.apply(Cmd::Open(gen, id, Box::new(Adapter::new(rate, ch, 48_000, 2).unwrap()), ring, writer()));
        // choragusd sets every new stream's level before the app can start it.
        e.apply(Cmd::Msg(gen, ToEngine::SetGain { stream: id, db: 0.0 }));
        e.apply(Cmd::Msg(gen, ToEngine::Start { stream: id }));
    }

    #[test]
    fn sanitize_zeroes_non_finite_and_clamps() {
        let mut b = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e9, -1e9, 0.5];
        sanitize(&mut b);
        assert_eq!(b, [0.0, 0.0, 0.0, SAMPLE_LIMIT, -SAMPLE_LIMIT, 0.5]);
    }

    /// ★ One stream writes NaN, infinity and huge values; the other's level
    /// comes through exactly. Before, one NaN made the whole sum NaN: silence
    /// for every app.
    #[test]
    fn a_poisoned_stream_does_not_touch_the_others() {
        let (pa, ca) = ring_pair("poison-a", 64, 2);
        let (pb, cb) = ring_pair("poison-b", 64, 2);
        let mut e = Engine::new(48_000, 2, 8);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 1, 48_000, 2, ca);
        open(&mut e, 1, 2, 48_000, 2, cb);
        let mut bad = vec![f32::NAN; 32];
        bad[3] = f32::INFINITY;
        bad[5] = 1e30;
        pa.write(&bad);
        pb.write(&[0.25; 32]);
        let mut mix = [0.0f32; 16];
        e.mix(&mut mix, 0, 0);
        assert!(mix.iter().all(|v| v.is_finite()), "{mix:?}");
        // The adapter starts with one zero frame as its left point, so output
        // frame k is stream frame k-1; frame 0 is that silent start.
        assert_eq!(&mix[..2], &[0.0, 0.0]);
        assert!(mix[2..].iter().all(|&v| v == 0.25 || v == 0.25 + SAMPLE_LIMIT), "{mix:?}");
    }

    #[test]
    fn a_new_stream_is_paused_until_start() {
        let (p, c) = ring_pair("paused", 64, 2);
        let mut e = Engine::new(48_000, 2, 8);
        e.apply(Cmd::Begin(1));
        e.apply(Cmd::Open(1, 1, Box::new(Adapter::new(48_000, 2, 48_000, 2).unwrap()), c, writer()));
        e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 1, db: 0.0 }));
        p.write(&[0.5; 32]);
        let mut mix = [0.0f32; 16];
        e.mix(&mut mix, 0, 0);
        assert!(mix.iter().all(|&v| v == 0.0), "paused: nothing read");
        assert_eq!(p.writable(), 64 - 16, "the ring was not touched");
        e.apply(Cmd::Msg(1, ToEngine::Start { stream: 1 }));
        e.mix(&mut mix, 0, 0);
        assert_eq!(mix[15], 0.5);
    }

    #[test]
    fn flush_drops_the_queue_and_reports_the_position() {
        let (p, c) = ring_pair("flush", 64, 2);
        let mut e = Engine::new(48_000, 2, 8);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 1, 48_000, 2, c);
        p.write(&[0.5; 40]); // 20 frames
        e.apply(Cmd::Msg(1, ToEngine::Flush { stream: 1 }));
        assert_eq!(e.take_events(), vec![FromEngine::Flushed { stream: 1, frame: 20 }]);
        let mut mix = [0.0f32; 16];
        e.mix(&mut mix, 0, 0);
        assert!(mix.iter().all(|&v| v == 0.0), "nothing left to play");
    }

    #[test]
    fn drain_reports_after_the_device_queue_plays_out() {
        let (p, c) = ring_pair("drain", 64, 2);
        let mut e = Engine::new(48_000, 2, 8);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 1, 48_000, 2, c);
        p.write(&[0.5; 8]); // 4 frames, less than one period
        e.apply(Cmd::Msg(1, ToEngine::Drain { stream: 1 }));
        let mut mix = [0.0f32; 16];
        // device has 16 frames queued = 2 periods; Drained after those + 1.
        let mut periods = 0;
        loop {
            e.mix(&mut mix, 16, 0);
            periods += 1;
            if e.take_events().contains(&FromEngine::Drained { stream: 1 }) {
                break;
            }
            assert!(periods < 10, "never drained");
        }
        assert_eq!(periods, 5, "dry period, then 3 periods of device queue + margin, then report");
    }

    #[test]
    fn underruns_are_reported_at_most_once_a_second() {
        let (_p, c) = ring_pair("underrun", 64, 2);
        let mut e = Engine::new(800, 2, 8); // 100 periods a second
        e.apply(Cmd::Begin(1));
        e.apply(Cmd::Open(1, 1, Box::new(Adapter::new(800, 2, 800, 2).unwrap()), c, writer()));
        e.apply(Cmd::Msg(1, ToEngine::Start { stream: 1 }));
        let mut mix = [0.0f32; 16];
        let mut reports = Vec::new();
        for _ in 0..250 {
            e.mix(&mut mix, 0, 0);
            reports.extend(e.take_events());
        }
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert_eq!(reports[0], FromEngine::Underrun { stream: 1, frames: 800 });
    }

    #[test]
    fn the_adapter_resamples_and_maps_channels() {
        // 24 kHz mono ramp into 48 kHz stereo: each stream frame becomes two
        // device frames, interpolated, on both channels at -3 dB (mono folds
        // to L and R as the centre does).
        let (p, c) = ring_pair("adapt", 64, 1);
        let mut a = Adapter::new(24_000, 1, 48_000, 2).unwrap();
        p.write(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let mut out = [0.0f32; 8 * 2];
        let got = a.pull(&c, &mut out);
        assert_eq!(got, 8);
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let left: Vec<f32> = out.chunks(2).map(|f| (f[0] / k * 10.0).round() / 10.0).collect();
        assert_eq!(left, vec![0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5]);
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
        // 44.1 kHz into 48 kHz consumes 44.1/48 stream frames per device frame.
        let (p, c) = ring_pair("adapt2", 4096, 2);
        let mut a = Adapter::new(44_100, 2, 48_000, 2).unwrap();
        p.write(&vec![0.1; 4000 * 2]);
        let mut out = vec![0.0f32; 480 * 2];
        for _ in 0..8 {
            assert_eq!(a.pull(&c, &mut out), 480);
        }
        let used = c.consumed() as f64;
        assert!((used - 3840.0 * 44_100.0 / 48_000.0).abs() < 3.0, "{used}");
    }

    #[test]
    fn unsupported_layouts_are_refused() {
        for ch in [0u8, 3, 4, 5, 7, 9] {
            assert!(Adapter::new(48_000, ch, 48_000, 2).is_none(), "{ch}");
        }
    }

    #[test]
    fn commands_from_a_stale_session_are_ignored() {
        let (_p, c1) = ring_pair("stale1", 8, 2);
        let (_q, c2) = ring_pair("stale2", 8, 2);
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 7, 48_000, 2, c1);
        e.apply(Cmd::Begin(2)); // choragusd restarted
        assert_eq!(e.streams(), 0, "a new session starts empty");
        open(&mut e, 1, 8, 48_000, 2, c2);
        e.apply(Cmd::End(1));
        assert_eq!(e.streams(), 0, "the old session can't open or close anything");
    }

    #[test]
    fn a_new_stream_starts_at_its_set_level_not_full() {
        let (_p, c) = ring_pair("first-gain", 8, 2);
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        e.apply(Cmd::Open(1, 1, Box::new(Adapter::new(48_000, 2, 48_000, 2).unwrap()), c, writer()));
        assert_eq!(e.slots[0].gain.current(), 0.0, "silent before choragusd says");
        e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 1, db: -120.0 }));
        assert_eq!(e.slots[0].gain.current(), 0.0);
        e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 1, db: -6.0 }));
        assert!((e.slots[0].gain.current() - 0.501).abs() < 0.001, "jumps before it plays");
        e.apply(Cmd::Msg(1, ToEngine::Start { stream: 1 }));
        e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 1, db: 0.0 }));
        assert!(e.slots[0].gain.current() < 0.6, "ramps once it plays");
    }

    #[test]
    fn virtual_surround_places_the_bed_and_leaves_bypassed_streams_alone() {
        let mut e = Engine::new(48_000, 2, 64);
        e.apply(Cmd::Begin(1));
        // A 7.1 stream with sound only in the left surround, and a stereo
        // stream that opts out (as a Pro stream would).
        let (p1, c1) = ring_pair("virt-sl", 4096, 8);
        open(&mut e, 1, 1, 48_000, 8, c1);
        let (p2, c2) = ring_pair("virt-pro", 4096, 2);
        e.apply(Cmd::Open(1, 2, Box::new(Adapter::new(48_000, 2, 48_000, 2).unwrap()), c2, writer()));
        e.apply(Cmd::Msg(1, ToEngine::SetStreamSpatial { stream: 2, on: false }));
        e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 2, db: 0.0 }));
        e.apply(Cmd::Msg(1, ToEngine::Start { stream: 2 }));
        e.apply(Cmd::Msg(1, ToEngine::SetSpatial { sink: 0, on: true }));
        // A 2 kHz tone in the left surround (period 64 = 2/3 ms).
        let tone_sl = |k: usize| -> Vec<f32> {
            (0..64 * 8).map(|j| if j % 8 == 4 { 0.5 * (2.0 * std::f32::consts::PI * 2000.0 * ((k * 64 + j / 8) as f32) / 48_000.0).sin() } else { 0.0 }).collect()
        };
        let mut mix = vec![0.0f32; 128];
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for k in 0..50 {
            p1.write(&tone_sl(k));
            p2.write(&[0.0; 128]);
            e.mix(&mut mix, 0, 0);
            if k >= 40 {
                l += mix.iter().step_by(2).map(|x| x * x).sum::<f32>();
                r += mix.iter().skip(1).step_by(2).map(|x| x * x).sum::<f32>();
            }
        }
        // Behind on the left: the left ear hears it much louder (FABIAN: 12.8 dB at 2 kHz).
        let d = 10.0 * (l / r).log10();
        assert!((d - 12.8).abs() < 1.5, "left over right {d:.1} dB");

        // The opted-out stream alone: bit-perfect even with surround on.
        p1.write(&[0.0; 64 * 8]);
        let tone: Vec<f32> = (0..128).map(|i| ((i as f32) * 0.37).sin() * 0.7).collect();
        e.apply(Cmd::Msg(1, ToEngine::Close { stream: 1 }));
        e.mix(&mut mix, 0, 0); // let the virtualizer's tail die out
        e.apply(Cmd::Msg(1, ToEngine::SetSpatial { sink: 0, on: false }));
        e.apply(Cmd::Msg(1, ToEngine::SetSpatial { sink: 0, on: true }));
        p2.write(&tone);
        e.mix(&mut mix, 0, 0);
        // (The adapter holds one frame back for its interpolation.)
        assert_eq!(mix[2..], tone[..126]);

        // On a 5.1 output the surround switch does nothing: the speakers
        // are real.
        let (p3, c3) = ring_pair("virt-51", 4096, 8);
        e.set_channels(6);
        open(&mut e, 1, 3, 48_000, 8, c3);
        e.apply(Cmd::Msg(1, ToEngine::Close { stream: 2 }));
        let mut sl = [0.0f32; 64 * 8];
        sl.iter_mut().skip(4).step_by(8).for_each(|v| *v = 0.5);
        let mut mix6 = vec![0.0f32; 64 * 6];
        for _ in 0..50 {
            p3.write(&sl);
            e.mix(&mut mix6, 0, 0);
        }
        assert!((mix6[64 * 6 - 6 + 4] - 0.5).abs() < 1e-3, "{:?}", &mix6[64 * 6 - 6..]);
        assert_eq!(mix6[64 * 6 - 6], 0.0);
    }

    #[test]
    fn balance_fades_the_far_side_only() {
        assert_eq!(balance_gains(0.0), [1.0, 1.0]);
        assert_eq!(balance_gains(-0.5), [1.0, 0.5]);
        assert_eq!(balance_gains(1.0), [0.0, 1.0]);
        assert_eq!(balance_gains(f32::NAN), [1.0, 1.0]);
        assert_eq!(balance_gains(-7.0), [1.0, 0.0]);
        let (p, c) = ring_pair("balance", 4096, 2);
        let mut e = Engine::new(48_000, 2, 64);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 1, 48_000, 2, c);
        e.apply(Cmd::Msg(1, ToEngine::SetBalance { sink: 0, balance: 0.5 }));
        let mut mix = vec![0.0f32; 128];
        for _ in 0..200 {
            p.write(&[0.5; 128]);
            e.mix(&mut mix, 0, 0);
        }
        assert!((mix[126] - 0.25).abs() < 0.01 && (mix[127] - 0.5).abs() < 0.01, "{:?}", &mix[126..]);
    }

    #[test]
    fn a_layout_change_keeps_streams_playing_in_the_new_layout() {
        let (p, c) = ring_pair("layout", 4096, 2);
        let mut e = Engine::new(48_000, 2, 64);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 1, 48_000, 2, c);
        e.apply(Cmd::Msg(1, ToEngine::SetLayout { sink: 0, channels: 6 }));
        assert_eq!(e.want_channels.take(), Some(6));
        e.set_channels(6);
        // A stream opened for the old layout is converted to the new one.
        let (p2, c2) = ring_pair("layout2", 4096, 1);
        e.apply(Cmd::Open(1, 2, Box::new(Adapter::new(48_000, 1, 48_000, 2).unwrap()), c2, writer()));
        e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 2, db: 0.0 }));
        e.apply(Cmd::Msg(1, ToEngine::Start { stream: 2 }));
        e.apply(Cmd::Msg(1, ToEngine::SetBalance { sink: 0, balance: 1.0 }));
        let mut mix = vec![0.0f32; 64 * 6];
        for _ in 0..200 {
            p.write(&[0.5; 128]);
            p2.write(&[0.25; 64]);
            e.mix(&mut mix, 0, 0);
        }
        let f = &mix[mix.len() - 6..];
        // FL faded out by the balance, FR full; mono lands in the centre;
        // nothing in LFE or the surrounds.
        assert!(f[0].abs() < 0.01 && (f[1] - 0.5).abs() < 0.01, "{f:?}");
        assert!((f[2] - 0.25).abs() < 0.01, "{f:?}");
        assert_eq!((f[3], f[4], f[5]), (0.0, 0.0, 0.0), "{f:?}");
        // Unknown layouts are ignored.
        e.apply(Cmd::Msg(1, ToEngine::SetLayout { sink: 0, channels: 5 }));
        assert_eq!(e.want_channels, None);
    }

    #[test]
    fn wild_gains_are_clamped() {
        let (_p, c) = ring_pair("gain", 8, 2);
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        open(&mut e, 1, 1, 48_000, 2, c);
        for db in [f32::NAN, f32::INFINITY, 1e9] {
            e.apply(Cmd::Msg(1, ToEngine::SetGain { stream: 1, db }));
            assert!(e.slots[0].gain.current().is_finite());
        }
    }

    #[test]
    fn peers_parse_and_check() {
        assert_eq!(parse_peers("65017, 0").unwrap(), vec![65017, 0]);
        assert!(parse_peers("_no_such_user_xyz").is_err());
        assert!(peer_allowed(65017, &[65017]));
        assert!(!peer_allowed(1234, &[65017]));
        let me = unsafe { libc::getuid() };
        assert!(peer_allowed(me, &[]), "dev mode: own uid only");
        assert!(!peer_allowed(me.wrapping_add(1), &[]));
    }

    /// The socket refuses an untrusted uid and serves a trusted one.
    #[test]
    fn socket_refuses_untrusted_and_serves_trusted() {
        let dir = std::env::temp_dir().join(format!("lyrad-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let me = unsafe { libc::getuid() };
        let base = Config { socket: dir.join("a.sock"), peers: vec![me.wrapping_add(1)], rate: 48_000, channels: 2, period_frames: 4, nfrags: 2 };
        let (tx, rx) = mpsc::channel();
        listen(&base, tx, Arc::new(Mutex::new(None))).unwrap();
        let mut c = UnixStream::connect(&base.socket).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut b = [0u8; 1];
        use std::io::Read;
        assert_eq!(c.read(&mut b).unwrap_or(0), 0, "hung up on the untrusted peer");
        assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "no session began");

        let cfg = Config { socket: dir.join("b.sock"), peers: vec![me], ..base };
        let (tx, rx) = mpsc::channel();
        listen(&cfg, tx, Arc::new(Mutex::new(None))).unwrap();
        let mut c = UnixStream::connect(&cfg.socket).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        // Out-of-range Open: refused with OpenFailed, no stream.
        wire::send(&mut c, &ToEngine::Open { stream: 3, rate: 1, channels: 2, ring_frames: 64 }).unwrap();
        assert_eq!(wire::recv::<_, FromEngine>(&mut c).unwrap(), FromEngine::OpenFailed { stream: 3 });
        wire::send(&mut c, &ToEngine::Start { stream: 3 }).unwrap();
        let t = std::time::Duration::from_secs(2);
        assert!(matches!(rx.recv_timeout(t).unwrap(), Cmd::Begin(1)));
        assert!(matches!(rx.recv_timeout(t).unwrap(), Cmd::Msg(1, ToEngine::Start { stream: 3 })));
        drop(c);
        assert!(matches!(rx.recv_timeout(t).unwrap(), Cmd::End(1)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
