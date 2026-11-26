//! `fenestra-view --headless --mint-dir DIR --out DIR [--run-secs N] [--reconnect-after S]`
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
        let views = Arc::new(Views::default());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(run_secs);
        let mut first = true;
        loop {
            let hc = viewer.connect(m.addr, &m.secret).await.unwrap_or_else(|e| die(e));
            eprintln!("fenestra-view: connected to {}", m.addr);
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
        let report = snapshot(&views, &out).unwrap_or_else(|e| die(e));
        print!("{report}");
        viewer.close();
    });
}
