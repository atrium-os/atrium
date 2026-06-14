//! lyra-feed — a stand-in audio source feeding REAL samples into a stream's
//! data-plane ring. The data plane, complementary to the control plane.
//!
//! It connects to lyrad's **data socket**, asks to feed a stream id, and receives
//! the ring's anonymous-shm fd over `SCM_RIGHTS` (the Carillon/fd-passing shape):
//! no shared name, no name-reuse race, and the fd is the capability to write that
//! stream. It maps the producer end and writes a tone, paced by the ring's
//! backpressure (lyrad's own consumption sets the rate). The control plane
//! (choragusd → lyrad: open the slot, set its gain) and this data edge meet at
//! the stream id — a stream opened + ducked by policy carries the audio fed here.
//!
//! usage: lyra-feed <stream_id> <freq_hz> <secs> [data_socket]

use lyra::fdpass::recv_fd;
use lyra::ring::Ring;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

const RATE: f32 = 48_000.0;
const CHUNK: usize = 256; // frames per write

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let id: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let freq: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(440.0);
    let secs: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let data_socket = args.get(4).map(String::as_str).unwrap_or("/tmp/lyrad.ctl.data");

    // connect to lyrad's data socket, request the stream, receive the ring fd.
    use std::io::Write;
    let mut sock = match UnixStream::connect(data_socket) {
        Ok(s) => s,
        Err(e) => { eprintln!("lyra-feed: connect {data_socket}: {e} (is lyrad --control up?)"); std::process::exit(1); }
    };
    if sock.write_all(&id.to_le_bytes()).is_err() {
        eprintln!("lyra-feed: send stream id failed");
        std::process::exit(1);
    }
    let fd = match recv_fd(sock.as_raw_fd()) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("lyra-feed: no ring fd from lyrad: {e}"); std::process::exit(1); }
    };
    let ring = match Ring::from_fd(fd, true) {
        Ok(r) => r,
        Err(e) => { eprintln!("lyra-feed: map ring: {e}"); std::process::exit(1); }
    };
    eprintln!("lyra-feed: feeding stream {id} ({freq:.0} Hz) for {secs}s (fd-passed ring)");

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
        // backpressure paces us: the ring blocks when full, so lyrad's consumption
        // sets the rate. (Keep `sock` alive so lyrad sees the source as connected.)
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
    drop(sock);
}
