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
}

impl Engine {
    pub fn new(rate: u32, channels: usize, period_frames: usize) -> Self {
        Engine {
            gen: 0,
            slots: Vec::new(),
            rate,
            period_frames,
            tmp: vec![0.0; period_frames * channels],
            events: Vec::new(),
            rate_ppm: 0,
        }
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
            Cmd::Open(_, id, adapter, ring, clock) => {
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
        }
    }

    /// Mix one period into `mix` (interleaved, `period_frames × channels`).
    /// `device_delay_frames` is what the device still has queued (for `Drain`
    /// and the clock), and `now_ns` is `CLOCK_MONOTONIC` now: this period's
    /// first frame is heard `device_delay_frames` later.
    pub fn mix(&mut self, mix: &mut [f32], device_delay_frames: u64, now_ns: i64) {
        mix.iter_mut().for_each(|v| *v = 0.0);
        let start_ns = now_ns + (device_delay_frames as i128 * 1_000_000_000 / self.rate as i128) as i64;
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
            let got = s.adapter.pull(&s.ring, &mut self.tmp);
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
            sanitize(&mut self.tmp);
            s.gain.process(&mut self.tmp);
            for (m, t) in mix.iter_mut().zip(&self.tmp) {
                *m += *t;
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

/// Measures the device's real speed from its played-frame count against
/// `CLOCK_MONOTONIC` (architecture §4: measured, not guessed). Reports parts
/// per million off nominal once the window is long enough to mean something.
pub struct DeviceSpeed {
    nominal: u32,
    first: Option<(u64, i64)>,
    last_report: i64,
}

impl DeviceSpeed {
    /// Shortest window before a figure is reported.
    pub const MIN_WINDOW_NS: i64 = 10_000_000_000;

    pub fn new(nominal: u32) -> Self {
        DeviceSpeed { nominal, first: None, last_report: 0 }
    }

    /// Feed a (played frames, now) sample; a new figure about once a second.
    pub fn sample(&mut self, played: u64, now: i64) -> Option<i32> {
        let (p0, t0) = *self.first.get_or_insert((played, now));
        let dt = now - t0;
        if dt < Self::MIN_WINDOW_NS || now - self.last_report < 1_000_000_000 || played < p0 {
            return None;
        }
        self.last_report = now;
        let rate = (played - p0) as f64 / (dt as f64 / 1e9);
        let ppm = (rate / self.nominal as f64 - 1.0) * 1e6;
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
    let n = (cfg.period_frames * cfg.channels) as usize;
    let mut engine = Engine::new(cfg.rate, cfg.channels as usize, cfg.period_frames as usize);
    let mut mix = vec![0.0f32; n];
    let mut out = vec![0i16; n];
    // Development: LYRA_DUMP=<file> writes the exact i16 stereo sent to the
    // device, for offline spectral checks (jaild passes no such variable).
    // Appended, so a restarted lyrad keeps what the last one wrote.
    let mut dump = std::env::var("LYRA_DUMP").ok()
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
    loop {
        let sink = match OssSink::open(cfg.rate, cfg.channels, cfg.period_frames, cfg.nfrags) {
            Ok(s) => s,
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
        let mut speed = DeviceSpeed::new(cfg.rate);
        loop {
            while let Ok(c) = rx.try_recv() {
                engine.apply(c);
            }
            let delay = sink.delay_frames().unwrap_or(0);
            let now = monotonic_ns();
            if let Ok(played) = sink.played_frames() {
                if let Some(ppm) = speed.sample(played, now) {
                    engine.rate_ppm = ppm;
                }
            }
            engine.mix(&mut mix, delay, now);
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
            if let Err(e) = sink.write_i16(&out) {
                eprintln!("lyrad: device write failed ({e}); reopening");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
