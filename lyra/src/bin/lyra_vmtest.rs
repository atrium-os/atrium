//! lyra-vmtest — the hostile and control clients for the VM checks
//! (lyra-v1.md §8; scripts/lyra-vm-check.sh).
//!
//!   lyra-vmtest hostile <choragus_sock> <secs>
//!       Open a stream and feed it garbage: NaN, ±infinity, 1e30, denormals,
//!       and a header rewritten every few milliseconds (wild head, tail,
//!       capacity and channel count). Another app's tone must come through
//!       untouched.
//!   lyra-vmtest ctl <control_sock> list
//!   lyra-vmtest ctl <control_sock> app <app_id> <volume 0..1>
//!   lyra-vmtest ctl <control_sock> mute <app_id> <0|1>
//!   lyra-vmtest ctl <control_sock> master <volume 0..1>
//!   lyra-vmtest ctl <control_sock> balance <-1..1>
//!   lyra-vmtest ctl <control_sock> layout <2|6|8>
//!   lyra-vmtest ctl <control_sock> spatial <0|1>
//!   lyra-vmtest ctl <control_sock> app-spatial <app_id> <0|1>
//!       The volume UI's requests (needs the `audio-control` grant).
//!   lyra-vmtest clock <choragus_sock> <rate> <secs> <offset_s>
//!       The clock test (lyra-v1.md §8 check 6): plays silence with a one-
//!       sample click at stream frame round(rate × (k + offset_s)) for every
//!       second k, and prints, for each click, the heard time the clock page
//!       predicted 30–300 ms ahead: `click <rate> <k> <frame> <predicted_ns>
//!       <rate_ppm>`. The host compares them with the dump and the device
//!       position (scripts/lyra-vm-check.sh clock).
//!   lyra-vmtest lyrad-raw <lyrad_sock>
//!       Connect to lyrad directly and ask for a stream. Prints `refused` if
//!       lyrad hangs up (only _choragus may talk to it), `ACCEPTED` otherwise.

use lyra_protocol::client::{Client, Control};
use lyra_protocol::wire::{self, FromControl, FromEngine, Role, StreamParams, ToControl, ToEngine};
use std::time::{Duration, Instant};

fn die(m: String) -> ! {
    eprintln!("lyra-vmtest: {m}");
    std::process::exit(1);
}

fn hostile(sock: &str, secs: f32) {
    let mut c = Client::connect(sock, None).unwrap_or_else(|e| die(format!("connect {sock}: {e}")));
    let params = StreamParams { role: Role::Media, rate: 48_000, channels: 2, buffer_ms: 50, label: "Hostile".into() };
    let s = c.open(params).unwrap_or_else(|e| die(format!("open: {e}")));
    c.start(&s).unwrap_or_else(|e| die(format!("start: {e}")));
    let junk = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30, f32::MIN_POSITIVE / 4.0, f32::MAX];
    let buf: Vec<f32> = (0..512).map(|i| junk[i % junk.len()]).collect();
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    while t0.elapsed().as_secs_f32() < secs {
        let _ = s.write(&buf);
        n += 1;
        if n % 4 == 0 {
            // xorshift: wild indices, including ones far ahead and behind.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let cap = [0, 3, 1 << 40, u64::MAX][(x % 4) as usize];
            let ch = [0, 9, 1 << 20, u64::MAX][((x >> 8) % 4) as usize];
            s.ring.scribble_header(cap, ch, x, x.rotate_left(17));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    eprintln!("lyra-vmtest: hostile stream {} fed {n} garbage writes for {secs}s", s.id);
}

fn ctl(sock: &str, args: &[String]) {
    let mut u = Control::connect(sock).unwrap_or_else(|e| die(format!("control {sock}: {e}")));
    let a = |i: usize| args.get(i).cloned().unwrap_or_else(|| die("missing argument".into()));
    let req = match a(0).as_str() {
        "list" => ToControl::List,
        "app" => ToControl::SetAppVolume { app_id: a(1), volume: a(2).parse().unwrap_or(1.0) },
        "mute" => ToControl::SetAppMute { app_id: a(1), muted: a(2) == "1" },
        "master" => ToControl::SetMaster { volume: a(1).parse().unwrap_or(1.0) },
        "balance" => ToControl::SetBalance { output: 0, balance: a(1).parse().unwrap_or(0.0) },
        "layout" => ToControl::SetLayout { output: 0, channels: a(1).parse().unwrap_or(2) },
        "spatial" => ToControl::SetSpatial { output: 0, on: a(1) == "1" },
        "app-spatial" => ToControl::SetAppSpatial { app_id: a(1), on: a(2) == "1" },
        other => die(format!("unknown ctl command {other}")),
    };
    u.send(&req).unwrap_or_else(|e| die(format!("send: {e}")));
    // Every request here changes or lists the apps, so an `Apps` follows.
    match u.recv() {
        Ok(FromControl::Apps { master, master_muted, apps }) => {
            println!("master {master:.3}{}", if master_muted { " muted" } else { "" });
            for e in apps {
                println!("{} \"{}\" {:?} volume {:.3}{}{} streams {}", e.app_id, e.label, e.role, e.volume, if e.muted { " muted" } else { "" }, if e.spatial { "" } else { " no-surround" }, e.streams);
            }
        }
        Ok(FromControl::Outputs { current, outputs }) => {
            for o in outputs {
                println!("output {} \"{}\" balance {:+.2} {} ch{}{}", o.id, o.name, o.balance, o.channels, if o.spatial { " surround" } else { "" }, if o.id == current { " (current)" } else { "" });
            }
        }
        Ok(m) => die(format!("unexpected {m:?}")),
        Err(e) => die(format!("recv: {e}")),
    }
}

fn clock(sock: &str, rate: u32, secs: f32, offset: f64) {
    let mut c = Client::connect(sock, None).unwrap_or_else(|e| die(format!("connect {sock}: {e}")));
    let params = StreamParams { role: Role::Media, rate, channels: 2, buffer_ms: 100, label: "Clock".into() };
    let s = c.open(params).unwrap_or_else(|e| die(format!("open: {e}")));
    let total = (secs as f64 * rate as f64) as u64;
    let click_at = |k: u64| (rate as f64 * (k as f64 + offset)).round() as u64;
    let mut next_pred = 0u64; // the first click not yet predicted
    let mut pos = 0u64; // frames written
    let mut started = false;
    let chunk = 256u64;
    let mut buf = vec![0.0f32; chunk as usize * 2];
    while pos < total || click_at(next_pred) < total {
        if pos < total {
            for (f, v) in buf.chunks_exact_mut(2).enumerate() {
                let n = pos + f as u64;
                let k = (n as f64 / rate as f64 - offset).round().max(0.0) as u64;
                let x = if click_at(k) == n { 0.9 } else { 0.0 };
                v[0] = x;
                v[1] = x;
            }
            let mut off = 0usize;
            while off < chunk as usize {
                let w = s.write(&buf[off * 2..]) as usize;
                off += w;
                if w == 0 {
                    if !started {
                        c.start(&s).unwrap_or_else(|e| die(format!("start: {e}")));
                        started = true;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    predict(&s, rate, &click_at, &mut next_pred);
                }
            }
            pos += chunk;
        } else {
            std::thread::sleep(Duration::from_millis(2));
            predict(&s, rate, &click_at, &mut next_pred);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
}

/// Print the clock's prediction for each click 30–300 ms before it is heard.
fn predict(s: &lyra_protocol::client::Stream, rate: u32, click_at: &dyn Fn(u64) -> u64, next: &mut u64) {
    let ck = s.clock();
    if ck.state != 1 {
        return;
    }
    let now = lyra::server::monotonic_ns();
    loop {
        let n = click_at(*next);
        let ahead = ck.heard_at(n) - now;
        if ahead > 300_000_000 {
            return;
        }
        if ahead >= 30_000_000 {
            println!("click {rate} {} {n} {} {}", *next, ck.heard_at(n), ck.rate_ppm);
        }
        *next += 1;
    }
}

fn lyrad_raw(sock: &str) {
    let mut s = match std::os::unix::net::UnixStream::connect(sock) {
        Ok(s) => s,
        Err(e) => {
            // The socket's directory keeps strangers out before lyrad does.
            println!("refused ({e})");
            return;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = wire::send(&mut s, &ToEngine::Open { stream: 1, rate: 48_000, channels: 2, ring_frames: 4096 });
    match wire::recv::<_, FromEngine>(&mut s) {
        Ok(m) => println!("ACCEPTED ({m:?})"),
        Err(_) => println!("refused"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let a = |i: usize| args.get(i).cloned().unwrap_or_default();
    match a(1).as_str() {
        "hostile" => hostile(&a(2), a(3).parse().unwrap_or(5.0)),
        "ctl" => ctl(&a(2), &args[3.min(args.len())..]),
        "lyrad-raw" => lyrad_raw(&a(2)),
        "clock" => clock(&a(2), a(3).parse().unwrap_or(48_000), a(4).parse().unwrap_or(600.0), a(5).parse().unwrap_or(0.0)),
        _ => die("usage: lyra-vmtest hostile|ctl|clock|lyrad-raw ...".into()),
    }
}
