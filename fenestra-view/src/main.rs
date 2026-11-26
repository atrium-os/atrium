//! `fenestra-view --headless --mint-dir DIR --out DIR [--run-secs N] [--reconnect-after S] [--connect ADDR] [--idle-after S]`
//!
//! Headless (gates, §14.2): connect with the mint in `--mint-dir`
//! (`fenestra_wire::mint`; it leaves its public key there first), draw the
//! host's windows for `--run-secs`, then write `window-ID.png` and
//! `viewer.txt` to `--out`. `--reconnect-after S` drops the connection
//! after S seconds and connects again, resuming from what it holds.
//! A windowed mode (winit) comes next.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fenestra_view::{run, snapshot, Views};
use fenestra_wire::{mint, SessionKey, Viewer};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-view: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--headless") {
        die("only --headless exists yet");
    }
    let dir = PathBuf::from(arg(&args, "--mint-dir").unwrap_or_else(|| die("--mint-dir DIR is needed")));
    let out = PathBuf::from(arg(&args, "--out").unwrap_or_else(|| die("--out DIR is needed")));
    let run_secs: u64 = arg(&args, "--run-secs").and_then(|s| s.parse().ok()).unwrap_or(20).min(3600);
    let reconnect: Option<u64> = arg(&args, "--reconnect-after").and_then(|s| s.parse().ok());
    // The host's address as this side reaches it (a forwarded port), in
    // place of the one in the mint.
    // Bytes received after this many seconds count as idle (`idle_bytes`).
    let idle_after: Option<u64> = arg(&args, "--idle-after").and_then(|s| s.parse().ok());
    let connect: Option<std::net::SocketAddr> = arg(&args, "--connect").map(|a| a.parse().unwrap_or_else(|_| die("--connect ADDR:PORT")));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    rt.block_on(async move {
        let key = SessionKey::generate().unwrap_or_else(|e| die(e));
        mint::write_viewer_key(&dir, key.public()).unwrap_or_else(|e| die(format!("{}: {e}", dir.display())));
        let mut tries = 0;
        let m = loop {
            match mint::DevMint::read(&dir) {
                Ok(m) => break m,
                Err(_) if tries < 600 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => die(format!("no mint: {e}")),
            }
        };
        let viewer = Viewer::new(&key, &m.host_key).unwrap_or_else(|e| die(e));
        let addr = connect.unwrap_or(m.addr);
        let views = Arc::new(Views::default());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(run_secs);
        let mark = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        if let Some(s) = idle_after {
            let (views, mark) = (views.clone(), mark.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(s)).await;
                mark.store(views.bytes.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
            });
        }
        let mut first = true;
        loop {
            let hc = viewer.connect(addr, &m.secret).await.unwrap_or_else(|e| die(e));
            eprintln!("fenestra-view: connected to {addr}");
            let cut = match (first, reconnect) {
                (true, Some(s)) => tokio::time::Instant::now() + Duration::from_secs(s),
                _ => deadline,
            };
            first = false;
            let conn = hc.conn.clone();
            tokio::select! {
                r = run(hc, views.clone()) => {
                    if let Err(e) = r {
                        die(e);
                    }
                    break;
                }
                _ = tokio::time::sleep_until(cut) => {
                    if cut >= deadline {
                        break;
                    }
                    eprintln!("fenestra-view: dropping the connection to reconnect");
                    conn.close(0u32.into(), b"reconnect");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        let mut report = snapshot(&views, &out).unwrap_or_else(|e| die(e));
        let at = mark.load(std::sync::atomic::Ordering::Relaxed);
        if at != u64::MAX {
            let line = format!("idle_bytes {}\n", views.bytes.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(at));
            report += &line;
            let _ = std::fs::OpenOptions::new().append(true).open(out.join("viewer.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        }
        print!("{report}");
        viewer.close();
    });
}
