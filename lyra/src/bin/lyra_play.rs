//! lyra-play — the reference audio app (lyra-v1.md §5).
//!
//! Connects to choragusd with `lyra_protocol::client`, opens one stream,
//! primes its ring, starts it, plays a tone, drains and exits. Its identity is
//! its uid (choragusd resolves it through the launch registry); it sends no
//! name of its own.
//!
//! usage: lyra-play <choragus_sock> <role> <secs> [ignored...]
//!   LYRA_PLAY_FREQ (Hz, default by role), LYRA_PLAY_RATE (default 48000),
//!   LYRA_PLAY_CHANNELS (1, 2, 6 or 8; default 2), LYRA_PLAY_VOLUME (0..1),
//!   LYRA_PLAY_ONLY (a channel index: the tone in that channel only).

use lyra_protocol::client::Client;
use lyra_protocol::wire::{Role, StreamParams, ToApp};

const CHUNK: usize = 256;

fn env<T: std::str::FromStr>(k: &str) -> Option<T> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = args.get(1).map(String::as_str).unwrap_or("/atrium/sockets/audio/audio.sock");
    let (role, base) = match args.get(2).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("comms") | Some("communication") => (Role::Communication, 590.0),
        Some("notification") | Some("notify") => (Role::Notification, 680.0),
        Some("game") => (Role::Game, 770.0),
        Some("pro") => (Role::Pro, 860.0),
        _ => (Role::Media, 500.0),
    };
    let secs: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let freq: f32 = env("LYRA_PLAY_FREQ").unwrap_or(base);
    let rate: u32 = env("LYRA_PLAY_RATE").unwrap_or(48_000);
    let channels: u8 = env("LYRA_PLAY_CHANNELS").unwrap_or(2);
    let only: Option<usize> = env("LYRA_PLAY_ONLY");

    let mut c = match Client::connect(sock, None) {
        Ok(c) => c,
        Err(e) => { eprintln!("lyra-play: connect {sock}: {e}"); std::process::exit(1); }
    };
    let params = StreamParams { role, rate, channels, buffer_ms: 100, label: "Tone".into() };
    let s = match c.open(params) {
        Ok(s) => s,
        Err(e) => { eprintln!("lyra-play: open refused: {e}"); std::process::exit(1); }
    };
    if let Some(v) = env::<f32>("LYRA_PLAY_VOLUME") {
        let _ = c.set_volume(&s, v);
    }
    eprintln!("lyra-play: stream {} ({role:?}, {rate} Hz, {channels} ch, {freq:.0} Hz tone) for {secs}s", s.id);

    let ch = channels as usize;
    let step = 2.0 * std::f32::consts::PI * freq / rate as f32;
    let mut phase = 0.0f32;
    let mut buf = vec![0.0f32; CHUNK * ch];
    let total = (secs * rate as f32) as u64;
    let mut written = 0u64;
    let mut started = false;
    while written < total {
        for f in 0..CHUNK {
            let v = phase.sin() * 0.25;
            for k in 0..ch {
                buf[f * ch + k] = if only.is_none_or(|o| o == k) { v } else { 0.0 };
            }
            phase += step;
            if phase > 2.0 * std::f32::consts::PI { phase -= 2.0 * std::f32::consts::PI; }
        }
        let mut off = 0usize;
        while off < CHUNK {
            let w = s.write(&buf[off * ch..]) as usize;
            off += w;
            if w == 0 {
                // The ring is full: a moment to hear from choragusd.
                match c.try_event() {
                    Ok(Some(ToApp::StreamLost { .. })) | Err(_) => {
                        eprintln!("lyra-play: stream lost");
                        std::process::exit(2);
                    }
                    Ok(Some(ToApp::Underrun { frames, .. })) => eprintln!("lyra-play: underrun, {frames} frames"),
                    _ => {}
                }
                if !started {
                    // ring primed: start playing.
                    let _ = c.start(&s);
                    started = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        written += CHUNK as u64;
    }
    if !started {
        let _ = c.start(&s);
    }
    let _ = c.drain(&s);
    loop {
        match c.next_event() {
            Ok(ToApp::Drained { .. }) => break,
            Ok(ToApp::Underrun { frames, .. }) => eprintln!("lyra-play: underrun, {frames} frames"),
            Ok(ToApp::StreamLost { .. }) | Err(_) => { eprintln!("lyra-play: stream lost"); break; }
            Ok(_) => {}
        }
    }
    eprintln!("lyra-play: done");
}
