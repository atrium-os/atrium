//! `fenestra-host-sim --rec FILE --mint-dir DIR --out DIR [--speed S] [--idle SECS]`
//!
//! A Fenestra host for gates on any machine (fenestra.md §14, R2): it
//! plays a frescod session recording (`fresco_render_core::record`) into a
//! frontend, as frescod's would have taken it, and serves the windows over
//! QUIC to viewers that prove the session. The mint goes through
//! `--mint-dir` (`fenestra_wire::mint`): the viewer leaves its public key
//! there, the host answers with the address, its key and the secret.
//!
//! Playback starts once the first viewer is in, paced by the recording's
//! ticks (`--speed` 1 is real time, capped at 1 s a gap). Then the host
//! stays up `--idle` seconds and counts what it sent meanwhile (an idle
//! screen sends nothing, §5.1), writes `DIR/host.txt` and exits:
//!
//!   window ID WxH SHA256        (each window as the host draws it)
//!   connection N frames F wholes W bytes B acks A resumed R resent S
//!   idle_bytes B

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fenestra_session::{serve, HostScene, ViewerStats};
use fenestra_wire::{mint, Host, SessionKey};
use fresco_render_core::record::{self, Event};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-host-sim: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rec = arg(&args, "--rec").unwrap_or_else(|| die("--rec FILE is needed"));
    let dir = PathBuf::from(arg(&args, "--mint-dir").unwrap_or_else(|| die("--mint-dir DIR is needed")));
    let out = PathBuf::from(arg(&args, "--out").unwrap_or_else(|| die("--out DIR is needed")));
    let speed: f64 = arg(&args, "--speed").and_then(|s| s.parse().ok()).unwrap_or(1.0f64).clamp(0.01, 1000.0);
    let idle: u64 = arg(&args, "--idle").and_then(|s| s.parse().ok()).unwrap_or(5).min(600);
    let bind = arg(&args, "--bind").unwrap_or_else(|| "127.0.0.1:0".into());
    let events = std::fs::File::open(&rec).and_then(record::read).unwrap_or_else(|e| die(format!("{rec}: {e}")));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    rt.block_on(run(events, dir, out, speed, idle, bind));
}

async fn run(events: Vec<Event>, dir: PathBuf, out: PathBuf, speed: f64, idle: u64, bind: String) {
    // The viewer's key, pinned (it would come in the mint request).
    let mut waited = 0;
    let viewer_key = loop {
        match mint::read_viewer_key(&dir) {
            Ok(k) => break k,
            Err(_) if waited < 600 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += 1;
            }
            Err(e) => die(format!("no viewer key in {}: {e}", dir.display())),
        }
    };
    let key = SessionKey::generate().unwrap_or_else(|e| die(e));
    let secret = mint::fresh_secret().unwrap_or_else(|e| die(e));
    let host = Host::bind(bind.parse().unwrap_or_else(|_| die("--bind address")), &key, &viewer_key, secret.clone()).unwrap_or_else(|e| die(e));
    let addr = host.local_addr().unwrap_or_else(|e| die(e));
    mint::DevMint { addr, host_key: key.public().to_vec(), secret }.write(&dir).unwrap_or_else(|e| die(e));
    eprintln!("fenestra-host-sim: serving on {addr}");

    // Epochs only have to differ between runs of the host.
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let scene = Arc::new(Mutex::new(HostScene::new(record::frontend(), epoch)));
    let (wake_tx, wake_rx) = tokio::sync::watch::channel(0u64);
    let conns: Arc<Mutex<Vec<Arc<ViewerStats>>>> = Arc::default();
    let (first_tx, mut first_rx) = tokio::sync::watch::channel(false);
    let host = Arc::new(host);
    {
        let (host, scene, conns) = (host.clone(), scene.clone(), conns.clone());
        tokio::spawn(async move {
            loop {
                match host.accept().await {
                    Ok(Some(hc)) => {
                        let stats = Arc::new(ViewerStats::default());
                        conns.lock().unwrap_or_else(|e| e.into_inner()).push(stats.clone());
                        let _ = first_tx.send(true);
                        let (scene, wake) = (scene.clone(), wake_rx.clone());
                        tokio::spawn(async move {
                            if let Err(e) = serve(hc, scene, wake, stats).await {
                                eprintln!("fenestra-host-sim: viewer: {e}");
                            }
                        });
                    }
                    Ok(None) => return,
                    Err(e) => eprintln!("fenestra-host-sim: refused a connection: {e}"),
                }
            }
        });
    }
    if tokio::time::timeout(Duration::from_secs(120), first_rx.wait_for(|v| *v)).await.is_err() {
        die("no viewer came in 120 s");
    }

    let mut last_tick: Option<u64> = None;
    for e in &events {
        match e {
            Event::Msg { client, msg } => {
                let _ = scene.lock().unwrap_or_else(|e| e.into_inner()).fe.dispatch(msg, *client);
            }
            Event::Blob(b) => scene.lock().unwrap_or_else(|e| e.into_inner()).fe.ingest_blob(b),
            Event::Tick(t) => {
                if let Some(l) = last_tick {
                    let gap = Duration::from_nanos(((t.saturating_sub(l)) as f64 / speed) as u64).min(Duration::from_secs(1));
                    tokio::time::sleep(gap).await;
                }
                last_tick = Some(*t);
                scene.lock().unwrap_or_else(|e| e.into_inner()).fe.tick_animations(*t);
            }
            Event::Draw => {
                let mut s = scene.lock().unwrap_or_else(|e| e.into_inner());
                if s.draw() {
                    let g = s.generation;
                    drop(s);
                    let _ = wake_tx.send(g);
                }
            }
        }
    }
    // Let the last frames go, then measure an idle screen.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let sent = |conns: &Mutex<Vec<Arc<ViewerStats>>>| -> u64 { conns.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|c| c.bytes.load(Ordering::Relaxed)).sum() };
    let before = sent(&conns);
    tokio::time::sleep(Duration::from_secs(idle)).await;
    let idle_bytes = sent(&conns) - before;

    let mut report = String::new();
    {
        let s = scene.lock().unwrap_or_else(|e| e.into_inner());
        for (win, _) in s.windows() {
            if let Some((w, h, _, sha)) = s.raster(win) {
                report += &format!("window {win} {w}x{h} {sha}\n");
            }
        }
    }
    for (i, c) in conns.lock().unwrap_or_else(|e| e.into_inner()).iter().enumerate() {
        report += &format!(
            "connection {i} frames {} wholes {} bytes {} acks {} resumed {} resent {}\n",
            c.frames.load(Ordering::Relaxed),
            c.wholes.load(Ordering::Relaxed),
            c.bytes.load(Ordering::Relaxed),
            c.acks.load(Ordering::Relaxed),
            c.resumed.load(Ordering::Relaxed),
            c.resent.load(Ordering::Relaxed)
        );
    }
    report += &format!("idle_bytes {idle_bytes}\n");
    if let Err(e) = std::fs::create_dir_all(&out).and_then(|_| std::fs::write(out.join("host.txt"), &report)) {
        die(format!("{}: {e}", out.display()));
    }
    print!("{report}");
    host.close_and_wait().await;
}
