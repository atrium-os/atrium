//! lyra-feed — a stand-in audio source that feeds REAL samples into a stream's
//! data-plane ring (the other half of the picture from the control plane).
//!
//! lyrad's mixer reads each stream's audio from a shared-memory [`Ring`] named by
//! the stream id; this writes a tone into `/lyra_pcm_<id>`, paced ~real-time. The
//! control plane (choragusd → lyrad: open the slot, set its gain) and the data
//! plane (this → the ring → the mix) are separate edges that meet at the id — so
//! a stream opened + ducked by policy carries the audio fed here.
//!
//! usage: lyra-feed <stream_id> <freq_hz> <secs>

use lyra::ring::Ring;

const RATE: f32 = 48_000.0;
const CHUNK: usize = 256; // frames per write

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let id: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let freq: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(440.0);
    let secs: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4.0);

    let name = format!("/lyra_pcm_{id}");
    let _ = lyra_unlink(&name); // clear any stale segment
    let ring = match Ring::create(&name, 4096, 2) {
        Ok(r) => r,
        Err(e) => { eprintln!("lyra-feed: create {name}: {e}"); std::process::exit(1); }
    };
    eprintln!("lyra-feed: feeding stream {id} ({freq:.0} Hz) into {name} for {secs}s");

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
        // write the whole chunk; the ring blocks us when full, so lyrad's own
        // consumption paces the feed (backpressure) — no fixed sleep to drift.
        let mut off = 0usize;
        while off < CHUNK {
            let w = ring.write(&buf[off * 2..]) as usize;
            off += w;
            if w == 0 {
                unsafe { libc::usleep(500) };
            }
        }
    }
    eprintln!("lyra-feed: stream {id} done");
    // `ring` drops here → the segment is unlinked; lyrad then reads silence.
}

/// Best-effort shm_unlink (the ring uses shm_open; a stale one would EEXIST).
fn lyra_unlink(name: &str) -> std::io::Result<()> {
    let c = std::ffi::CString::new(name).unwrap();
    unsafe { libc::shm_unlink(c.as_ptr()) };
    Ok(())
}
