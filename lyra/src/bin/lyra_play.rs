//! lyra-play — the reference audio app, using ONE front door.
//!
//! It calls `lyra_protocol::client::play`: connect to **choragusd**, register
//! under a role (declaring requested capabilities), and receive the data-plane
//! ring — which choragusd brokered from lyrad after applying policy (routing,
//! ducking of others, the §9 capability check). The app never touches the RT
//! engine directly. It then writes a tone into the ring; lyrad mixes it.
//!
//! This is the payoff of the client-library refactor: an app depends only on
//! `lyra-protocol` (wire + ring + client), not on the engine or the policy crate.
//!
//! usage: lyra-play <choragus_sock> <role> <secs> [app_id] [monitor|mic]

use lyra_protocol::app::{role_byte, CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
use lyra_protocol::client;

const RATE: f32 = 48_000.0;
const CHUNK: usize = 256;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = args.get(1).map(String::as_str).unwrap_or("/tmp/choragus.sock");
    let role = args.get(2).and_then(|s| role_byte(s)).unwrap_or(0);
    let secs: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let app_id = args.get(4).map(String::as_str).unwrap_or("org.atrium.player");
    let mut caps = CAP_AUDIO;
    if args.iter().any(|a| a == "monitor") { caps |= CAP_MONITOR; }
    if args.iter().any(|a| a == "mic") { caps |= CAP_MICROPHONE; }
    // distinct tone per role, and distinct from lyrad's synth fallback (440/660)
    // so the spectral proof shows the REAL fed audio. LYRA_PLAY_FREQ overrides it
    // (the multi-session demo gives two same-role streams distinct tones).
    let freq = std::env::var("LYRA_PLAY_FREQ")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(500.0 + 90.0 * role as f32);

    let stream = match client::play(sock, app_id, role, caps) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lyra-play: play denied/failed: {e} (granted? choragusd up?)");
            std::process::exit(1);
        }
    };
    eprintln!("lyra-play: '{app_id}' registered as role {role} -> stream {} ({freq:.0} Hz), playing {secs}s",
        stream.stream_id);

    let step = 2.0 * std::f32::consts::PI * freq / RATE;
    let mut phase = 0.0f32;
    let mut buf = vec![0.0f32; CHUNK * 2];
    let chunks = (secs * RATE / CHUNK as f32) as u64;
    for _ in 0..chunks {
        for f in 0..CHUNK {
            let s = phase.sin() * 0.25;
            buf[f * 2] = s;
            buf[f * 2 + 1] = s;
            phase += step;
            if phase > 2.0 * std::f32::consts::PI { phase -= 2.0 * std::f32::consts::PI; }
        }
        let mut off = 0usize;
        while off < CHUNK {
            let w = stream.write(&buf[off * 2..]) as usize;
            off += w;
            if w == 0 {
                unsafe { libc::usleep(500) };
            }
        }
    }
    eprintln!("lyra-play: done (dropping the session closes the stream)");
    // dropping `stream` closes the choragusd session -> choragusd closes the
    // stream and un-ducks anything that was ducked for it.
}
