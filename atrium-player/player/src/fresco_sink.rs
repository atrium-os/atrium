//! Frames into a Fresco video layer (fresco-video.md §3–4, V1 and V2): the
//! layer's buffers come from frescod, the worker writes them, and each is
//! queued with the `CLOCK_MONOTONIC` instant it should show at.
use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use fresco_client::{Connection, Event};
use fresco_protocol::video::format as vfmt;

use crate::supervisor::Sink;
use crate::wire::Format;

pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    #[allow(unsafe_code)]
    // SAFETY: one timespec we own.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Media time to wall time. With sound, the audio clock is the master
/// (`av`, from [`crate::audio_out`]); without, `CLOCK_MONOTONIC`, started at
/// the first frame after play or a seek.
#[derive(Debug)]
pub struct Clock {
    base: Option<(i64, i64)>,
    /// How far ahead of now the first frame is scheduled.
    pub lead_ns: i64,
    pub av: Option<crate::audio_out::AvMap>,
    /// Playback rate (media seconds per second), for the monotonic path.
    pub rate: f64,
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new(0)
    }
}

impl Clock {
    pub fn new(lead_ns: i64) -> Self {
        Clock { base: None, lead_ns, av: None, rate: 1.0 }
    }
    pub fn present_at(&mut self, pts_ns: i64) -> i64 {
        if let Some(m) = self.av {
            return m.at(pts_ns);
        }
        let lead = self.lead_ns;
        let (m, p) = *self.base.get_or_insert_with(|| (monotonic_ns() + lead, pts_ns));
        m + ((pts_ns - p) as f64 / self.rate) as i64
    }
    pub fn reset(&mut self) {
        self.base = None;
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub presented: u32,
    pub dropped: u32,
    /// |vblank - asked| over presented frames, ns.
    pub skew_sum: i64,
    pub skew_max: i64,
}

pub struct FrescoSink {
    pub conn: Connection,
    pub window: u32,
    pub win_size: (u32, u32),
    pub layer: Option<u32>,
    pub clock: Clock,
    seq: u32,
    /// seq → asked present time.
    asked: HashMap<u32, i64>,
    released: Vec<u32>,
    /// The layer's source size, to place it again on a resize.
    src: Option<(u32, u32)>,
    pub stats: Stats,
    pub closed: bool,
}

impl FrescoSink {
    pub fn new(conn: Connection, window: u32, win_size: (u32, u32)) -> Self {
        FrescoSink { conn, window, win_size, layer: None, clock: Clock::new(60_000_000), seq: 0, asked: HashMap::new(), released: Vec::new(), src: None, stats: Stats::default(), closed: false }
    }

    /// Read Fresco's events; returns the buffers given back, and the events
    /// that aren't the layer's (input, resizes) for the app.
    pub fn pump(&mut self, timeout: Duration) -> io::Result<(Vec<u32>, Vec<Event>)> {
        let mut other = Vec::new();
        let mut first = true;
        loop {
            let ev = if first { self.conn.wait_event(Some(timeout.max(Duration::from_millis(1))))? } else { self.conn.poll_event()? };
            first = false;
            let Some(ev) = ev else { break };
            match ev {
                Event::VideoReleased { layer, buffer, .. } if Some(layer) == self.layer => self.released.push(buffer),
                Event::VideoPresented { layer, seq, vblank_ns, dropped, .. } if Some(layer) == self.layer => {
                    self.stats.presented += 1;
                    self.stats.dropped += dropped;
                    if let Some(asked) = self.asked.remove(&seq) {
                        let skew = (vblank_ns as i64 - asked).abs();
                        self.stats.skew_sum += skew;
                        self.stats.skew_max = self.stats.skew_max.max(skew);
                    }
                }
                Event::CloseRequested { .. } => self.closed = true,
                Event::Resized { width, height, .. } => {
                    self.win_size = (width, height);
                    if let (Some(layer), Some((w, h))) = (self.layer, self.src) {
                        let rect = self.letterbox(w, h);
                        self.conn.video_layer_place(layer, Some(rect))?;
                    }
                    other.push(ev);
                }
                Event::VideoReleased { .. } | Event::VideoPresented { .. } => {}
                ev => other.push(ev),
            }
        }
        Ok((std::mem::take(&mut self.released), other))
    }

    /// The layer's rect in the window: the video scaled to fit, centred.
    pub fn letterbox(&self, w: u32, h: u32) -> [f32; 4] {
        let (ww, wh) = (self.win_size.0 as f32, self.win_size.1 as f32);
        let s = (ww / w as f32).min(wh / h as f32);
        let (vw, vh) = (w as f32 * s, h as f32 * s);
        [(ww - vw) / 2.0, (wh - vh) / 2.0, vw, vh]
    }
}

impl Sink for FrescoSink {
    fn layer(&mut self, format: Format, width: u32, height: u32, count: u32) -> io::Result<Vec<OwnedFd>> {
        let f = match format {
            Format::Nv12 => vfmt::NV12,
            Format::P010 => vfmt::P010,
        };
        // A restarted worker gets a new layer; the old one goes.
        if let Some(old) = self.layer.take() {
            let _ = self.conn.video_layer_destroy(old);
            self.released.clear();
            self.asked.clear();
        }
        let Some((layer, fds)) = self.conn.video_layer_create(self.window, f, width, height, count)? else {
            return Err(io::Error::other("frescod refused the video layer"));
        };
        let rect = self.letterbox(width, height);
        self.conn.video_layer_place(layer, Some(rect))?;
        self.layer = Some(layer);
        self.src = Some((width, height));
        Ok(fds)
    }

    fn frame(&mut self, index: u32, pts_ns: i64) -> bool {
        let Some(layer) = self.layer else { return true };
        let at = self.clock.present_at(pts_ns);
        self.seq = self.seq.wrapping_add(1);
        self.asked.insert(self.seq, at);
        if self.asked.len() > 64 {
            // Frames frescod passed over never get PRESENTED.
            let oldest = self.seq.wrapping_sub(64);
            self.asked.retain(|&s, _| s.wrapping_sub(oldest) < 64);
        }
        self.conn.video_frame(layer, index, self.seq, at.max(1) as u64).is_err()
    }

    fn discontinuity(&mut self) {
        self.clock.reset();
    }
}
