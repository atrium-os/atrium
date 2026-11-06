//! The windowed player: one loop for a window of its own (`atrium-player
//! FILE`) and for a Limen `media-player` child, which is the same window
//! shown in the parent's slot and driven by the parent's messages
//! (limen.md §2.2, atrium-player.md §6–7).
use std::fs::File;
use std::io;
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use limen::media_player::{code, Source as LoadSource, ToChild, ToParent, RANGES_MAX, TIME_HZ_MAX};

use crate::audio_out::AudioOut;
use crate::controls::{Action, Controls};
use crate::fresco_sink::FrescoSink;
use crate::supervisor::{Options, Player, PlayerEvent, Source};
use crate::wire::err;

/// What the app hears from whoever embeds it.
#[derive(Debug)]
pub enum HostEvent {
    Msg(ToChild),
    /// `Load` with its descriptor: a read-only file or pipe, checked and
    /// narrowed by limend.
    Load(std::os::fd::OwnedFd),
    /// The slot is hidden: pause, and keep the position.
    Suspend,
    Resume,
    /// The parent detached us, or the channel broke.
    Gone,
}

pub trait Host {
    /// Waiting at most `wait`.
    fn poll(&mut self, wait: Duration) -> Vec<HostEvent>;
    fn emit(&mut self, m: ToParent);
    /// Limen's attribution ticket for Lyra, for an embedded player.
    fn audio_ticket(&self) -> Option<[u8; 16]> {
        None
    }
}

/// A window of its own: nobody to tell.
pub struct Standalone;
impl Host for Standalone {
    fn poll(&mut self, _: Duration) -> Vec<HostEvent> {
        Vec::new()
    }
    fn emit(&mut self, _: ToParent) {}
}

/// A Limen `media-player` child: the parent's messages arrive through
/// limend, checked by [`ToChild::decode`]; anything out of shape is dropped.
pub struct LimenHost {
    pub child: limen::client::Child,
    pub embed: limen::proto::EmbedId,
    pub dropped: u32,
    pub ticket: Option<[u8; 16]>,
}

impl Host for LimenHost {
    fn poll(&mut self, wait: Duration) -> Vec<HostEvent> {
        use limen::proto::ChildEvent;
        let mut out = Vec::new();
        // A zero read timeout is refused by the socket; aqueduct keeps a
        // message cut off by a timeout, so short ones are safe.
        let mut wait = wait.max(Duration::from_millis(1));
        loop {
            let ev = match self.child.poll(wait) {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(_) => {
                    out.push(HostEvent::Gone);
                    break;
                }
            };
            wait = Duration::from_millis(1);
            match ev {
                ChildEvent::Message { embed, bytes } if embed == self.embed => match ToChild::decode(&bytes) {
                    Some(m) => out.push(HostEvent::Msg(m)),
                    None => self.dropped += 1,
                },
                ChildEvent::MessageFd { embed, bytes } => {
                    // Taken whatever it is, so none is left for a later message.
                    let fd = self.child.take_fd();
                    match (embed == self.embed, ToChild::decode(&bytes), fd) {
                        (true, Some(ToChild::Load { source: LoadSource::Fd, .. }), Some(fd)) => out.push(HostEvent::Load(fd)),
                        _ => self.dropped += 1,
                    }
                }
                ChildEvent::Suspended { embed } if embed == self.embed => out.push(HostEvent::Suspend),
                ChildEvent::Resumed { embed } if embed == self.embed => out.push(HostEvent::Resume),
                ChildEvent::Detaching { embed } if embed == self.embed => out.push(HostEvent::Gone),
                ChildEvent::AudioTicket { embed, ticket } if embed == self.embed => self.ticket = Some(ticket),
                _ => {}
            }
        }
        out
    }

    fn emit(&mut self, m: ToParent) {
        let _ = self.child.emit(self.embed, m.encode());
    }

    fn audio_ticket(&self) -> Option<[u8; 16]> {
        self.ticket
    }
}

pub struct App<H: Host> {
    pub host: H,
    sink: Option<FrescoSink>,
    pub player: Option<Player<FrescoSink>>,
    pub controls: Controls,
    opts: Options,
    duration: Option<f64>,
    ready_sent: bool,
    video: Option<(u32, u32)>,
    want_play: bool,
    suspended: bool,
    last_time: Instant,
    last_frame: Instant,
    ended_sent: bool,
    exit_at_end: bool,
    ended_at: Option<Instant>,
    pcm: Vec<f32>,
    pub frames: u32,
    pub done: bool,
    /// Buffered ranges of the first track the feed reported, in seconds.
    shown_track: Option<u8>,
    pub audio: Option<AudioOut>,
    /// choragusd's socket.
    pub audio_socket: String,
    volume: f32,
    /// Since when play is waiting for the audio to start.
    audio_wait: Option<Instant>,
}

fn secs(ns: i64) -> f64 {
    ns as f64 / 1e9
}

fn ns(s: f64) -> i64 {
    if s.is_infinite() {
        i64::MAX
    } else {
        (s * 1e9) as i64
    }
}

fn limen_code(c: u32) -> u16 {
    match c {
        err::DECODE => code::DECODE,
        err::UNSUPPORTED => code::UNSUPPORTED,
        err::TOO_BIG => code::TOO_BIG,
        err::STATE => code::STATE,
        _ => code::DECODE,
    }
}

impl<H: Host> App<H> {
    pub fn new(host: H, conn: Connection, window: u32, size: (u32, u32), opts: Options) -> Self {
        App {
            host,
            sink: Some(FrescoSink::new(conn, window, size)),
            player: None,
            controls: Controls::new(size),
            opts,
            duration: None,
            ready_sent: false,
            video: None,
            want_play: false,
            suspended: false,
            last_time: Instant::now(),
            last_frame: Instant::now(),
            ended_sent: false,
            exit_at_end: false,
            ended_at: None,
            pcm: vec![0f32; 48_000 * 2],
            frames: 0,
            done: false,
            shown_track: None,
            audio: None,
            audio_socket: std::env::var("ATRIUM_AUDIO_SOCKET").unwrap_or_else(|_| crate::audio_out::DEFAULT_SOCKET.into()),
            volume: 1.0,
            audio_wait: None,
        }
    }

    pub fn sink(&mut self) -> &mut FrescoSink {
        match self.player.as_mut() {
            Some(p) => &mut p.sink,
            None => self.sink.as_mut().expect("a sink until the player takes it"),
        }
    }

    /// Start playing a whole resource now (the standalone player).
    pub fn play_file(&mut self, f: File, exit_at_end: bool) -> io::Result<()> {
        self.exit_at_end = exit_at_end;
        self.start(Source::File(f))?;
        self.set_playing(true);
        Ok(())
    }

    fn start(&mut self, source: Source) -> io::Result<()> {
        let Some(sink) = self.sink.take() else {
            // One source per embed: a new `src` is a new embed.
            self.host.emit(ToParent::Error { code: code::STATE });
            return Ok(());
        };
        let mut p = Player::new(self.opts.clone(), sink, source)?;
        // Paused until Play, with the first frame showing as the poster.
        p.set_paused(true);
        p.show_next();
        self.player = Some(p);
        Ok(())
    }

    fn set_playing(&mut self, play: bool) {
        self.want_play = play;
        self.controls.playing = play;
        self.controls.poke();
        if play && self.player.as_ref().is_some_and(|p| p.ended()) && self.ended_sent {
            // Play after the end starts again.
            self.seek(0.0, false);
        }
        self.audio_wait = None;
        self.sync_paused();
        self.host.emit(if play { ToParent::Playing } else { ToParent::Paused });
    }

    /// Video runs when playing, shown and, with sound, once the audio clock
    /// runs (or has failed to start within a second).
    fn sync_paused(&mut self) {
        let run = self.want_play && !self.suspended;
        if let Some(a) = self.audio.as_mut() {
            a.set_playing(run);
        }
        let audio_ready = match self.audio.as_ref() {
            None => true,
            Some(a) => a.map().is_some() || self.audio_wait.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(1),
        };
        if !run {
            self.audio_wait = None;
        }
        let ended = self.player.as_ref().is_some_and(|p| p.ended());
        if let Some(p) = self.player.as_mut() {
            p.set_paused(!(run && (audio_ready || ended)));
        }
    }

    fn seek(&mut self, to: f64, from_controls: bool) {
        let to = match self.duration {
            Some(d) if d.is_finite() => to.clamp(0.0, d),
            _ => to.max(0.0),
        };
        if let Some(p) = self.player.as_mut() {
            p.seek(ns(to));
        }
        if let Some(a) = self.audio.as_mut() {
            a.seek(ns(to));
        }
        self.audio_wait = None;
        self.sink().clock.av = None;
        self.ended_sent = false;
        self.ended_at = None;
        self.controls.position_s = to;
        if from_controls {
            self.host.emit(ToParent::Seeking { secs: to });
        }
    }

    fn on_host(&mut self, ev: HostEvent) -> io::Result<()> {
        let m = match ev {
            HostEvent::Gone => {
                self.done = true;
                return Ok(());
            }
            HostEvent::Suspend | HostEvent::Resume => {
                self.suspended = matches!(ev, HostEvent::Suspend);
                self.audio_wait = None;
                self.sync_paused();
                return Ok(());
            }
            HostEvent::Load(fd) => return self.start(Source::File(File::from(fd))),
            HostEvent::Msg(m) => m,
        };
        fn feed<H: Host>(app: &mut App<H>) -> Option<&mut Player<FrescoSink>> {
            if app.player.is_none() {
                app.host.emit(ToParent::Error { code: code::STATE });
            }
            app.player.as_mut()
        }
        match m {
            // A `Load` of a descriptor comes as HostEvent::Load; without one
            // it is out of place.
            ToChild::Load { source: LoadSource::Fd, .. } => self.host.emit(ToParent::Error { code: code::STATE }),
            // No jailed fetcher in the player yet.
            ToChild::Load { source: LoadSource::Url(_), .. } => self.host.emit(ToParent::Error { code: code::NETWORK }),
            ToChild::OpenSource => self.start(Source::Feed(Vec::new()))?,
            ToChild::AddTrack { track, mime } => {
                if !atrium_media_demux::mime_supported(&mime) {
                    self.host.emit(ToParent::Error { code: code::UNSUPPORTED });
                } else if let Some(p) = feed(self) {
                    p.add_track(track, mime);
                }
            }
            ToChild::Append { track, bytes, timestamp_offset } => {
                if let Some(p) = feed(self) {
                    p.append(track, bytes, timestamp_offset.map(ns));
                }
            }
            ToChild::Remove { track, start, end } => {
                if let Some(p) = feed(self) {
                    p.remove(track, ns(start), ns(end));
                }
            }
            ToChild::Abort { track } => {
                if let Some(p) = feed(self) {
                    p.abort(track);
                }
            }
            ToChild::EndOfStream { error } => {
                if error.is_some() {
                    self.set_playing(false);
                } else if let Some(p) = feed(self) {
                    p.end_of_stream();
                }
            }
            ToChild::SetDuration { secs } => {
                self.duration = Some(secs);
                self.controls.duration_s = if secs.is_finite() { secs } else { 0.0 };
            }
            ToChild::CanPlay { mime } => {
                let supported = atrium_media_demux::mime_supported(&mime);
                self.host.emit(ToParent::CanPlay { mime, supported });
            }
            ToChild::Play => self.set_playing(true),
            ToChild::Pause | ToChild::Stop => self.set_playing(false),
            ToChild::Seek { secs } => self.seek(secs, false),
            ToChild::SetVolume { volume } => {
                self.volume = volume;
                if let Some(a) = self.audio.as_mut() {
                    a.set_volume(volume);
                }
            }
            // Rate (WSOLA) is still to come; the value is checked by decode.
            ToChild::SetRate { .. } => {}
        }
        Ok(())
    }

    fn ready(&mut self) {
        if !self.ready_sent {
            self.ready_sent = true;
            let (width, height) = self.video.unwrap_or((0, 0));
            self.host.emit(ToParent::Ready { duration_secs: self.duration.unwrap_or(f64::INFINITY), width, height });
        }
    }

    fn on_player(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::VideoTrack { width, height, bit_depth } => {
                self.video = Some((width, height));
                println!("video-track {width}x{height} {bit_depth}-bit");
            }
            PlayerEvent::AudioTrack { rate, channels } => {
                if self.audio.is_none() {
                    match AudioOut::open(&self.audio_socket, self.host.audio_ticket(), rate, channels) {
                        Ok(mut a) => {
                            if self.volume < 1.0 {
                                a.set_volume(self.volume);
                            }
                            self.audio = Some(a);
                            self.sync_paused();
                        }
                        // No sound for us (no grant, or no Lyra): play silent.
                        Err(e) => eprintln!("atrium-player: no audio: {e}"),
                    }
                }
            }
            PlayerEvent::Sandboxed(_) | PlayerEvent::Probe { .. } | PlayerEvent::Restarted => {}
            PlayerEvent::Duration(d) => {
                if self.duration.is_none() {
                    self.duration = Some(d);
                    self.controls.duration_s = d;
                }
            }
            PlayerEvent::Frame { pts_ns } => {
                self.frames += 1;
                self.last_frame = Instant::now();
                self.controls.position_s = secs(pts_ns);
                self.ready();
            }
            PlayerEvent::Appended { track } => self.host.emit(ToParent::Appended { track }),
            PlayerEvent::Buffered { track, ranges } => {
                let r: Vec<(f64, f64)> = ranges.iter().map(|&(a, b)| (secs(a).max(0.0), secs(b).max(0.0))).collect();
                if self.shown_track.is_none_or(|t| t == track) {
                    self.shown_track = Some(track);
                    self.controls.buffered = r.clone();
                }
                self.host.emit(ToParent::Buffered { track, ranges: r.into_iter().take(RANGES_MAX).collect() });
            }
            PlayerEvent::Ended => self.ended_at = Some(Instant::now()),
            PlayerEvent::Error(c) => {
                println!("error {c}");
                self.host.emit(ToParent::Error { code: limen_code(c) });
                if self.player.is_none() {
                    self.done = true;
                }
            }
        }
    }

    /// One turn of the loop.
    pub fn step(&mut self) -> io::Result<()> {
        for ev in self.host.poll(Duration::ZERO) {
            self.on_host(ev)?;
        }
        let mut evs = Vec::new();
        if let Some(p) = self.player.as_mut() {
            while let Some(ev) = p.poll(Duration::from_millis(1)) {
                evs.push(ev);
            }
        }
        for ev in evs {
            self.on_player(ev);
        }
        if let Some(p) = self.player.as_mut() {
            let ended = p.ended();
            match self.audio.as_mut() {
                Some(a) => {
                    if !a.pump(p, ended) {
                        eprintln!("atrium-player: Lyra went away; playing on silent");
                        self.audio = None;
                    }
                }
                // No sound: drop it while playing so the worker keeps
                // going; paused, the ring fills and the worker waits.
                None if self.want_play && !self.suspended => while p.read_pcm(&mut self.pcm).0 > 0 {},
                None => {}
            }
        }
        let map = self.audio.as_ref().and_then(|a| a.map());
        self.sink().clock.av = map;
        self.sync_paused();
        let (released, other) = self.sink().pump(Duration::from_millis(4))?;
        if let Some(p) = self.player.as_mut() {
            for i in released {
                p.release(i);
            }
        }
        for ev in other {
            self.on_fresco(ev);
        }
        if self.sink().closed {
            self.done = true;
        }
        // The end: everything decoded, and the last frame's had its time.
        if let Some(t) = self.ended_at {
            let playing = self.player.as_ref().is_some_and(|p| !p.paused());
            if playing && !self.ended_sent && t.elapsed() > Duration::from_millis(300) && self.last_frame.elapsed() > Duration::from_millis(300) {
                self.ended_sent = true;
                self.ready();
                self.host.emit(ToParent::Ended);
                if self.exit_at_end {
                    self.done = true;
                }
                self.want_play = false;
                self.controls.playing = false;
                if let Some(p) = self.player.as_mut() {
                    p.set_paused(true);
                }
            }
        }
        if self.want_play && !self.suspended && self.last_time.elapsed() >= Duration::from_millis(1000 / TIME_HZ_MAX as u64) {
            self.last_time = Instant::now();
            if let Some(t) = self.player.as_ref().and_then(|p| p.position_ns()) {
                self.host.emit(ToParent::Time { secs: secs(t).max(0.0) });
            }
        }
        let mut c = std::mem::replace(&mut self.controls, Controls::new((0, 0)));
        let r = c.draw(&mut self.sink().conn);
        self.controls = c;
        r
    }

    fn on_fresco(&mut self, ev: Event) {
        let action = match ev {
            Event::Resized { width, height, .. } => {
                self.controls.size = (width, height);
                None
            }
            Event::PointerMotion { .. } => {
                self.controls.poke();
                None
            }
            Event::PointerButton { x, y, button: 1, pressed: true, .. } => self.controls.press(x, y),
            Event::Key { hid_usage, pressed: true, .. } => self.controls.key(hid_usage),
            _ => None,
        };
        match action {
            Some(Action::Toggle) => self.set_playing(!self.want_play),
            Some(Action::Seek(f)) => {
                let d = self.controls.duration_s;
                self.seek(f * d, true);
            }
            Some(Action::Step(s)) => {
                let to = self.controls.position_s + s;
                self.seek(to, true);
            }
            None => {}
        }
    }
}
