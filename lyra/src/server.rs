//! lyrad as a service (lyra-v1.md §3, work item A2).
//!
//! The mixer `lyrad --control` proved, run the way a system service must run:
//!
//! - **One socket, one peer.** `lyrad.sock` carries the control frames *and*
//!   ring requests (`Ctl::RingFor`), and lyrad accepts a connection only from
//!   the uids it was told to trust (choragusd's). The old separate data socket
//!   handed a fresh ring for any stream id to anyone who could connect, which
//!   let a caller take over another app's stream; it is gone.
//! - **Sessions.** The trusted peer's connection is a session. Every stream
//!   belongs to it: when it ends (choragusd exits or restarts) every stream is
//!   closed, and a new connection starts with none.
//! - **Runs until stopped**, reopening the device if it goes away, and plays
//!   silence for a stream with no ring (no demo tones).
//! - **A bad stream is only noise in its own slot.** Samples are checked before
//!   they are summed ([`sanitize`]): one NaN no longer silences every app.
//!
//! The engine state ([`Engine`]) is host-testable; [`run`] is the FreeBSD loop.

use crate::gain::Gain;
use crate::oss::OssSink;
use crate::ring::Ring;
use lyra_protocol::{Ctl, FRAME_LEN};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;

/// The most streams lyrad mixes at once (lyra-v1.md §5).
pub const MAX_STREAMS: usize = 64;
/// Ring depth per stream, in frames (A4 sizes it from the stream's `buffer_ms`).
pub const RING_FRAMES: u64 = 4096;
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

/// A command for the engine, applied at a period boundary.
pub enum Cmd {
    /// A trusted peer connected: session `gen` begins with no streams.
    Begin(u64),
    /// Session `gen`'s connection ended: close its streams.
    End(u64),
    Ctl(u64, Ctl),
    /// The consumer end of the ring made for `stream` by session `gen`.
    Attach(u64, u32, Ring),
}

struct Slot {
    id: u32,
    gain: Gain,
    ring: Option<Ring>,
}

/// The mix state: the current session and its streams.
pub struct Engine {
    gen: u64,
    slots: Vec<Slot>,
    rate: f32,
    channels: usize,
    tmp: Vec<f32>,
}

impl Engine {
    pub fn new(rate: u32, channels: usize, period_frames: usize) -> Self {
        Engine { gen: 0, slots: Vec::new(), rate: rate as f32, channels, tmp: vec![0.0; period_frames * channels] }
    }

    pub fn streams(&self) -> usize {
        self.slots.len()
    }

    pub fn is_open(&self, stream: u32) -> bool {
        self.slots.iter().any(|s| s.id == stream)
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
                self.gen = g;
            }
            Cmd::End(g) if g == self.gen => {
                if !self.slots.is_empty() {
                    eprintln!("lyrad: session {g} ended; closing {} stream(s)", self.slots.len());
                }
                self.slots.clear();
            }
            Cmd::End(_) => {}
            Cmd::Ctl(g, _) | Cmd::Attach(g, _, _) if g != self.gen => {}
            Cmd::Ctl(_, c) => self.apply_ctl(c),
            Cmd::Attach(_, stream, ring) => {
                // Only an open stream takes a ring; a ring never creates a slot.
                if let Some(s) = self.slots.iter_mut().find(|s| s.id == stream) {
                    s.ring = Some(ring);
                }
            }
        }
    }

    fn apply_ctl(&mut self, c: Ctl) {
        match c {
            Ctl::OpenStream { stream } => {
                if self.is_open(stream) {
                    return;
                }
                if self.slots.len() >= MAX_STREAMS {
                    eprintln!("lyrad: OpenStream {stream} refused: {MAX_STREAMS} streams open");
                    return;
                }
                self.slots.push(Slot { id: stream, gain: Gain::new(1.0, self.rate, 20.0), ring: None });
            }
            Ctl::CloseStream { stream } => self.slots.retain(|s| s.id != stream),
            Ctl::SetGainDb { stream, db } => {
                if let Some(s) = self.slots.iter_mut().find(|s| s.id == stream) {
                    // A non-finite or absurd level from the wire is clamped too.
                    let db = if db.is_finite() { db.clamp(-120.0, 12.0) } else { -120.0 };
                    s.gain.set_target(if db <= -120.0 { 0.0 } else { 10f32.powf(db / 20.0) });
                }
            }
            Ctl::Reroute { .. } => {} // single sink in v1
            Ctl::RingFor { .. } => {}  // answered by the connection thread
        }
    }

    /// Mix one period into `mix` (interleaved, `period_frames × channels`).
    pub fn mix(&mut self, mix: &mut [f32]) {
        mix.iter_mut().for_each(|v| *v = 0.0);
        let ch = self.channels;
        for s in self.slots.iter_mut() {
            let got = match s.ring.as_ref() {
                Some(r) => r.read(&mut self.tmp) as usize,
                None => 0,
            };
            self.tmp[got * ch..].iter_mut().for_each(|v| *v = 0.0);
            sanitize(&mut self.tmp);
            s.gain.process(&mut self.tmp);
            for (m, t) in mix.iter_mut().zip(&self.tmp) {
                *m += *t;
            }
        }
    }
}

/// Where lyrad listens and whom it trusts.
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

/// Serve one trusted connection as session `gen`: forward control frames to the
/// engine and answer `RingFor` with a ring fd on the same connection.
fn serve_session(mut conn: UnixStream, gen: u64, tx: mpsc::Sender<Cmd>) {
    let _ = tx.send(Cmd::Begin(gen));
    // This session's open streams, so RingFor answers only for those.
    let mut open = std::collections::HashSet::new();
    let mut frame = [0u8; FRAME_LEN];
    while conn.read_exact(&mut frame).is_ok() {
        let Some(c) = Ctl::decode(&frame) else { continue };
        match c {
            Ctl::RingFor { stream } => {
                let is_open = open.contains(&stream);
                let made = if is_open { Ring::create_anon(RING_FRAMES, 2, false).ok() } else { None };
                match made {
                    Some((ring, fd)) => {
                        if crate::fdpass::send_fd(conn.as_raw_fd(), fd.as_raw_fd()).is_ok() {
                            let _ = tx.send(Cmd::Attach(gen, stream, ring));
                        }
                    }
                    None => {
                        // No ring: answer with the filler byte alone, so the
                        // peer's recv_fd fails instead of waiting forever.
                        use std::io::Write;
                        let _ = conn.write_all(&[0u8]);
                    }
                }
            }
            other => {
                match other {
                    Ctl::OpenStream { stream } => { open.insert(stream); }
                    Ctl::CloseStream { stream } => { open.remove(&stream); }
                    _ => {}
                }
                let _ = tx.send(Cmd::Ctl(gen, other));
            }
        }
    }
    let _ = tx.send(Cmd::End(gen));
}

/// Bind the socket and accept trusted peers forever (on its own thread).
pub fn listen(cfg: &Config, tx: mpsc::Sender<Cmd>) -> io::Result<()> {
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = UnixListener::bind(&cfg.socket)?;
    {
        // Group-connectable; the directory (owned _lyra, group _choragus, 0750)
        // and the peer-uid check below are what keep everyone else out.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o660))?;
    }
    let peers = cfg.peers.clone();
    std::thread::spawn(move || {
        let mut gen = 0u64;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let uid = match lyra_protocol::peer::peer_uid(conn.as_raw_fd()) {
                Ok(u) => u,
                Err(_) => continue,
            };
            if !peer_allowed(uid, &peers) {
                eprintln!("lyrad: refused a connection from uid {uid} (not a trusted peer)");
                continue; // dropping `conn` hangs up
            }
            gen += 1;
            eprintln!("lyrad: session {gen} from uid {uid}");
            let tx = tx.clone();
            std::thread::spawn(move || serve_session(conn, gen, tx));
        }
    });
    Ok(())
}

/// The service: listen, then mix to the device forever, reopening it if it goes.
pub fn run(cfg: Config) -> ! {
    let (tx, rx) = mpsc::channel::<Cmd>();
    if let Err(e) = listen(&cfg, tx) {
        eprintln!("lyrad: bind {}: {e}", cfg.socket.display());
        std::process::exit(1);
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
    let mut dump = std::env::var("LYRA_DUMP").ok().and_then(|p| std::fs::File::create(p).ok());
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
        loop {
            while let Ok(c) = rx.try_recv() {
                engine.apply(c);
            }
            engine.mix(&mut mix);
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

    #[test]
    fn sanitize_zeroes_non_finite_and_clamps() {
        let mut b = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e9, -1e9, 0.5];
        sanitize(&mut b);
        assert_eq!(b, [0.0, 0.0, 0.0, SAMPLE_LIMIT, -SAMPLE_LIMIT, 0.5]);
    }

    /// ★ One stream writes NaN, infinity and huge values; the other's tone comes
    /// through exactly. Before, one NaN made the whole sum NaN: silence for all.
    #[test]
    fn a_poisoned_stream_does_not_touch_the_others() {
        let name_a = format!("/lyra-test-a-{}", std::process::id());
        let name_b = format!("/lyra-test-b-{}", std::process::id());
        let pa = Ring::create(&name_a, 8, 2).unwrap();
        let pb = Ring::create(&name_b, 8, 2).unwrap();
        let ca = Ring::open(&name_a, false).unwrap();
        let cb = Ring::open(&name_b, false).unwrap();
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        for (id, r) in [(1, ca), (2, cb)] {
            e.apply(Cmd::Ctl(1, Ctl::OpenStream { stream: id }));
            e.apply(Cmd::Attach(1, id, r));
        }
        pa.write(&[f32::NAN, f32::INFINITY, 1e30, f32::NEG_INFINITY, f32::NAN, f32::NAN, f32::NAN, f32::NAN]);
        pb.write(&[0.25; 8]);
        let mut mix = [0.0f32; 8];
        e.mix(&mut mix);
        // stream 1 contributes 0 for the non-finite samples and ±4.0 at most for 1e30.
        assert_eq!(mix[0], 0.25);
        assert_eq!(mix[1], 0.25);
        assert_eq!(mix[2], 0.25 + SAMPLE_LIMIT);
        assert!(mix.iter().all(|v| v.is_finite()), "{mix:?}");
        assert_eq!(&mix[3..], &[0.25; 5]);
    }

    #[test]
    fn a_ring_never_creates_a_stream() {
        let name = format!("/lyra-test-c-{}", std::process::id());
        let _p = Ring::create(&name, 8, 2).unwrap();
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        e.apply(Cmd::Attach(1, 5, Ring::open(&name, false).unwrap()));
        assert_eq!(e.streams(), 0);
    }

    #[test]
    fn commands_from_a_stale_session_are_ignored() {
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        e.apply(Cmd::Ctl(1, Ctl::OpenStream { stream: 7 }));
        e.apply(Cmd::Begin(2)); // choragusd restarted
        assert_eq!(e.streams(), 0, "a new session starts empty");
        e.apply(Cmd::Ctl(1, Ctl::OpenStream { stream: 8 }));
        e.apply(Cmd::End(1));
        assert_eq!(e.streams(), 0, "the old session can't open or close anything");
        e.apply(Cmd::Ctl(2, Ctl::OpenStream { stream: 9 }));
        assert!(e.is_open(9));
        e.apply(Cmd::End(2));
        assert_eq!(e.streams(), 0, "the session's end closes its streams");
    }

    #[test]
    fn stream_count_is_capped() {
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        for id in 0..(MAX_STREAMS as u32 + 10) {
            e.apply(Cmd::Ctl(1, Ctl::OpenStream { stream: id }));
        }
        assert_eq!(e.streams(), MAX_STREAMS);
    }

    #[test]
    fn wild_gains_are_clamped() {
        let mut e = Engine::new(48_000, 2, 4);
        e.apply(Cmd::Begin(1));
        e.apply(Cmd::Ctl(1, Ctl::OpenStream { stream: 1 }));
        for db in [f32::NAN, f32::INFINITY, 1e9] {
            e.apply(Cmd::Ctl(1, Ctl::SetGainDb { stream: 1, db }));
            let s = &e.slots[0];
            assert!(s.gain.current().is_finite());
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

    /// The socket refuses an untrusted uid and serves a trusted one: a session
    /// opens a stream and `RingFor` on an unopened stream returns no fd.
    #[test]
    fn socket_serves_trusted_peer_and_ring_for_needs_an_open_stream() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("lyrad-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let me = unsafe { libc::getuid() };
        // Untrusted: trust only some other uid.
        let cfg = Config { socket: dir.join("a.sock"), peers: vec![me.wrapping_add(1)], rate: 48_000, channels: 2, period_frames: 4, nfrags: 2 };
        let (tx, rx) = mpsc::channel();
        listen(&cfg, tx).unwrap();
        let mut c = UnixStream::connect(&cfg.socket).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(c.read(&mut b).unwrap_or(0), 0, "hung up on the untrusted peer");
        assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "no session began");

        // Trusted: ourselves.
        let cfg = Config { socket: dir.join("b.sock"), peers: vec![me], ..cfg };
        let (tx, rx) = mpsc::channel();
        listen(&cfg, tx).unwrap();
        let mut c = UnixStream::connect(&cfg.socket).unwrap();
        c.write_all(&Ctl::RingFor { stream: 3 }.encode()).unwrap();
        assert!(crate::fdpass::recv_fd(c.as_raw_fd()).is_err(), "no ring for a stream that isn't open");
        c.write_all(&Ctl::OpenStream { stream: 3 }.encode()).unwrap();
        let mut e = Engine::new(48_000, 2, 4);
        let t = std::time::Duration::from_secs(2);
        e.apply(rx.recv_timeout(t).unwrap()); // Begin
        e.apply(rx.recv_timeout(t).unwrap()); // Open
        assert!(e.is_open(3));
        drop(c);
        e.apply(rx.recv_timeout(t).unwrap()); // End
        assert_eq!(e.streams(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
